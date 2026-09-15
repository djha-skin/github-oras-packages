//! Credential-safe error classification and response construction.
//!
//! The proxy never sends a formatted request, response, URL, header map, or
//! third-party error to a client.  Callers translate failures into the typed
//! [`ProxyError`] variants below and this module emits only a fixed response
//! shape.  The two deliberate upstream-header exceptions are
//! `WWW-Authenticate` on a 401 and a validated `Retry-After` on a 503.

use std::fmt;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    Response, StatusCode,
    header::{self, HeaderValue},
};

use crate::inbound::InboundError;
use crate::routing::RouteError;

/// The response body type emitted by [`map_error`].
pub type ErrorBody = Full<Bytes>;
/// A complete safe error response ready for the HTTP server boundary.
pub type ErrorResponse = Response<ErrorBody>;

const ERROR_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const ERROR_TITLE: &str = "Request could not be completed.";

/// The upstream object whose absence was observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamResource {
    /// The fixed layout tag or its manifest was not published.
    Layout,
    /// A descriptor selected by a validated layout was not available.
    MappedContent,
}

/// A typed failure returned by the fixed OCI upstream boundary.
///
/// This type deliberately contains no URL, repository, status text, response
/// body, or authorization value.  The two header vectors are the only values
/// retained from an upstream response, and they are copied directly to the
/// client only in the narrowly specified cases below.  Its [`Debug`]
/// implementation reports only classifications and counts.
#[derive(Clone)]
pub enum UpstreamFailure {
    /// GHCR requires credentials; challenges may be relayed unchanged.
    Unauthorized {
        /// The validated upstream challenge fields, in received order.
        challenges: Vec<HeaderValue>,
    },
    /// GHCR refused access to an otherwise valid request.
    Forbidden,
    /// An OCI object was absent.
    NotFound { resource: UpstreamResource },
    /// GHCR imposed a request limit.
    RateLimited {
        /// A candidate `Retry-After`; invalid values are omitted by the mapper.
        retry_after: Option<HeaderValue>,
    },
    /// A recognized upstream server failure.
    Server {
        /// Temporary failures map to 503; other failures map to 502.
        temporary: bool,
        /// A candidate `Retry-After`; invalid values are omitted by the mapper.
        retry_after: Option<HeaderValue>,
    },
    /// The upstream response violated the expected OCI/HTTP contract.
    Malformed,
}

impl fmt::Debug for UpstreamFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized { challenges } => formatter
                .debug_struct("Unauthorized")
                .field("challenge_count", &challenges.len())
                .finish(),
            Self::Forbidden => formatter.write_str("Forbidden"),
            Self::NotFound { resource } => formatter
                .debug_struct("NotFound")
                .field("resource", resource)
                .finish(),
            Self::RateLimited { retry_after } => formatter
                .debug_struct("RateLimited")
                .field("retry_after_present", &retry_after.is_some())
                .finish(),
            Self::Server {
                temporary,
                retry_after,
            } => formatter
                .debug_struct("Server")
                .field("temporary", temporary)
                .field("retry_after_present", &retry_after.is_some())
                .finish(),
            Self::Malformed => formatter.write_str("Malformed"),
        }
    }
}

/// A typed error that may cross the proxy's internal response boundary.
///
/// No variant stores caller input or an arbitrary source error.  This makes it
/// safe to use in a future request task, log classification, or response
/// mapper without accidentally formatting a credential or upstream diagnostic.
#[derive(Clone)]
pub enum ProxyError {
    /// The request failed local admission or route validation.
    Inbound(InboundError),
    /// The fixed OCI origin returned a classified failure.
    Upstream(UpstreamFailure),
    /// The fixed-origin operation exceeded its deadline.
    Timeout,
    /// The request task was cancelled before a response was complete.
    Cancelled,
    /// An internal operation failed without a safe public classification.
    Unexpected,
}

impl fmt::Debug for ProxyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inbound(error) => formatter
                .debug_tuple("Inbound")
                .field(&error.code())
                .finish(),
            Self::Upstream(error) => formatter.debug_tuple("Upstream").field(error).finish(),
            Self::Timeout => formatter.write_str("Timeout"),
            Self::Cancelled => formatter.write_str("Cancelled"),
            Self::Unexpected => formatter.write_str("Unexpected"),
        }
    }
}

