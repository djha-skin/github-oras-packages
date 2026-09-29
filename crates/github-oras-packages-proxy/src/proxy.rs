//! OCI Distribution and human-readable autoindex request handling.
//!
//! All upstream requests are scoped to the configured repository. OCI writes
//! stream through a narrow header allowlist; autoindex reads project standard
//! manifest descriptors without a custom route-map namespace.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{HeaderMap, Method, Request, Response, StatusCode, body::Incoming, header};
use sha2::{Digest, Sha256};

use crate::{
    autoindex::{PathError, parse_request_path, render_directory_html},
    errors::{ProxyError, UpstreamFailure, UpstreamResource, map_error},
    inbound::{
        InboundLimits, validate_autoindex_request, validate_distribution_request, validate_request,
    },
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
    let target = match validate_distribution_request(&request, limits) {
        Ok(_) => match parse_path(request.uri().path(), repository.as_str()) {
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
    if !valid_distribution_query(
        &target,
        request.method(),
        request.uri().query(),
        &repository,
    ) {
        return into_service_response(map_error(ProxyError::Inbound(
            crate::inbound::InboundError::Route(crate::routing::RouteError::InvalidRoute),
        )));
    }
    let authorization = match inbound_authorization(&request) {
        Ok(authorization) => authorization,
        Err(error) => return into_service_response(map_error(error)),
    };
    let cache_control = if authorization.is_some() || client.has_token_broker() {
        "private, no-store, no-transform"
    } else {
        "public, max-age=0, must-revalidate, no-transform"
    };
    match target {
        DistributionTarget::Version if matches!(*request.method(), Method::GET | Method::HEAD) => {
            Response::builder()
                .status(StatusCode::OK)
                .header("docker-distribution-api-version", "registry/2.0")
                .header(header::CONTENT_LENGTH, 0)
                .body(crate::server::fixed_body(Bytes::new()))
                .expect("OCI version response headers are valid")
        }
        DistributionTarget::Manifest { reference, .. } if request.method() == Method::GET => {
            let bytes = match client
                .manifest(&repository, &reference, authorization.as_ref())
                .await
            {
                Ok(bytes) => bytes,
                Err(error) => return map_oci_error(error, true),
            };
            let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
            Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    "application/vnd.oci.image.manifest.v1+json",
                )
                .header(header::CONTENT_LENGTH, bytes.len())
                .header("docker-content-digest", &digest)
                .header(header::ETAG, format!("\"{digest}\""))
                .header(header::CACHE_CONTROL, cache_control)
                .body(crate::server::fixed_body(bytes))
                .expect("OCI manifest response headers are valid")
        }
        DistributionTarget::Manifest { reference, .. } if request.method() == Method::HEAD => {
            let headers = match client
                .manifest_head(&repository, &reference, authorization.as_ref())
                .await
            {
                Ok(headers) => headers,
                Err(error) => return map_oci_error(error, true),
            };
            distribution_head_response(
                headers,
                "application/vnd.oci.image.manifest.v1+json",
                cache_control,
                None,
            )
        }
        DistributionTarget::Blob { digest, .. } if request.method() == Method::GET => {
            let (body, size, media_type) = match client
                .blob_by_digest(&repository, &digest, authorization.as_ref())
                .await
            {
                Ok(result) => result,
                Err(error) => return map_oci_error(error, true),
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, media_type)
                .header(header::CONTENT_LENGTH, size)
                .header("docker-content-digest", &digest)
                .header(header::CACHE_CONTROL, cache_control)
                .body(body)
                .expect("OCI blob response headers are valid")
        }
        DistributionTarget::Blob { digest, .. } if request.method() == Method::HEAD => {
            let headers = match client
                .blob_head(&repository, &digest, authorization.as_ref())
                .await
            {
                Ok(headers) => headers,
                Err(error) => return map_oci_error(error, true),
            };
            distribution_head_response(
                headers,
                "application/octet-stream",
                cache_control,
                Some(&digest),
            )
        }
        DistributionTarget::Manifest { reference, .. } if request.method() == Method::PUT => {
            forward_distribution_write(
                request,
                client,
                repository,
                format!("/manifests/{reference}"),
                authorization,
            )
            .await
        }
        DistributionTarget::Manifest { reference, .. } if request.method() == Method::DELETE => {
            forward_distribution_write(
                request,
                client,
                repository,
                format!("/manifests/{reference}"),
                authorization,
            )
            .await
        }
        DistributionTarget::Blob { digest, .. } if request.method() == Method::DELETE => {
            forward_distribution_write(
                request,
                client,
                repository,
                format!("/blobs/{digest}"),
                authorization,
            )
            .await
        }
        DistributionTarget::BlobUploadStart { .. } if request.method() == Method::POST => {
            forward_distribution_write(
                request,
                client,
                repository,
                "/blobs/uploads/".to_owned(),
                authorization,
            )
            .await
        }
        DistributionTarget::BlobUpload { upload_id, .. }
            if matches!(
                *request.method(),
                Method::PATCH | Method::PUT | Method::DELETE
            ) =>
        {
            forward_distribution_write(
                request,
                client,
                repository,
                format!("/blobs/uploads/{upload_id}"),
                authorization,
            )
            .await
        }
        target => distribution_method_not_allowed(&target),
    }
}

fn distribution_head_response(
    upstream_headers: HeaderMap,
    fallback_media_type: &str,
    cache_control: &str,
    fallback_digest: Option<&str>,
) -> ServiceResponse {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CACHE_CONTROL, cache_control);
    for name in [
        header::CONTENT_LENGTH,
        header::ETAG,
        header::HeaderName::from_static("docker-content-digest"),
    ] {
        if let Some(value) = upstream_headers.get(&name) {
            response = response.header(name, value);
        }
    }
    response = response.header(
        header::CONTENT_TYPE,
        upstream_headers
            .get(header::CONTENT_TYPE)
            .map_or(fallback_media_type, |value| {
                value.to_str().unwrap_or(fallback_media_type)
            }),
    );
    if upstream_headers.get("docker-content-digest").is_none() {
        if let Some(digest) = fallback_digest {
            response = response.header("docker-content-digest", digest);
        }
    }
    response
        .body(crate::server::fixed_body(Bytes::new()))
        .expect("validated OCI HEAD response headers are valid")
}

