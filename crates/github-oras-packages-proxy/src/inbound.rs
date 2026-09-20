//! Admission checks for untrusted requests at the proxy boundary.
//!
//! The listener must call these checks before selecting an upstream operation.
//! They intentionally operate on the request head only: v1 is a read-only
//! `GET`/`HEAD` surface, so a request body is rejected rather than consumed or
//! forwarded.  The [`limited_body`] helper is available for future routes that
//! need to consume a bounded body after this boundary.

use http_body_util::Limited;
use hyper::{
    HeaderMap, Method, Request,
    body::Body,
    header::{self, HeaderValue},
};

use crate::routing::{EnabledProtocols, RouteError, ValidatedRoute, parse_raw_target};

/// Maximum request-target bytes accepted by the default edge policy.
pub const DEFAULT_MAX_TARGET_BYTES: usize = 8 * 1024;
/// Maximum number of received header fields, counting repeated fields.
pub const DEFAULT_MAX_HEADER_COUNT: usize = 64;
/// Maximum serialized header-field bytes, excluding the request line.
pub const DEFAULT_MAX_HEADER_BYTES: usize = 16 * 1024;
/// Default maximum body bytes for a bounded body stream.
pub const DEFAULT_MAX_BODY_BYTES: usize = 0;

/// Limits applied before a request can cause an upstream operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InboundLimits {
    max_target_bytes: usize,
    max_header_count: usize,
    max_header_bytes: usize,
    max_body_bytes: usize,
}

impl InboundLimits {
    /// Creates an edge policy with explicit byte and count limits.
    ///
    /// A zero body limit is the safe default for the read-only v1 routes.  The
    /// body limit is also used by [`limited_body`] for a future body-consuming
    /// endpoint.
    pub const fn new(
        max_target_bytes: usize,
        max_header_count: usize,
        max_header_bytes: usize,
        max_body_bytes: usize,
    ) -> Self {
        Self {
            max_target_bytes,
            max_header_count,
            max_header_bytes,
            max_body_bytes,
        }
    }

    /// Returns the request-target limit.
    pub const fn max_target_bytes(self) -> usize {
        self.max_target_bytes
    }

    /// Returns the header-field count limit.
    pub const fn max_header_count(self) -> usize {
        self.max_header_count
    }

    /// Returns the serialized header-byte limit.
    pub const fn max_header_bytes(self) -> usize {
        self.max_header_bytes
    }

    /// Returns the body-stream limit.
    pub const fn max_body_bytes(self) -> usize {
        self.max_body_bytes
    }
}

impl Default for InboundLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_TARGET_BYTES,
            DEFAULT_MAX_HEADER_COUNT,
            DEFAULT_MAX_HEADER_BYTES,
            DEFAULT_MAX_BODY_BYTES,
        )
    }
}

/// A safe, input-free reason a request was rejected at the edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboundError {
    /// The target exceeded the configured request-line target limit.
    RequestTargetTooLarge,
    /// The request contained too many header fields.
    HeaderCountExceeded,
    /// The serialized headers exceeded the configured byte limit.
    HeaderBytesExceeded,
    /// A header value contained a forbidden control byte.
    InvalidHeader,
    /// The request declared a body on the read-only v1 surface.
    RequestBodyNotAllowed,
    /// The request declared an invalid Content-Length value.
    InvalidContentLength,
    /// Repeated Content-Length fields disagreed.
    ConflictingContentLength,
    /// Transfer-Encoding is not accepted on the read-only surface.
    UnsupportedTransferEncoding,
    /// Expect/continue is not accepted on the read-only surface.
    UnsupportedExpectation,
    /// Protocol upgrade is not accepted by the package route.
    UnsupportedUpgrade,
    /// The route parser rejected the method or target.
    Route(RouteError),
}

