//! PyPI MVP request handling over the fixed OCI layout.
//!
//! This is deliberately a narrow adapter: it validates the public route,
//! performs an exact route-map lookup, and returns the prebuilt bytes selected
//! by the descriptor. It does not render or rewrite Simple API HTML.

use std::sync::Arc;

use bytes::Bytes;
use hyper::{Method, Request, Response, StatusCode, body::Incoming, header};

use crate::{
    errors::{ProxyError, UpstreamFailure, UpstreamResource, map_error},
    inbound::{InboundLimits, validate_request},
    oci::{OciClient, OciError},
    routing::{EnabledProtocols, Protocol},
    server::{ServiceResponse, into_service_response},
};

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