fn distribution_method_not_allowed(target: &DistributionTarget) -> ServiceResponse {
    let allow = match target {
        DistributionTarget::Version => "GET, HEAD",
        DistributionTarget::Manifest { .. } => "GET, HEAD, PUT, DELETE",
        DistributionTarget::Blob { .. } => "GET, HEAD, DELETE",
        DistributionTarget::BlobUploadStart { .. } => "POST",
        DistributionTarget::BlobUpload { .. } => "PATCH, PUT, DELETE",
    };
    let mut response = map_error(ProxyError::Inbound(crate::inbound::InboundError::Route(
        crate::routing::RouteError::UnsupportedMethod,
    )));
    response.headers_mut().insert(
        header::ALLOW,
        hyper::header::HeaderValue::from_static(allow),
    );
    into_service_response(response)
}

fn valid_distribution_query(
    target: &DistributionTarget,
    method: &Method,
    query: Option<&str>,
    repository: &ValidatedRepository,
) -> bool {
    let Some(query) = query else {
        return true;
    };
    if query.is_empty()
        || query
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b'#')
    {
        return false;
    }
    match target {
        DistributionTarget::BlobUploadStart { .. } if method == Method::POST => {
            let mut mount = None;
            let mut from = None;
            for part in query.split('&') {
                let Some((key, value)) = part.split_once('=') else {
                    return false;
                };
                match key {
                    "mount" if mount.is_none() && valid_sha256_digest(value) => {
                        mount = Some(value);
                    }
                    "from" if from.is_none() && value == repository.as_str() => {
                        from = Some(value);
                    }
                    _ => return false,
                }
            }
            mount.is_some() && from.is_some()
        }
        DistributionTarget::BlobUpload { .. }
            if matches!(*method, Method::PATCH | Method::PUT | Method::DELETE) =>
        {
            let mut state_seen = false;
            let mut digest_seen = false;
            for part in query.split('&') {
                let Some((key, value)) = part.split_once('=') else {
                    return false;
                };
                match key {
                    "_state" if !state_seen && !value.is_empty() => state_seen = true,
                    "digest" if !digest_seen && valid_sha256_digest(value) => digest_seen = true,
                    _ => return false,
                }
            }
            state_seen || digest_seen
        }
        _ => false,
    }
}

fn valid_sha256_digest(value: &str) -> bool {
    let hex = value
        .strip_prefix("sha256:")
        .or_else(|| value.strip_prefix("sha256%3A"))
        .or_else(|| value.strip_prefix("sha256%3a"));
    hex.is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

async fn forward_distribution_write(
    request: Request<Incoming>,
    client: Arc<OciClient>,
    repository: ValidatedRepository,
    suffix: String,
    authorization: Option<hyper::header::HeaderValue>,
) -> ServiceResponse {
    let upstream = match client
        .forward_distribution(&suffix, request, &repository, authorization.as_ref())
        .await
    {
        Ok(response) => response,
        Err(error) => return map_oci_error(error, true),
    };
    let status = upstream.status();
    let mut builder = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store");
    for name in [
        header::CONTENT_LENGTH,
        header::CONTENT_TYPE,
        header::LOCATION,
        header::ETAG,
        header::RETRY_AFTER,
        header::WWW_AUTHENTICATE,
        header::HeaderName::from_static("docker-content-digest"),
        header::HeaderName::from_static("docker-upload-uuid"),
        header::HeaderName::from_static("docker-distribution-api-version"),
        header::HeaderName::from_static("range"),
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            builder = builder.header(name, value);
        }
    }
    let body = upstream
        .into_body()
        .map_err(|error| Box::new(error) as crate::server::BoxError)
        .boxed_unsync();
    builder
        .body(body)
        .expect("validated OCI response headers are valid")
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
    let authorization = match inbound_authorization(&request) {
        Ok(authorization) => authorization,
        Err(error) => return into_service_response(map_error(error)),
    };
    let authenticated = authorization.is_some() || client.has_token_broker();
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
            authenticated,
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
            if authenticated {
                "private, no-store, no-transform"
            } else {
                "public, max-age=0, must-revalidate, no-transform"
            },
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

    let authorization = match inbound_authorization(&request) {
        Ok(authorization) => authorization,
        Err(error) => return into_service_response(map_error(error)),
    };
    let authenticated = authorization.is_some() || client.has_token_broker();
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
        authenticated,
        request.headers().get(header::IF_NONE_MATCH),
        &descriptor,
        body,
    )
}

fn inbound_authorization(
    request: &Request<Incoming>,
) -> Result<Option<hyper::header::HeaderValue>, ProxyError> {
    let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
    let authorization = values.next().cloned();
    if values.next().is_some() {
        return Err(ProxyError::Inbound(
            crate::inbound::InboundError::InvalidHeader,
        ));
    }
    Ok(authorization)
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