impl fmt::Display for ProxyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for ProxyError {}

impl From<InboundError> for ProxyError {
    fn from(error: InboundError) -> Self {
        Self::Inbound(error)
    }
}

impl From<RouteError> for ProxyError {
    fn from(error: RouteError) -> Self {
        Self::Inbound(InboundError::Route(error))
    }
}

impl ProxyError {
    /// Returns the stable internal classification, never caller input.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Inbound(error) => error.code(),
            Self::Upstream(UpstreamFailure::Unauthorized { .. }) => "upstream_unauthorized",
            Self::Upstream(UpstreamFailure::Forbidden) => "upstream_forbidden",
            Self::Upstream(UpstreamFailure::NotFound { .. }) => "upstream_not_found",
            Self::Upstream(UpstreamFailure::RateLimited { .. }) => "upstream_rate_limited",
            Self::Upstream(UpstreamFailure::Server {
                temporary: true, ..
            }) => "upstream_unavailable",
            Self::Upstream(UpstreamFailure::Server {
                temporary: false, ..
            }) => "upstream_failure",
            Self::Upstream(UpstreamFailure::Malformed) => "upstream_invalid_response",
            Self::Timeout => "upstream_timeout",
            Self::Cancelled => "request_cancelled",
            Self::Unexpected => "internal_error",
        }
    }
}

/// Maps a typed internal failure to a fixed, credential-safe HTTP response.
///
/// The response body contains only a stable safe code and constant title.  It
/// never contains the incoming target, repository, package path, URL, source
/// error, upstream body, or authorization details.  `WWW-Authenticate` is
/// copied only for a classified 401, and `Retry-After` is copied only when it
/// is a syntactically valid delta-seconds or HTTP-date value.
pub fn map_error(error: ProxyError) -> ErrorResponse {
    let mut response = match &error {
        ProxyError::Inbound(error) => map_inbound(error),
        ProxyError::Upstream(error) => map_upstream(error),
        ProxyError::Timeout => {
            response_plan(StatusCode::GATEWAY_TIMEOUT, "upstream_timeout", false)
        }
        ProxyError::Cancelled => response_plan(
            StatusCode::INTERNAL_SERVER_ERROR,
            "request_cancelled",
            false,
        ),
        ProxyError::Unexpected => {
            response_plan(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", false)
        }
    };

    if let ProxyError::Upstream(UpstreamFailure::Unauthorized { challenges }) = &error {
        for challenge in challenges {
            response
                .headers_mut()
                .append(header::WWW_AUTHENTICATE, challenge.clone());
        }
    }

    if let ProxyError::Upstream(UpstreamFailure::RateLimited { retry_after })
    | ProxyError::Upstream(UpstreamFailure::Server {
        temporary: true,
        retry_after,
    }) = &error
    {
        if let Some(retry_after) = retry_after
            .as_ref()
            .filter(|value| valid_retry_after(value))
        {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, retry_after.clone());
        }
    }

    response
}

