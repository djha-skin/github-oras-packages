//! Black-box coverage for same-origin GitHub Packages bearer authentication.

mod support;

use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use github_oras_packages_proxy::{
    config::Config, oci::OciClient, proxy, routing::ValidatedRepository, server::Server,
};
use hyper::header::HeaderValue;
use support::registry::{RegistryFixture, RegistryFixtureConfig, Resource, ResourceResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const REPOSITORY: &str = "acme/private";
const MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.empty.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":0},"layers":[]}"#;
const TOKEN_BASIC: &str = "Basic Z2l0aHViLXVzZXI6c2VjcmV0";

fn config(upstream: &str, broker: bool) -> Config {
    let mut values = BTreeMap::from([
        ("LISTEN_ADDR".to_owned(), "127.0.0.1:0".to_owned()),
        ("UPSTREAM".to_owned(), upstream.to_owned()),
        ("REPOSITORY".to_owned(), REPOSITORY.to_owned()),
        ("ALLOWED_HOSTS".to_owned(), "127.0.0.1".to_owned()),
        ("ALLOW_INSECURE_LOOPBACK".to_owned(), "true".to_owned()),
    ]);
    if broker {
        values.insert("TOKEN_USERNAME".to_owned(), "github-user".to_owned());
        values.insert("TOKEN_PASSWORD".to_owned(), "secret".to_owned());
    }
    Config::from_maps(&BTreeMap::new(), &values).unwrap()
}

async fn register_private_manifest(fixture: &RegistryFixture) {
    fixture
        .register(
            Resource::Manifest {
                repository: REPOSITORY.to_owned(),
                reference: "demo".to_owned(),
            },
            ResourceResponse::Body {
                body: Bytes::from_static(MANIFEST),
                content_type: "application/vnd.oci.image.manifest.v1+json",
            },
        )
        .await;
}

async fn start_proxy(config: &Config) -> Server {
    let client = Arc::new(OciClient::new(config).unwrap());
    let repository = ValidatedRepository::parse(REPOSITORY).unwrap();
    let limits = config.inbound_limits();
    let handler = move |request| {
        let client = Arc::clone(&client);
        let repository = repository.clone();
        async move { proxy::handle_gateway(request, client, repository, limits).await }
    };
    Server::start(config, handler).await.unwrap()
}

async fn request(address: std::net::SocketAddr, authorization: Option<&str>) -> Vec<u8> {
    let authorization =
        authorization.map_or(String::new(), |value| format!("Authorization: {value}\r\n"));
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /v2/{REPOSITORY}/manifests/demo HTTP/1.1\r\nHost: proxy\r\n{authorization}Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    response
}

fn starts_with_status(response: &[u8], status: &str) -> bool {
    String::from_utf8_lossy(response).starts_with(status)
}

fn token_request_count(fixture: &[support::registry::ObservedRequest]) -> usize {
    fixture
        .iter()
        .filter(|request| request.path.split('?').next() == Some("/token"))
        .count()
}

#[tokio::test]
async fn private_repository_exchanges_and_caches_same_origin_bearer_tokens() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig {
        private: true,
        ..RegistryFixtureConfig::default()
    })
    .await;
    register_private_manifest(&fixture).await;
    fixture
        .configure_token_service(HeaderValue::from_static(TOKEN_BASIC))
        .await;
    fixture.issue_token("first-token", 120).await;
    fixture
        .set_expected_authorization(HeaderValue::from_static("Bearer first-token"))
        .await;
    let config = config(&fixture.origin(), true);
    let server = start_proxy(&config).await;

    let first = request(server.address(), None).await;
    assert!(starts_with_status(&first, "HTTP/1.1 200 OK"));
    assert!(String::from_utf8_lossy(&first).contains("cache-control: private, no-store"));
    let observed = fixture.observed().await;
    assert_eq!(observed.len(), 3);
    assert!(!observed[0].authorization_present);
    assert!(observed[1].authorization_present);
    assert!(observed[2].authorization_present);
    assert_eq!(token_request_count(&observed), 1);

    let cached = request(server.address(), None).await;
    assert!(starts_with_status(&cached, "HTTP/1.1 200 OK"));
    let observed = fixture.observed().await;
    assert_eq!(observed.len(), 4);
    assert_eq!(token_request_count(&observed), 1);
}

#[tokio::test]
async fn rejected_cached_token_is_refreshed_once() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig {
        private: true,
        ..RegistryFixtureConfig::default()
    })
    .await;
    register_private_manifest(&fixture).await;
    fixture
        .configure_token_service(HeaderValue::from_static(TOKEN_BASIC))
        .await;
    fixture.issue_token("first-token", 120).await;
    fixture.issue_token("second-token", 120).await;
    fixture
        .set_expected_authorization(HeaderValue::from_static("Bearer first-token"))
        .await;
    let config = config(&fixture.origin(), true);
    let server = start_proxy(&config).await;
    assert!(starts_with_status(
        &request(server.address(), None).await,
        "HTTP/1.1 200 OK"
    ));

    fixture
        .set_expected_authorization(HeaderValue::from_static("Bearer second-token"))
        .await;
    let refreshed = request(server.address(), None).await;
    assert!(starts_with_status(&refreshed, "HTTP/1.1 200 OK"));
    let observed = fixture.observed().await;
    assert_eq!(token_request_count(&observed), 2);
    assert_eq!(observed.len(), 6);
}

#[tokio::test]
async fn caller_authorization_is_forwarded_without_broker_substitution() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig {
        private: true,
        expected_authorization: Some(HeaderValue::from_static("Bearer caller-canary")),
        ..RegistryFixtureConfig::default()
    })
    .await;
    register_private_manifest(&fixture).await;
    let config = config(&fixture.origin(), true);
    let server = start_proxy(&config).await;

    let response = request(server.address(), Some("Bearer caller-canary")).await;
    assert!(starts_with_status(&response, "HTTP/1.1 200 OK"));
    let observed = fixture.observed().await;
    assert_eq!(observed.len(), 1);
    assert!(observed[0].authorization_present);
    assert_eq!(token_request_count(&observed), 0);
}

#[tokio::test]
async fn failed_or_out_of_repository_authentication_is_safe() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig {
        private: true,
        ..RegistryFixtureConfig::default()
    })
    .await;
    register_private_manifest(&fixture).await;
    let config = config(&fixture.origin(), true);
    assert!(!format!("{config:?}").contains("secret"));
    let server = start_proxy(&config).await;

    let unauthenticated = request(server.address(), None).await;
    let text = String::from_utf8_lossy(&unauthenticated);
    assert!(text.starts_with("HTTP/1.1 401 Unauthorized"));
    assert!(text.contains("www-authenticate: Bearer realm="));
    assert!(!text.contains("secret"));

    let mut stream = tokio::net::TcpStream::connect(server.address())
        .await
        .unwrap();
    stream
        .write_all(
            b"GET /v2/acme/other/manifests/demo HTTP/1.1\r\nHost: proxy\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    assert!(starts_with_status(&response, "HTTP/1.1 404 Not Found"));

    let mut stream = tokio::net::TcpStream::connect(server.address())
        .await
        .unwrap();
    stream
        .write_all(
            b"GET /v2/acme/private/manifests/demo HTTP/1.1\r\nHost: proxy\r\nAuthorization: Bearer one\r\nAuthorization: Bearer two\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut duplicate = Vec::new();
    stream.read_to_end(&mut duplicate).await.unwrap();
    assert!(starts_with_status(&duplicate, "HTTP/1.1 400 Bad Request"));
    assert_eq!(fixture.observed().await.len(), 1);
}