impl InboundError {
    /// Returns a stable non-sensitive error code for the response mapper.
    pub const fn code(self) -> &'static str {
        match self {
            Self::RequestTargetTooLarge => "request_target_too_large",
            Self::HeaderCountExceeded => "header_count_exceeded",
            Self::HeaderBytesExceeded => "header_bytes_exceeded",
            Self::InvalidHeader => "invalid_header",
            Self::RequestBodyNotAllowed => "request_body_not_allowed",
            Self::InvalidContentLength => "invalid_content_length",
            Self::ConflictingContentLength => "conflicting_content_length",
            Self::UnsupportedTransferEncoding => "unsupported_transfer_encoding",
            Self::UnsupportedExpectation => "unsupported_expectation",
            Self::UnsupportedUpgrade => "unsupported_upgrade",
            Self::Route(error) => error.code(),
        }
    }
}

impl std::fmt::Display for InboundError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for InboundError {}

/// Validates a Hyper request without exposing its URI or headers to callers.
///
/// Hyper has already parsed the request line by this point.  Absolute-form,
/// authority-form, and a missing path are still rejected because the public
/// contract accepts only origin-form package routes.
pub fn validate_request<B>(
    request: &Request<B>,
    enabled_protocols: EnabledProtocols,
    limits: InboundLimits,
) -> Result<ValidatedRoute, InboundError>
where
    B: Body,
{
    let uri = request.uri();
    if uri.scheme().is_some() || uri.authority().is_some() {
        return Err(InboundError::Route(RouteError::InvalidRoute));
    }

    let target = uri
        .path_and_query()
        .ok_or(InboundError::Route(RouteError::InvalidRoute))?;
    let raw_target = target.as_str();
    if raw_target.len() > limits.max_target_bytes {
        return Err(InboundError::RequestTargetTooLarge);
    }

    let route = validate_request_head(
        request.method(),
        raw_target,
        request.headers(),
        enabled_protocols,
        limits,
    )?;

    if !request.body().is_end_stream() {
        return Err(InboundError::RequestBodyNotAllowed);
    }

    Ok(route)
}

/// Validates a read-only request for the human-readable autoindex namespace.
///
/// Unlike [`validate_request`], this boundary does not select a package
/// protocol or repository. The caller supplies the configured repository and
/// receives only the unnormalized origin-form target.
pub fn validate_autoindex_request<B>(
    request: &Request<B>,
    limits: InboundLimits,
) -> Result<&str, InboundError>
where
    B: Body,
{
    if request.uri().scheme().is_some() || request.uri().authority().is_some() {
        return Err(InboundError::Route(RouteError::InvalidRoute));
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return Err(InboundError::Route(RouteError::UnsupportedMethod));
    }
    let target = request
        .uri()
        .path_and_query()
        .ok_or(InboundError::Route(RouteError::InvalidRoute))?
        .as_str();
    if target.len() > limits.max_target_bytes {
        return Err(InboundError::RequestTargetTooLarge);
    }
    validate_headers(request.headers(), limits)?;
    if !request.body().is_end_stream() {
        return Err(InboundError::RequestBodyNotAllowed);
    }
    Ok(target)
}

/// Validates a parsed request head before any upstream operation is selected.
///
/// The raw target must be passed before framework routing or normalization.  A
/// successful result is the same typed route boundary used by the existing
/// route parser.  No header values are returned, and no body is consumed.
pub fn validate_request_head(
    method: &Method,
    raw_target: &str,
    headers: &HeaderMap,
    enabled_protocols: EnabledProtocols,
    limits: InboundLimits,
) -> Result<ValidatedRoute, InboundError> {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return Err(InboundError::Route(RouteError::UnsupportedMethod));
    }

    if raw_target.len() > limits.max_target_bytes {
        return Err(InboundError::RequestTargetTooLarge);
    }

    validate_headers(headers, limits)?;
    parse_raw_target(method.as_str(), raw_target, enabled_protocols).map_err(InboundError::Route)
}