fn map_inbound(error: &InboundError) -> ErrorResponse {
    match error {
        InboundError::RequestTargetTooLarge => {
            response_plan(StatusCode::URI_TOO_LONG, "request_target_too_large", false)
        }
        InboundError::HeaderCountExceeded | InboundError::HeaderBytesExceeded => response_plan(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "request_headers_too_large",
            false,
        ),
        InboundError::InvalidHeader => {
            response_plan(StatusCode::BAD_REQUEST, "invalid_header", false)
        }
        InboundError::RequestBodyNotAllowed => {
            response_plan(StatusCode::BAD_REQUEST, "request_body_not_allowed", false)
        }
        InboundError::InvalidContentLength | InboundError::ConflictingContentLength => {
            response_plan(StatusCode::BAD_REQUEST, "invalid_request_framing", false)
        }
        InboundError::UnsupportedTransferEncoding => response_plan(
            StatusCode::BAD_REQUEST,
            "unsupported_transfer_encoding",
            false,
        ),
        InboundError::UnsupportedExpectation => response_plan(
            StatusCode::EXPECTATION_FAILED,
            "unsupported_expectation",
            false,
        ),
        InboundError::UnsupportedUpgrade => {
            response_plan(StatusCode::BAD_REQUEST, "unsupported_upgrade", false)
        }
        InboundError::Route(RouteError::UnsupportedMethod) => {
            response_plan(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", true)
        }
        InboundError::Route(_) => response_plan(StatusCode::NOT_FOUND, "not_found", false),
    }
}

fn map_upstream(error: &UpstreamFailure) -> ErrorResponse {
    match error {
        UpstreamFailure::Unauthorized { .. } => {
            response_plan(StatusCode::UNAUTHORIZED, "upstream_unauthorized", false)
        }
        UpstreamFailure::Forbidden => {
            response_plan(StatusCode::FORBIDDEN, "upstream_forbidden", false)
        }
        UpstreamFailure::NotFound { resource } => match resource {
            UpstreamResource::Layout => response_plan(StatusCode::NOT_FOUND, "not_found", false),
            UpstreamResource::MappedContent => {
                response_plan(StatusCode::BAD_GATEWAY, "upstream_missing_content", false)
            }
        },
        UpstreamFailure::RateLimited { .. } => response_plan(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_rate_limited",
            false,
        ),
        UpstreamFailure::Server { temporary, .. } => {
            if *temporary {
                response_plan(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "upstream_unavailable",
                    false,
                )
            } else {
                response_plan(StatusCode::BAD_GATEWAY, "upstream_failure", false)
            }
        }
        UpstreamFailure::Malformed => {
            response_plan(StatusCode::BAD_GATEWAY, "upstream_invalid_response", false)
        }
    }
}

fn response_plan(status: StatusCode, code: &'static str, allow_methods: bool) -> ErrorResponse {
    let body = Bytes::from(format!(
        "{{\"error\":\"{code}\",\"message\":\"{ERROR_TITLE}\"}}"
    ));
    let content_length = body.len().to_string();
    let mut builder = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_TYPE, ERROR_CONTENT_TYPE)
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if allow_methods {
        builder = builder.header(header::ALLOW, "GET, HEAD");
    }
    builder
        .body(Full::new(body))
        .expect("safe response headers are valid")
}

fn valid_retry_after(value: &HeaderValue) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if bytes.iter().all(u8::is_ascii_digit) {
        return bytes
            .iter()
            .try_fold(0_u64, |seconds, byte| {
                seconds
                    .checked_mul(10)
                    .and_then(|seconds| seconds.checked_add(u64::from(byte - b'0')))
            })
            .is_some();
    }

    valid_http_date(bytes)
}

fn valid_http_date(bytes: &[u8]) -> bool {
    valid_imf_fixdate(bytes) || valid_rfc850_date(bytes) || valid_asctime_date(bytes)
}

fn valid_imf_fixdate(bytes: &[u8]) -> bool {
    bytes.len() == 29
        && valid_weekday(&bytes[..3])
        && bytes[3] == b','
        && bytes[4] == b' '
        && valid_day(&bytes[5..7])
        && bytes[7] == b' '
        && valid_month(&bytes[8..11])
        && bytes[11] == b' '
        && bytes[12..16].iter().all(u8::is_ascii_digit)
        && bytes[16] == b' '
        && valid_time(&bytes[17..25])
        && bytes[25] == b' '
        && &bytes[26..] == b"GMT"
}

fn valid_rfc850_date(bytes: &[u8]) -> bool {
    let Some(comma) = bytes.iter().position(|byte| *byte == b',') else {
        return false;
    };
    (6..=8).contains(&comma)
        && valid_long_weekday(&bytes[..comma])
        && bytes.get(comma + 1) == Some(&b' ')
        && bytes.len() == comma + 24
        && valid_day(&bytes[comma + 2..comma + 4])
        && bytes[comma + 4] == b'-'
        && valid_month(&bytes[comma + 5..comma + 8])
        && bytes[comma + 8] == b'-'
        && bytes[comma + 9..comma + 11].iter().all(u8::is_ascii_digit)
        && bytes[comma + 11] == b' '
        && valid_time(&bytes[comma + 12..comma + 20])
        && bytes[comma + 20] == b' '
        && &bytes[comma + 21..] == b"GMT"
}

