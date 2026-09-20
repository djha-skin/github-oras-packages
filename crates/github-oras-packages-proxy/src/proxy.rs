//! PyPI MVP request handling over the fixed OCI layout.
//!
//! This is deliberately a narrow adapter: it validates the public route,
//! performs an exact route-map lookup, and returns the prebuilt bytes selected
//! by the descriptor. It does not render or rewrite Simple API HTML.

use std::sync::Arc;

use bytes::Bytes;
use hyper::{Method, Request, Response, StatusCode, body::Incoming, header};

use crate::{
    autoindex::{PathError, parse_request_path, render_directory_html},
    errors::{ProxyError, UpstreamFailure, UpstreamResource, map_error},
    inbound::{InboundLimits, validate_autoindex_request, validate_request},
    oci::{OciClient, OciError},
    oci_gateway::{DistributionTarget, parse_path},
    routing::{EnabledProtocols, Protocol, ValidatedRepository},
    server::{ServiceResponse, into_service_response},
};

/// Dispatches standard OCI paths and human-readable autoindex paths.
pub async fn handle_gateway(
    request: Request<Incoming>,
    client: Arc<OciClient>,
    repository: ValidatedRepository,
    limits: InboundLimits,
) -> ServiceResponse {
    if request.uri().path() == "/v2/" || request.uri().path().starts_with("/v2/") {
        return handle_distribution(request, client, repository, limits).await;
    }
    handle_autoindex(request, client, repository, limits).await
}

async fn handle_distribution(
    request: Request<Incoming>,
    client: Arc<OciClient>,
    repository: ValidatedRepository,
    limits: InboundLimits,
) -> ServiceResponse {
    let target = match crate::inbound::validate_autoindex_request(&request, limits) {
        Ok(raw_target) => match parse_path(raw_target, repository.as_str()) {
            Ok(target) => target,
            Err(_) => {
                return into_service_response(map_error(ProxyError::Upstream(
                    UpstreamFailure::NotFound {
                        resource: UpstreamResource::Layout,
                    },
                )));
            }
        },
        Err(error) => return into_service_response(map_error(error.into())),
    };
    let authorization = request.headers().get(header::AUTHORIZATION).cloned();
    match target {
        DistributionTarget::Version => Response::builder()
            .status(StatusCode::OK)
            .header("docker-distribution-api-version", "registry/2.0")
            .header(header::CONTENT_LENGTH, 0)
            .body(crate::server::fixed_body(Bytes::new()))
            .expect("OCI version response headers are valid"),
        DistributionTarget::Manifest { reference, .. } => {
            let bytes = match client
                .manifest(&repository, &reference, authorization.as_ref())
                .await
            {
                Ok(bytes) => bytes,
                Err(error) => return map_oci_error(error, true),
            };
            let body = if request.method() == Method::HEAD {
                Bytes::new()
            } else {
                bytes.clone()
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    "application/vnd.oci.image.manifest.v1+json",
                )
                .header(header::CONTENT_LENGTH, bytes.len())
                .body(crate::server::fixed_body(body))
                .expect("OCI manifest response headers are valid")
        }
        DistributionTarget::Blob { digest, .. } => {
            let (body, size, media_type) = match client
                .blob_by_digest(&repository, &digest, authorization.as_ref())
                .await
            {
                Ok(result) => result,
                Err(error) => return map_oci_error(error, false),
            };
            let body = if request.method() == Method::HEAD {
                crate::server::fixed_body(Bytes::new())
            } else {
                body
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, media_type)
                .header(header::CONTENT_LENGTH, size)
                .body(body)
                .expect("OCI blob response headers are valid")
        }
    }
}

/// Handles a human-readable autoindex request using the configured OCI repository.
pub async fn handle_autoindex(
    request: Request<Incoming>,
    client: Arc<OciClient>,
    repository: ValidatedRepository,
    limits: InboundLimits,
) -> ServiceResponse {
    let raw_target = match validate_autoindex_request(&request, limits) {
        Ok(target) => target,
        Err(error) => return into_service_response(map_error(error.into())),
    };
    let path = match parse_request_path(raw_target) {
        Ok(path) => path,
        Err(PathError::Invalid | PathError::DirectoryNeedsTrailingSlash) => {
            return into_service_response(map_error(ProxyError::Inbound(
                crate::inbound::InboundError::Route(crate::routing::RouteError::InvalidRoute),
            )));
        }
        Err(_) => return into_service_response(map_error(ProxyError::Unexpected)),
    };
    let authorization = request.headers().get(header::AUTHORIZATION).cloned();
    let snapshot = match client
        .autoindex_snapshot(&repository, authorization.as_ref())
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => return map_oci_error(error, true),
    };

    if let Some(object) = snapshot.object(&path) {
        let descriptor = crate::oci::Descriptor::with_annotations(
            object.media_type(),
            object.digest(),
            object.size(),
            std::collections::BTreeMap::new(),
        );
        let body = match client
            .blob(&repository, &descriptor, authorization.as_ref())
            .await
        {
            Ok(body) => body,
            Err(error) => return map_oci_error(error, false),
        };
        return representation(
            request.method(),
            request.headers().contains_key(header::AUTHORIZATION),
            request.headers().get(header::IF_NONE_MATCH),
            &descriptor,
            body,
        );
    }

    let directory = if path.is_empty() || path.ends_with('/') {
        path
    } else {
        let candidate = format!("{path}/");
        if snapshot.has_directory(&candidate).unwrap_or(false) {
            return redirect_to_directory(&candidate);
        }
        return into_service_response(map_error(ProxyError::Upstream(UpstreamFailure::NotFound {
            resource: UpstreamResource::Layout,
        })));
    };
    let entries = match snapshot.children(&directory) {
        Ok(entries) => entries,
        Err(_) => {
            return into_service_response(map_error(ProxyError::Upstream(
                UpstreamFailure::NotFound {
                    resource: UpstreamResource::Layout,
                },
            )));
        }
    };
    let html = match render_directory_html(&directory, &entries) {
        Ok(html) => html,
        Err(_) => return into_service_response(map_error(ProxyError::Unexpected)),
    };
    let body = Bytes::from(html);
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CACHE_CONTROL,
            "public, max-age=0, must-revalidate, no-transform",
        )
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(crate::server::fixed_body(
            if request.method() == Method::HEAD {
                Bytes::new()
            } else {
                body
            },
        ))
        .expect("autoindex headers are valid")
}