/// Wraps a body in a byte-counting limit before it is consumed.
///
/// The wrapper yields [`http_body_util::LengthLimitError`] once a data frame
/// would exceed the configured limit.  Callers must not start upstream work
/// until the body has been accepted or fully drained according to their route
/// policy.
pub fn limited_body<B>(body: B, limits: InboundLimits) -> Limited<B>
where
    B: Body,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    Limited::new(body, limits.max_body_bytes)
}

fn validate_headers(headers: &HeaderMap, limits: InboundLimits) -> Result<(), InboundError> {
    if headers.len() > limits.max_header_count {
        return Err(InboundError::HeaderCountExceeded);
    }

    let mut serialized_bytes = 2_usize; // the terminating CRLF
    for (name, value) in headers {
        if contains_forbidden_control(value) {
            return Err(InboundError::InvalidHeader);
        }

        // Account for `name: value\r\n`, not just the payload.  checked_add
        // keeps a maliciously large configured limit from wrapping.
        serialized_bytes = serialized_bytes
            .checked_add(name.as_str().len())
            .and_then(|bytes| bytes.checked_add(2))
            .and_then(|bytes| bytes.checked_add(value.as_bytes().len()))
            .and_then(|bytes| bytes.checked_add(2))
            .ok_or(InboundError::HeaderBytesExceeded)?;
    }
    if serialized_bytes > limits.max_header_bytes {
        return Err(InboundError::HeaderBytesExceeded);
    }

    validate_content_length(headers)?;

    if headers.contains_key(header::TRANSFER_ENCODING) {
        return Err(InboundError::UnsupportedTransferEncoding);
    }
    if headers.contains_key(header::EXPECT) {
        return Err(InboundError::UnsupportedExpectation);
    }
    if headers.contains_key(header::UPGRADE) || connection_requests_upgrade(headers) {
        return Err(InboundError::UnsupportedUpgrade);
    }

    Ok(())
}

fn validate_content_length(headers: &HeaderMap) -> Result<(), InboundError> {
    let mut declared_length = None;
    for value in headers.get_all(header::CONTENT_LENGTH).iter() {
        let length = parse_content_length(value)?;
        if let Some(previous) = declared_length {
            if previous != length {
                return Err(InboundError::ConflictingContentLength);
            }
        } else {
            declared_length = Some(length);
        }
    }

    if declared_length.is_some_and(|length| length != 0) {
        return Err(InboundError::RequestBodyNotAllowed);
    }

    Ok(())
}

fn parse_content_length(value: &HeaderValue) -> Result<u64, InboundError> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(InboundError::InvalidContentLength);
    }

    bytes.iter().try_fold(0_u64, |length, byte| {
        length
            .checked_mul(10)
            .and_then(|length| length.checked_add(u64::from(byte - b'0')))
            .ok_or(InboundError::InvalidContentLength)
    })
}

fn contains_forbidden_control(value: &HeaderValue) -> bool {
    value
        .as_bytes()
        .iter()
        .any(|byte| byte.is_ascii_control() && *byte != b'\t')
}

