//! Minimal HTTP service lifecycle and health endpoints.
//!
//! The package request handler is intentionally supplied by a later gateway;
//! this module owns only listener binding, bounded HTTP/1 connections, health
//! responses, and deterministic shutdown. Health output never contains request
//! targets, repository names, upstream URLs, or header values.

use std::{convert::Infallible, error::Error, future::Future, net::SocketAddr, sync::Arc};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{Method, Request, Response, StatusCode, body::Incoming, header};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, sync::watch, task::JoinSet};

use crate::{config::Config, errors::map_error, inbound::InboundLimits, routing::EnabledProtocols};

/// A health response body accepted by Hyper's HTTP/1 server.
pub type BoxError = Box<dyn Error + Send + Sync>;
/// A health response body accepted by Hyper's HTTP/1 server.
pub type HealthBody = UnsyncBoxBody<Bytes, BoxError>;
/// A request handler result for the lifecycle server.
pub type ServiceResponse = Response<HealthBody>;

fn never_to_error(error: Infallible) -> Box<dyn Error + Send + Sync> {
    match error {}
}

/// Converts a fixed infallible body into the service's response body type.
pub fn fixed_body(body: Bytes) -> HealthBody {
    Full::new(body).map_err(never_to_error).boxed_unsync()
}

/// Converts a fixed safe response into the common service response body type.
pub fn into_service_response(response: Response<Full<Bytes>>) -> ServiceResponse {
    response.map(|body| body.map_err(never_to_error).boxed_unsync())
}

/// A small HTTP/1 loopback service lifecycle.
pub struct Server {
    address: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Server {
    /// Binds the configured listen address and starts accepting HTTP/1 clients.
    ///
    /// `handler` is called only for non-health requests. The handler must
    /// return a safe response and must not retain request credentials.
    pub async fn start<F, Fut>(config: &Config, handler: F) -> Result<Self, std::io::Error>
    where
        F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ServiceResponse> + Send + 'static,
    {
        let listener = TcpListener::bind(config.listen_addr()).await?;
        let address = listener.local_addr()?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let handler = Arc::new(handler);
        let task = tokio::spawn(accept_loop(listener, handler, shutdown_rx));
        Ok(Self {
            address,
            shutdown,
            task: Some(task),
        })
    }

    /// Returns the actual bound socket address, including an ephemeral port.
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// Marks the listener not-ready and waits for its accept loop to stop.
    ///
    /// This consumes the owner so no new requests can race with teardown.
    /// Existing connection tasks are cancelled after the accept loop stops.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn accept_loop<F, Fut>(
    listener: TcpListener,
    handler: Arc<F>,
    mut shutdown: watch::Receiver<bool>,
) where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ServiceResponse> + Send + 'static,
{
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else { break };
                let handler = Arc::clone(&handler);
                connections.spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = hyper::service::service_fn(move |request| {
                        let handler = Arc::clone(&handler);
                        async move { Ok::<_, Infallible>(dispatch(request, handler).await) }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(true).max_headers(64);
                    let _ = builder.serve_connection(io, service).await;
                });
            }
        }
    }
    connections.shutdown().await;
}

async fn dispatch<F, Fut>(request: Request<Incoming>, handler: Arc<F>) -> ServiceResponse
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ServiceResponse> + Send + 'static,
{
    match (request.method(), request.uri().path()) {
        (&Method::GET | &Method::HEAD, "/healthz") => health_response(true, request.method()),
        (&Method::GET | &Method::HEAD, "/readyz") => health_response(true, request.method()),
        _ => handler(request).await,
    }
}

fn health_response(ready: bool, method: &Method) -> ServiceResponse {
    let body = Bytes::from(if ready { "ok\n" } else { "not ready\n" });
    Response::builder()
        .status(if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        })
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(fixed_body(if method == Method::HEAD {
            Bytes::new()
        } else {
            body
        }))
        .expect("fixed health headers are valid")
}

/// Constructs the default handler used before the OCI/PyPI gateway is wired.
///
/// It validates the inbound request and returns the common safe error shape;
/// no request can accidentally reach an upstream from this placeholder.
pub fn unavailable_handler(
    enabled_protocols: EnabledProtocols,
    limits: InboundLimits,
) -> impl Fn(Request<Incoming>) -> std::future::Ready<ServiceResponse> + Send + Sync + 'static {
    move |request| {
        let response = match crate::inbound::validate_request(&request, enabled_protocols, limits) {
            Ok(_) => map_error(crate::errors::ProxyError::Unexpected),
            Err(error) => map_error(error.into()),
        };
        std::future::ready(response.map(|body| body.map_err(never_to_error).boxed_unsync()))
    }
}

#[cfg(test)]
mod tests {
    use super::{Server, unavailable_handler};
    use crate::{config::Config, routing::EnabledProtocols};
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn request(address: std::net::SocketAddr, method: &str, target: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                format!(
                    "{method} {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    fn config() -> Config {
        Config::from_maps(
            &BTreeMap::new(),
            &BTreeMap::from([(String::from("LISTEN_ADDR"), String::from("127.0.0.1:0"))]),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn health_is_loopback_safe_and_shutdown_is_deterministic() {
        let config = config();
        let server = Server::start(
            &config,
            unavailable_handler(EnabledProtocols::none(), config.inbound_limits()),
        )
        .await
        .unwrap();
        let response = request(server.address(), "GET", "/healthz").await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with("ok\n"));
        let head = request(server.address(), "HEAD", "/healthz").await;
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.ends_with("\r\n\r\n"));
        let address = server.address();
        server.shutdown().await;
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn ready_and_unknown_requests_have_stable_safe_shapes() {
        let config = config();
        let server = Server::start(
            &config,
            unavailable_handler(EnabledProtocols::none(), config.inbound_limits()),
        )
        .await
        .unwrap();
        let ready = request(server.address(), "GET", "/readyz").await;
        assert!(ready.starts_with("HTTP/1.1 200 OK\r\n"));
        let unknown = request(server.address(), "GET", "/not-health").await;
        assert!(unknown.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(unknown.contains("\"error\":\"not_found\""));
        server.shutdown().await;
    }
}
