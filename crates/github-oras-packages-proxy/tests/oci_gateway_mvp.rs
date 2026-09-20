//! End-to-end coverage for the standard OCI Distribution read gateway.

mod support;

use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use github_oras_packages_proxy::{
    config::Config, oci::OciClient, proxy, routing::ValidatedRepository, server::Server,
};
use hyper::Method;
use support::registry::{RegistryFixture, RegistryFixtureConfig, Resource, ResourceResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const REPOSITORY: &str = "acme/fixture";
const MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.empty.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":0},"layers":[]}"#;
const BLOB_DIGEST: &str = "sha256:69c998b199efb04029017e6205c766e087757b15c584a161db3c83838981f9e4";
const BLOB: &[u8] = b"<html><body>capturepkg</body></html>\n";

fn config(upstream: &str) -> Config {
    Config::from_maps(
        &BTreeMap::new(),
        &BTreeMap::from([
            ("LISTEN_ADDR".to_owned(), "127.0.0.1:0".to_owned()),
            ("UPSTREAM".to_owned(), upstream.to_owned()),
            ("REPOSITORY".to_owned(), REPOSITORY.to_owned()),
            ("ALLOWED_HOSTS".to_owned(), "127.0.0.1".to_owned()),
            ("ALLOW_INSECURE_LOOPBACK".to_owned(), "true".to_owned()),
        ]),
    )
    .unwrap()
}

async fn register(fixture: &RegistryFixture) {
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
    fixture
        .register(
            Resource::Blob {
                repository: REPOSITORY.to_owned(),
                digest: BLOB_DIGEST.to_owned(),
            },
            ResourceResponse::Body {
                body: Bytes::from_static(BLOB),
                content_type: "text/html; charset=utf-8",
            },
        )
        .await;
}

async fn request(address: std::net::SocketAddr, method: Method, path: &str) -> Vec<u8> {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!("{method} {path} HTTP/1.1\r\nHost: proxy\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    response
}

fn body(response: &[u8]) -> &[u8] {
    let start = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap()
        + 4;
    &response[start..]
}

#[tokio::test]
async fn proxies_standard_oci_version_manifests_and_blobs() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig::default()).await;
    register(&fixture).await;
    let config = config(&fixture.origin());
    let client = Arc::new(OciClient::new(&config).unwrap());
    let repository = ValidatedRepository::parse(REPOSITORY).unwrap();
    let limits = config.inbound_limits();
    let handler = {
        let client = Arc::clone(&client);
        let repository = repository.clone();
        move |request| {
            let client = Arc::clone(&client);
            let repository = repository.clone();
            async move { proxy::handle_gateway(request, client, repository, limits).await }
        }
    };
    let server = Server::start(&config, handler).await.unwrap();

    let version = request(server.address(), Method::GET, "/v2/").await;
    assert!(String::from_utf8_lossy(&version).starts_with("HTTP/1.1 200 OK\r\n"));

    let manifest = request(
        server.address(),
        Method::GET,
        "/v2/acme/fixture/manifests/demo",
    )
    .await;
    assert!(String::from_utf8_lossy(&manifest).starts_with("HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&manifest), MANIFEST);

    let head = request(
        server.address(),
        Method::HEAD,
        "/v2/acme/fixture/manifests/demo",
    )
    .await;
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(body(&head).is_empty());

    let blob = request(
        server.address(),
        Method::GET,
        "/v2/acme/fixture/blobs/sha256:69c998b199efb04029017e6205c766e087757b15c584a161db3c83838981f9e4",
    )
    .await;
    assert!(String::from_utf8_lossy(&blob).starts_with("HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&blob), BLOB);

    let other = request(
        server.address(),
        Method::GET,
        "/v2/acme/other/manifests/demo",
    )
    .await;
    assert!(String::from_utf8_lossy(&other).starts_with("HTTP/1.1 404 Not Found\r\n"));

    let observed = fixture.observed().await;
    assert!(
        observed
            .iter()
            .all(|request| request.path.starts_with("/v2/acme/fixture/"))
    );
    server.shutdown().await;
}
