//! A deterministic loopback-only OCI Distribution fixture for integration tests.
//!
//! The fixture intentionally implements only the read-side subset needed by
//! the proxy: `/v2/`, manifests, and blobs. It has no registry persistence,
//! uploads, redirects, token exchange, or external-network behavior.

use std::{collections::HashMap, convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode,
    body::{Frame, Incoming},
    header::{self, HeaderName, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::sleep,
};
use tokio_stream::wrappers::ReceiverStream;

/// The boxed body used by fixture responses.
type FixtureBody = BoxBody<Bytes, Infallible>;
/// A complete fixture response.
type FixtureResponse = Response<FixtureBody>;

/// A route key for one repository-scoped OCI read.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Resource {
    /// The OCI manifest identified by a tag or digest reference.
    Manifest {
        /// Exact repository path below `/v2/`.
        repository: String,
        /// Exact tag or digest reference.
        reference: String,
    },
    /// An OCI blob identified by its digest.
    Blob {
        /// Exact repository path below `/v2/`.
        repository: String,
        /// Exact digest reference.
        digest: String,
    },
}

impl Resource {
    /// Returns the exact OCI Distribution path for this resource.
    pub fn path(&self) -> String {
        match self {
            Self::Manifest {
                repository,
                reference,
            } => format!("/v2/{repository}/manifests/{reference}"),
            Self::Blob { repository, digest } => format!("/v2/{repository}/blobs/{digest}"),
        }
    }
}

/// A response behavior for one registered OCI resource.
#[derive(Clone, Debug)]
pub enum ResourceResponse {
    /// Return the complete body with the provided media type.
    Body {
        /// Bytes returned to the client.
        body: Bytes,
        /// OCI media type or another test-controlled content type.
        content_type: &'static str,
    },
    /// Return a deliberately incomplete body with a larger advertised length.
    Truncated {
        /// Bytes sent before the connection is closed.
        body: Bytes,
        /// The larger length advertised in `Content-Length`.
        advertised_length: usize,
        /// Media type for the response.
        content_type: &'static str,
    },
    /// Return a controlled HTTP failure without an upstream body.
    Status {
        /// Response status.
        status: StatusCode,
        /// Optional validated response headers.
        headers: Vec<(HeaderName, HeaderValue)>,
    },
}

/// A request expectation checked by the fixture before dispatching a response.
#[derive(Clone, Debug)]
pub struct RequestExpectation {
    /// Required method.
    pub method: Method,
    /// Required exact origin-form path and query.
    pub path: String,
    /// Required header values, matched case-insensitively by header name.
    pub headers: Vec<(HeaderName, HeaderValue)>,
}

impl RequestExpectation {
    /// Creates an expectation for an exact method and path.
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            headers: Vec::new(),
        }
    }

    /// Requires an exact value for one received header.
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.push((name, value));
        self
    }
}

/// An observed request retained without body or sensitive header values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedRequest {
    /// Received method.
    pub method: Method,
    /// Received exact path and query.
    pub path: String,
    /// Whether an Authorization field was present.
    pub authorization_present: bool,
    /// Whether the request satisfied all configured expectations.
    pub matched_expectation: bool,
}

#[derive(Clone, Debug)]
struct FixtureState {
    resources: HashMap<Resource, ResourceResponse>,
    expectations: Vec<RequestExpectation>,
    observed: Vec<ObservedRequest>,
    private: bool,
    challenge: HeaderValue,
    expected_authorization: Option<HeaderValue>,
    large_stream_delay: Option<Duration>,
}

/// Configuration for [`RegistryFixture`].
#[derive(Clone, Debug)]
pub struct RegistryFixtureConfig {
    /// Require Authorization for all manifest/blob resources.
    pub private: bool,
    /// Challenge returned for an unauthenticated private request.
    pub challenge: HeaderValue,
    /// If set, require this exact Authorization value on private requests.
    pub expected_authorization: Option<HeaderValue>,
    /// Delay between chunks of a large response.
    pub large_stream_delay: Option<Duration>,
}

impl Default for RegistryFixtureConfig {
    fn default() -> Self {
        Self {
            private: false,
            challenge: HeaderValue::from_static(
                "Bearer realm=\"http://127.0.0.1/token\",service=\"fixture\"",
            ),
            expected_authorization: None,
            large_stream_delay: None,
        }
    }
}