fn connection_requests_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .flat_map(|value| value.as_bytes().split(|byte| *byte == b','))
        .any(|token| token.trim_ascii().eq_ignore_ascii_case(b"upgrade"))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Empty, Full};
    use hyper::header::HeaderValue;
    use hyper::{HeaderMap, Method, Request, header};

    use super::{
        DEFAULT_MAX_BODY_BYTES, DEFAULT_MAX_HEADER_BYTES, DEFAULT_MAX_HEADER_COUNT,
        DEFAULT_MAX_TARGET_BYTES, InboundError, InboundLimits, limited_body, validate_request,
        validate_request_head,
    };
    use crate::routing::{EnabledProtocols, Protocol, RouteError, ValidatedRepository};

    fn target() -> &'static str {
        "/r/v1/Zm9v/pypi/simple/example/"
    }

    fn limits() -> InboundLimits {
        InboundLimits::default()
    }

    #[test]
    fn defaults_are_bounded_and_read_only() {
        let limits = limits();
        assert_eq!(limits.max_target_bytes(), DEFAULT_MAX_TARGET_BYTES);
        assert_eq!(limits.max_header_count(), DEFAULT_MAX_HEADER_COUNT);
        assert_eq!(limits.max_header_bytes(), DEFAULT_MAX_HEADER_BYTES);
        assert_eq!(limits.max_body_bytes(), DEFAULT_MAX_BODY_BYTES);
    }

    #[test]
    fn accepts_get_and_head_with_zero_length_body() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        for method in [Method::GET, Method::HEAD] {
            let route = validate_request_head(
                &method,
                target(),
                &headers,
                EnabledProtocols::all(),
                limits(),
            )
            .expect("read-only request should be accepted");
            assert_eq!(route.protocol(), Protocol::Pypi);
            assert_eq!(route.repository().as_str(), "foo");
        }
    }

    #[test]
    fn request_validation_preserves_only_the_typed_route() {
        let request = Request::builder()
            .method(Method::GET)
            .uri(target())
            .header("user-agent", "test-client")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let route = validate_request(&request, EnabledProtocols::all(), limits()).unwrap();
        assert_eq!(route.protocol_path().as_str(), "simple/example/");
    }

    #[test]
    fn request_validation_rejects_a_body_even_without_content_length() {
        let request = Request::builder()
            .method(Method::GET)
            .uri(target())
            .body(Full::new(Bytes::from_static(b"unexpected body")))
            .unwrap();
        assert_eq!(
            validate_request(&request, EnabledProtocols::all(), limits()),
            Err(InboundError::RequestBodyNotAllowed)
        );
    }

    #[test]
    fn rejects_unsupported_method_before_other_request_details() {
        let mut headers = HeaderMap::new();
        for index in 0..=DEFAULT_MAX_HEADER_COUNT {
            let name = format!("x-test-{index}");
            headers.insert(
                hyper::header::HeaderName::try_from(name).unwrap(),
                HeaderValue::from_static("value"),
            );
        }
        assert_eq!(
            validate_request_head(
                &Method::POST,
                "/not-a-route",
                &headers,
                EnabledProtocols::all(),
                limits(),
            ),
            Err(InboundError::Route(RouteError::UnsupportedMethod))
        );
    }

    #[test]
    fn rejects_oversized_request_target_before_route_parsing() {
        let limits = InboundLimits::new(8, 64, 16 * 1024, 0);
        assert_eq!(
            validate_request_head(
                &Method::GET,
                target(),
                &HeaderMap::new(),
                EnabledProtocols::all(),
                limits,
            ),
            Err(InboundError::RequestTargetTooLarge)
        );

        let request = Request::builder()
            .method(Method::GET)
            .uri(target())
            .body(Empty::<Bytes>::new())
            .unwrap();
        assert_eq!(
            validate_request(&request, EnabledProtocols::all(), limits),
            Err(InboundError::RequestTargetTooLarge)
        );
    }

    #[test]
    fn rejects_oversized_header_count() {
        let mut headers = HeaderMap::new();
        for index in 0..3 {
            let name = format!("x-test-{index}");
            headers.insert(
                hyper::header::HeaderName::try_from(name).unwrap(),
                HeaderValue::from_static("v"),
            );
        }
        assert_eq!(
            validate_request_head(
                &Method::GET,
                target(),
                &headers,
                EnabledProtocols::all(),
                InboundLimits::new(8 * 1024, 2, 16 * 1024, 0),
            ),
            Err(InboundError::HeaderCountExceeded)
        );
    }

    #[test]
    fn rejects_oversized_header_bytes() {
        let mut headers = HeaderMap::new();
        headers.insert("x-test", HeaderValue::from_static("1234567890"));
        assert_eq!(
            validate_request_head(
                &Method::GET,
                target(),
                &headers,
                EnabledProtocols::all(),
                InboundLimits::new(8 * 1024, 64, 10, 0),
            ),
            Err(InboundError::HeaderBytesExceeded)
        );
    }

    #[test]
    fn rejects_control_bytes_in_header_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-test", HeaderValue::from_static("value\twith-tab"));
        // HTAB is permitted by HTTP field-value syntax and is not rejected.
        assert!(
            validate_request_head(
                &Method::GET,
                target(),
                &headers,
                EnabledProtocols::all(),
                limits(),
            )
            .is_ok()
        );

        // Hyper's typed header parser rejects the same forbidden bytes before
        // they can enter a HeaderMap; the edge check remains defense in depth
        // for any alternate parser or future adapter.
        assert!(HeaderValue::from_bytes(b"value\x7fwith-del").is_err());
    }

    #[test]
    fn rejects_nonzero_and_invalid_body_framing() {
        let cases = [
            ("1", InboundError::RequestBodyNotAllowed),
            ("not-a-length", InboundError::InvalidContentLength),
            ("18446744073709551616", InboundError::InvalidContentLength),
        ];
        for (value, error) in cases {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static(value));
            assert_eq!(
                validate_request_head(
                    &Method::GET,
                    target(),
                    &headers,
                    EnabledProtocols::all(),
                    limits(),
                ),
                Err(error)
            );
        }
    }

    #[test]
    fn rejects_conflicting_lengths_transfer_encoding_expect_and_upgrade() {
        let mut conflicting = HeaderMap::new();
        conflicting.append(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        conflicting.append(header::CONTENT_LENGTH, HeaderValue::from_static("1"));
        assert_eq!(
            validate_request_head(
                &Method::GET,
                target(),
                &conflicting,
                EnabledProtocols::all(),
                limits(),
            ),
            Err(InboundError::ConflictingContentLength)
        );

        for (name, value, error) in [
            (
                header::TRANSFER_ENCODING,
                "chunked",
                InboundError::UnsupportedTransferEncoding,
            ),
            (
                header::EXPECT,
                "100-continue",
                InboundError::UnsupportedExpectation,
            ),
            (header::UPGRADE, "h2c", InboundError::UnsupportedUpgrade),
            (
                header::CONNECTION,
                "keep-alive, Upgrade",
                InboundError::UnsupportedUpgrade,
            ),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(name, HeaderValue::from_static(value));
            assert_eq!(
                validate_request_head(
                    &Method::GET,
                    target(),
                    &headers,
                    EnabledProtocols::all(),
                    limits(),
                ),
                Err(error)
            );
        }
    }

    #[tokio::test]
    async fn limited_body_fails_when_a_frame_exceeds_the_configured_limit() {
        let body = limited_body(
            Full::new(Bytes::from_static(b"too large")),
            InboundLimits::new(8 * 1024, 64, 16 * 1024, 3),
        );
        let error = body.collect().await.expect_err("body should exceed limit");
        assert!(error.to_string().contains("length limit exceeded"));
    }

    #[test]
    fn rejects_absolute_form_in_request_validation() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("http://example.test/r/v1/Zm9v/pypi/simple/example/")
            .body(Empty::<Bytes>::new())
            .unwrap();
        assert_eq!(
            validate_request(&request, EnabledProtocols::all(), limits()),
            Err(InboundError::Route(RouteError::InvalidRoute))
        );
    }

    #[test]
    fn repository_route_errors_remain_safe_and_input_free() {
        let repository = ValidatedRepository::parse("foo").unwrap();
        let bad_target = format!("/r/v1/{}/pypi/simple/%2f", repository.locator());
        let error = validate_request_head(
            &Method::GET,
            &bad_target,
            &HeaderMap::new(),
            EnabledProtocols::all(),
            limits(),
        )
        .unwrap_err();
        assert_eq!(error, InboundError::Route(RouteError::InvalidRoute));
        assert_eq!(error.code(), "invalid_route");
    }
}