fn valid_asctime_date(bytes: &[u8]) -> bool {
    bytes.len() == 24
        && valid_weekday(&bytes[..3])
        && bytes[3] == b' '
        && valid_month(&bytes[4..7])
        && bytes[7] == b' '
        && (bytes[8].is_ascii_digit() || bytes[8] == b' ')
        && bytes[9].is_ascii_digit()
        && bytes[10] == b' '
        && valid_time(&bytes[11..19])
        && bytes[19] == b' '
        && bytes[20..].iter().all(u8::is_ascii_digit)
}

fn valid_day(bytes: &[u8]) -> bool {
    bytes.len() == 2
        && bytes.iter().all(u8::is_ascii_digit)
        && bytes[0] <= b'3'
        && !(bytes[0] == b'0' && bytes[1] == b'0')
        && !(bytes[0] == b'3' && bytes[1] > b'1')
}

fn valid_time(bytes: &[u8]) -> bool {
    bytes.len() == 8
        && bytes[2] == b':'
        && bytes[5] == b':'
        && bytes[..2].iter().all(u8::is_ascii_digit)
        && bytes[3..5].iter().all(u8::is_ascii_digit)
        && bytes[6..].iter().all(u8::is_ascii_digit)
        && two_digit_at_most(&bytes[..2], 23)
        && two_digit_at_most(&bytes[3..5], 59)
        && two_digit_at_most(&bytes[6..], 59)
}

fn two_digit_at_most(bytes: &[u8], maximum: u8) -> bool {
    bytes.len() == 2
        && bytes.iter().all(u8::is_ascii_digit)
        && (bytes[0] - b'0') * 10 + (bytes[1] - b'0') <= maximum
}

fn valid_weekday(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"Sun" | b"Mon" | b"Tue" | b"Wed" | b"Thu" | b"Fri" | b"Sat"
    )
}

fn valid_long_weekday(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"Sunday" | b"Monday" | b"Tuesday" | b"Wednesday" | b"Thursday" | b"Friday" | b"Saturday"
    )
}