/// A programmable local OCI Distribution fixture.
///
/// Call [`RegistryFixture::start`] to bind an ephemeral loopback port. The
/// returned handle owns the server task and shuts it down on drop. All
/// observations exclude header values and request bodies; callers can inspect
/// only safe presence/matching facts.
pub struct RegistryFixture {
    address: SocketAddr,
    state: Arc<Mutex<FixtureState>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl RegistryFixture {
    /// Starts a fixture on `127.0.0.1` with an ephemeral port.
    pub async fn start(config: RegistryFixtureConfig) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("loopback fixture should bind");
        let address = listener.local_addr().expect("fixture address should exist");
        let state = Arc::new(Mutex::new(FixtureState {
            resources: HashMap::new(),
            expectations: Vec::new(),
            observed: Vec::new(),
            private: config.private,
            challenge: config.challenge,
            expected_authorization: config.expected_authorization,
            large_stream_delay: config.large_stream_delay,
        }));
        let (shutdown, mut shutdown_rx) = oneshot::channel();
        let task_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else { break };
                        let io = TokioIo::new(stream);
                        let state = Arc::clone(&task_state);
                        tokio::spawn(async move {
                            let service = service_fn(move |request| handle(request, Arc::clone(&state)));
                            let _ = http1::Builder::new().serve_connection(io, service).await;
                        });
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });
        Self {
            address,
            state,
            shutdown: Some(shutdown),
            task: Some(task),
        }
    }

    /// Returns the loopback fixture socket address.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    #[allow(dead_code)]
    /// Returns the loopback fixture origin, such as `http://127.0.0.1:12345`.
    pub fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Adds or replaces an exact repository-scoped resource.
    pub async fn register(&self, resource: Resource, response: ResourceResponse) {
        self.state.lock().await.resources.insert(resource, response);
    }

    /// Adds an exact method/path/header expectation.
    pub async fn expect(&self, expectation: RequestExpectation) {
        self.state.lock().await.expectations.push(expectation);
    }

    /// Returns safe facts about received requests in arrival order.
    pub async fn observed(&self) -> Vec<ObservedRequest> {
        self.state.lock().await.observed.clone()
    }

    /// Returns once the listener is accepting connections.
    pub async fn wait_until_ready(&self) {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(self.address).await.is_ok() {
                return;
            }
            sleep(Duration::from_millis(1)).await;
        }
        panic!("fixture did not become ready");
    }
}

impl Drop for RegistryFixture {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn handle(
    request: Request<Incoming>,
    state: Arc<Mutex<FixtureState>>,
) -> Result<FixtureResponse, Infallible> {
    let method = request.method().clone();
    let path = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_owned(), |target| target.as_str().to_owned());
    let authorization = request.headers().get(header::AUTHORIZATION).cloned();

    if method != Method::GET && method != Method::HEAD {
        return Ok(simple_response(StatusCode::METHOD_NOT_ALLOWED, &[]));
    }

    let mut fixture = state.lock().await;
    let matched_expectation = fixture.expectations.iter().any(|expectation| {
        expectation.method == method
            && expectation.path == path
            && expectation.headers.iter().all(|(name, value)| {
                request
                    .headers()
                    .get(name)
                    .is_some_and(|received| received == value)
            })
    });
    fixture.observed.push(ObservedRequest {
        method: method.clone(),
        path: path.clone(),
        authorization_present: authorization.is_some(),
        matched_expectation,
    });

    if path == "/v2/" {
        return Ok(simple_response(
            StatusCode::OK,
            &[(
                HeaderName::from_static("docker-distribution-api-version"),
                HeaderValue::from_static("registry/2.0"),
            )],
        ));
    }

    let resource = match resource_for_path(&path) {
        Some(resource) => resource,
        None => return Ok(simple_response(StatusCode::NOT_FOUND, &[])),
    };

    if fixture.private
        && (authorization.is_none()
            || fixture
                .expected_authorization
                .as_ref()
                .is_some_and(|expected| authorization.as_ref() != Some(expected)))
    {
        return Ok(simple_response(
            StatusCode::UNAUTHORIZED,
            &[(header::WWW_AUTHENTICATE, fixture.challenge.clone())],
        ));
    }

    let response = fixture.resources.get(&resource).cloned();
    let delay = fixture.large_stream_delay;
    drop(fixture);
    let Some(response) = response else {
        return Ok(simple_response(StatusCode::NOT_FOUND, &[]));
    };
    render_resource(response, method == Method::HEAD, request.headers(), delay).await
}

fn resource_for_path(path: &str) -> Option<Resource> {
    let remainder = path.strip_prefix("/v2/")?;
    if let Some((repository, reference)) = remainder.split_once("/manifests/") {
        return Some(Resource::Manifest {
            repository: repository.to_owned(),
            reference: reference.to_owned(),
        });
    }
    let (repository, digest) = remainder.split_once("/blobs/")?;
    Some(Resource::Blob {
        repository: repository.to_owned(),
        digest: digest.to_owned(),
    })
}