fn redirect_to_directory(path: &str) -> ServiceResponse {
    Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header(header::LOCATION, format!("/{path}"))
        .header(header::CACHE_CONTROL, "public, max-age=0, must-revalidate")
        .body(crate::server::fixed_body(Bytes::new()))
        .expect("validated directory redirect is valid")
}

/// Handles a validated PyPI v1 request using the fixed OCI client.
pub async fn handle(
    request: Request<Incoming>,
    client: Arc<OciClient>,
    enabled_protocols: EnabledProtocols,
    limits: InboundLimits,
) -> ServiceResponse {
    let route = match validate_request(&request, enabled_protocols, limits) {
        Ok(route) => route,
        Err(error) => return into_service_response(map_error(error.into())),
    };
    if route.protocol() != Protocol::Pypi {
        return into_service_response(map_error(ProxyError::Unexpected));
    }

    let authorization = request.headers().get(header::AUTHORIZATION).cloned();
    let snapshot = match client
        .snapshot(route.repository(), authorization.as_ref())
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => return map_oci_error(error, true),
    };
    let Some(descriptor) = snapshot.descriptor("pypi", route.protocol_path().as_str()) else {
        return into_service_response(map_error(ProxyError::Upstream(UpstreamFailure::NotFound {
            resource: UpstreamResource::Layout,
        })));
    };
    let descriptor = descriptor.clone();
    let body = match client
        .blob(route.repository(), &descriptor, authorization.as_ref())
        .await
    {
        Ok(body) => body,
        Err(error) => return map_oci_error(error, false),
    };
    representation(
        request.method(),
        request.headers().contains_key(header::AUTHORIZATION),
        request.headers().get(header::IF_NONE_MATCH),
        &descriptor,
        body,
    )
}

fn representation(
    method: &Method,
    authenticated: bool,
    if_none_match: Option<&header::HeaderValue>,
    descriptor: &crate::oci::Descriptor,
    body: crate::oci::BlobBody,
) -> ServiceResponse {
    let etag = format!("\"{}\"", descriptor.digest());
    let cache_control = if authenticated {
        "private, no-store, no-transform"
    } else {
        "public, max-age=0, must-revalidate, no-transform"
    };
    if if_none_match.is_some_and(|value| value.as_bytes() == etag.as_bytes()) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::CONTENT_TYPE, descriptor.media_type())
            .header(header::ETAG, etag)
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .body(crate::server::fixed_body(Bytes::new()))
            .expect("descriptor-controlled response headers are valid");
    }
    let body = if method == Method::HEAD {
        crate::server::fixed_body(Bytes::new())
    } else {
        body
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::CONTENT_TYPE, descriptor.media_type())
        .header(header::CONTENT_LENGTH, descriptor.size())
        .header(header::ETAG, etag)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(body)
        .expect("descriptor-controlled response headers are valid")
}

fn map_oci_error(error: OciError, layout_operation: bool) -> ServiceResponse {
    let error = match error {
        OciError::NotFound => ProxyError::Upstream(UpstreamFailure::NotFound {
            resource: if layout_operation {
                UpstreamResource::Layout
            } else {
                UpstreamResource::MappedContent
            },
        }),
        OciError::Unauthorized { challenges } => {
            ProxyError::Upstream(UpstreamFailure::Unauthorized { challenges })
        }
        OciError::Forbidden => ProxyError::Upstream(UpstreamFailure::Forbidden),
        OciError::RateLimited { retry_after } => {
            ProxyError::Upstream(UpstreamFailure::RateLimited { retry_after })
        }
        OciError::Server {
            temporary,
            retry_after,
        } => ProxyError::Upstream(UpstreamFailure::Server {
            temporary,
            retry_after,
        }),
        OciError::Timeout => ProxyError::Timeout,
        OciError::InvalidLayout | OciError::InvalidDescriptor => {
            ProxyError::Upstream(UpstreamFailure::Malformed)
        }
        OciError::Configuration | OciError::Transport | OciError::Upstream => {
            ProxyError::Upstream(UpstreamFailure::Server {
                temporary: false,
                retry_after: None,
            })
        }
    };
    into_service_response(map_error(error))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use hyper::{Method, body::Incoming};

    use super::representation;
    use crate::oci::Descriptor;

    #[test]
    fn descriptor_response_preserves_type_length_and_digest_etag() {
        let descriptor = Descriptor::new(
            "text/html; charset=utf-8",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            4,
        );
        let response = representation(
            &Method::GET,
            false,
            None,
            &descriptor,
            crate::server::fixed_body(Bytes::from_static(b"test")),
        );
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["content-type"],
            "text/html; charset=utf-8"
        );
        assert_eq!(response.headers()["content-length"], "4");
        assert_eq!(
            response.headers()["etag"],
            "\"sha256:0000000000000000000000000000000000000000000000000000000000000000\""
        );
    }

    // Keep the response body type visible to this module's API review without
    // introducing a second body abstraction.
    fn _body_type(_: Incoming) {}
}