fn valid_month(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"Jan"
            | b"Feb"
            | b"Mar"
            | b"Apr"
            | b"May"
            | b"Jun"
            | b"Jul"
            | b"Aug"
            | b"Sep"
            | b"Oct"
            | b"Nov"
            | b"Dec"
    )
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use hyper::{StatusCode, header, header::HeaderValue};

    use super::{ProxyError, UpstreamFailure, UpstreamResource, map_error};
    use crate::inbound::InboundError;
    use crate::routing::RouteError;

    fn body(response: hyper::Response<super::ErrorBody>) -> String {
        let bytes = response
            .into_body()
            .into_inner()
            .expect("safe body is always present");
        String::from_utf8(bytes.to_vec()).expect("safe body is UTF-8")
    }

    fn snapshot(response: hyper::Response<super::ErrorBody>) -> String {
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                format!(
                    "{}: {}",
                    name,
                    value.to_str().expect("safe response header is ASCII")
                )
            })
            .collect::<Vec<_>>()
            .join("\\n");
        format!("{status}\\n{headers}\\n\\n{}", body(response))
    }

    #[test]
    fn malformed_input_is_a_fixed_not_found_response() {
        let response = map_error(ProxyError::Inbound(InboundError::Route(
            RouteError::InvalidRoute,
        )));
        assert_eq!(
            snapshot(response),
            "404 Not Found\\ncache-control: no-store\\ncontent-type: application/json; charset=utf-8\\ncontent-length: 65\\nx-content-type-options: nosniff\\n\\n{\"error\":\"not_found\",\"message\":\"Request could not be completed.\"}"
        );
    }

    #[test]
    fn method_errors_preserve_allow_semantics_without_input() {
        let response = map_error(ProxyError::Inbound(InboundError::Route(
            RouteError::UnsupportedMethod,
        )));
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers().get("allow").unwrap(), "GET, HEAD");
        assert!(!body(response).contains("POST"));
    }

    #[test]
    fn local_limit_errors_have_client_status_and_safe_body() {
        let response = map_error(ProxyError::Inbound(InboundError::HeaderBytesExceeded));
        assert_eq!(
            response.status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
        assert_eq!(
            body(response),
            "{\"error\":\"request_headers_too_large\",\"message\":\"Request could not be completed.\"}"
        );
    }

    #[test]
    fn unauthorized_relays_only_challenges() {
        let challenge = HeaderValue::from_static(
            "Bearer realm=\"https://ghcr.example/token\",service=\"ghcr.io\"",
        );
        let response = map_error(ProxyError::Upstream(UpstreamFailure::Unauthorized {
            challenges: vec![challenge],
        }));
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer realm=\"https://ghcr.example/token\",service=\"ghcr.io\""
        );
        let body = body(response);
        assert!(!body.contains("ghcr"));
        assert!(!body.contains("token"));
        assert_eq!(
            body,
            "{\"error\":\"upstream_unauthorized\",\"message\":\"Request could not be completed.\"}"
        );
    }

    #[test]
    fn upstream_failure_classes_map_without_upstream_details() {
        let cases = [
            (
                UpstreamFailure::Forbidden,
                StatusCode::FORBIDDEN,
                "upstream_forbidden",
            ),
            (
                UpstreamFailure::NotFound {
                    resource: UpstreamResource::Layout,
                },
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                UpstreamFailure::NotFound {
                    resource: UpstreamResource::MappedContent,
                },
                StatusCode::BAD_GATEWAY,
                "upstream_missing_content",
            ),
            (
                UpstreamFailure::Malformed,
                StatusCode::BAD_GATEWAY,
                "upstream_invalid_response",
            ),
            (
                UpstreamFailure::Server {
                    temporary: false,
                    retry_after: None,
                },
                StatusCode::BAD_GATEWAY,
                "upstream_failure",
            ),
            (
                UpstreamFailure::Server {
                    temporary: true,
                    retry_after: None,
                },
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream_unavailable",
            ),
        ];

        for (failure, status, code) in cases {
            let response = map_error(ProxyError::Upstream(failure));
            assert_eq!(response.status(), status);
            let body = body(response);
            assert!(body.contains(code));
            assert!(!body.contains("ghcr"));
            assert!(!body.contains("https"));
        }
    }

    #[test]
    fn rate_limit_preserves_valid_retry_after_and_discards_invalid_values() {
        let response = map_error(ProxyError::Upstream(UpstreamFailure::RateLimited {
            retry_after: Some(HeaderValue::from_static("120")),
        }));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "120");

        let response = map_error(ProxyError::Upstream(UpstreamFailure::RateLimited {
            retry_after: Some(HeaderValue::from_static("retry immediately")),
        }));
        assert!(response.headers().get(header::RETRY_AFTER).is_none());
    }

    #[test]
    fn timeout_and_unexpected_errors_are_generic() {
        for (error, status, code) in [
            (
                ProxyError::Timeout,
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
            ),
            (
                ProxyError::Unexpected,
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
            ),
        ] {
            let response = map_error(error);
            assert_eq!(response.status(), status);
            let body = body(response);
            assert!(body.contains(code));
            assert!(!body.contains("error source"));
        }
    }

    #[test]
    fn response_body_has_a_consistent_safe_shape() {
        let response = map_error(ProxyError::Inbound(InboundError::InvalidHeader));
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json; charset=utf-8"
        );
        assert_eq!(
            response
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH).unwrap(),
            "70"
        );
        let expected = Bytes::from_static(
            b"{\"error\":\"invalid_header\",\"message\":\"Request could not be completed.\"}",
        );
        let actual = response
            .into_body()
            .into_inner()
            .expect("safe body is always present");
        assert_eq!(actual, expected);
    }

    #[test]
    fn debug_and_display_never_include_upstream_header_values() {
        let secret = "Bearer secret-canary";
        let error = ProxyError::Upstream(UpstreamFailure::Unauthorized {
            challenges: vec![HeaderValue::from_static(secret)],
        });
        assert!(!format!("{error:?}").contains(secret));
        assert!(!error.to_string().contains(secret));
    }

    #[test]
    fn retry_after_accepts_standard_http_dates() {
        for value in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            let response = map_error(ProxyError::Upstream(UpstreamFailure::RateLimited {
                retry_after: Some(HeaderValue::from_static(value)),
            }));
            assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), value);
        }
    }
}