async fn render_resource(
    response: ResourceResponse,
    head: bool,
    request_headers: &HeaderMap,
    delay: Option<Duration>,
) -> Result<FixtureResponse, Infallible> {
    match response {
        ResourceResponse::Body { body, content_type } => {
            if request_headers
                .get(header::IF_NONE_MATCH)
                .is_some_and(|value| value == "\"fixture-etag\"")
            {
                return Ok(simple_response(
                    StatusCode::NOT_MODIFIED,
                    &[(header::ETAG, HeaderValue::from_static("\"fixture-etag\""))],
                ));
            }
            let mut response = simple_response(
                StatusCode::OK,
                &[
                    (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
                    (header::ETAG, HeaderValue::from_static("\"fixture-etag\"")),
                ],
            );
            let body = selected_range(body, request_headers, &mut response);
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&body.len().to_string()).expect("length is valid"),
            );
            if head {
                *response.body_mut() = boxed_full(Bytes::new());
            } else if delay.is_some() {
                *response.body_mut() = boxed_chunks(vec![body], delay);
            } else {
                *response.body_mut() = boxed_full(body);
            }
            Ok(response)
        }
        ResourceResponse::Truncated {
            body,
            advertised_length,
            content_type,
        } => {
            let mut response = simple_response(
                StatusCode::OK,
                &[
                    (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
                    (
                        header::CONTENT_LENGTH,
                        HeaderValue::from_str(&advertised_length.to_string())
                            .expect("length is valid"),
                    ),
                ],
            );
            if head {
                *response.body_mut() = boxed_full(Bytes::new());
            } else {
                *response.body_mut() = boxed_chunks(vec![body], delay);
            }
            Ok(response)
        }
        ResourceResponse::Status { status, headers } => Ok(simple_response(status, &headers)),
    }
}

fn selected_range(
    body: Bytes,
    request_headers: &HeaderMap,
    response: &mut FixtureResponse,
) -> Bytes {
    let Some(range) = request_headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
    else {
        return body;
    };
    let Some((start, end)) = parse_single_range(range, body.len()) else {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes */{}", body.len())).expect("range is valid"),
        );
        *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
        return Bytes::new();
    };
    response.headers_mut().insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes {start}-{end}/{}", body.len()))
            .expect("range is valid"),
    );
    *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    body.slice(start..=end)
}

fn parse_single_range(value: &str, length: usize) -> Option<(usize, usize)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    let start = start.parse::<usize>().ok()?;
    let end = if end.is_empty() {
        length.checked_sub(1)?
    } else {
        end.parse::<usize>().ok()?.min(length.checked_sub(1)?)
    };
    (start <= end && start < length).then_some((start, end))
}

fn boxed_full(body: Bytes) -> FixtureBody {
    Full::new(body).boxed()
}

fn boxed_chunks(chunks: Vec<Bytes>, delay: Option<Duration>) -> FixtureBody {
    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        for chunk in chunks {
            if let Some(delay) = delay {
                sleep(delay).await;
            }
            if sender.send(Ok(Frame::data(chunk))).await.is_err() {
                break;
            }
        }
    });
    StreamBody::new(ReceiverStream::new(receiver)).boxed()
}

fn simple_response(status: StatusCode, headers: &[(HeaderName, HeaderValue)]) -> FixtureResponse {
    let mut response = Response::new(boxed_full(Bytes::new()));
    *response.status_mut() = status;
    for (name, value) in headers {
        response.headers_mut().insert(name, value.clone());
    }
    response
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn binds_loopback_and_records_safe_request_facts() {
        let fixture = RegistryFixture::start(RegistryFixtureConfig::default()).await;
        fixture.wait_until_ready().await;
        fixture
            .register(
                Resource::Blob {
                    repository: "acme/demo".into(),
                    digest: "sha256:abc".into(),
                },
                ResourceResponse::Body {
                    body: Bytes::from_static(b"fixture"),
                    content_type: "application/octet-stream",
                },
            )
            .await;
        fixture
            .expect(
                RequestExpectation::new(Method::GET, "/v2/acme/demo/blobs/sha256:abc").with_header(
                    header::AUTHORIZATION,
                    HeaderValue::from_static("Bearer canary"),
                ),
            )
            .await;

        let mut stream = tokio::net::TcpStream::connect(fixture.address)
            .await
            .unwrap();
        stream
            .write_all(
                b"GET /v2/acme/demo/blobs/sha256:abc HTTP/1.1\r\nHost: fixture\r\nAuthorization: Bearer canary\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.ends_with("\r\n\r\nfixture"));
        let observed = fixture.observed().await;
        assert_eq!(observed.len(), 1);
        assert!(observed[0].matched_expectation);
        assert!(observed[0].authorization_present);
    }

    #[test]
    fn builds_exact_manifest_and_blob_paths() {
        assert_eq!(
            Resource::Manifest {
                repository: "acme/demo".into(),
                reference: "oras-packages.v1".into(),
            }
            .path(),
            "/v2/acme/demo/manifests/oras-packages.v1"
        );
        assert_eq!(
            resource_for_path("/v2/acme/demo/blobs/sha256:abc"),
            Some(Resource::Blob {
                repository: "acme/demo".into(),
                digest: "sha256:abc".into(),
            })
        );
    }

    #[test]
    fn parses_only_one_bounded_byte_range() {
        assert_eq!(parse_single_range("bytes=1-3", 8), Some((1, 3)));
        assert_eq!(parse_single_range("bytes=4-", 8), Some((4, 7)));
        assert_eq!(parse_single_range("bytes=1-3,5-6", 8), None);
        assert_eq!(parse_single_range("bytes=9-10", 8), None);
    }
}
