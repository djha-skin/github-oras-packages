//! End-to-end coverage for the standard OCI-backed autoindex read path.

mod support;

use std::{collections::BTreeMap, sync::Arc};

use github_oras_packages_proxy::{
    config::Config, oci::OciClient, proxy, routing::ValidatedRepository, server::Server,
};
use hyper::Method;
use support::registry::{RegistryFixture, RegistryFixtureConfig, Resource, ResourceResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const REPOSITORY: &str = "acme/fixture";
const MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.empty.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":0},"layers":[{"mediaType":"text/html; charset=utf-8","digest":"sha256:69c998b199efb04029017e6205c766e087757b15c584a161db3c83838981f9e4","size":37,"annotations":{"org.opencontainers.image.title":"pypi/simple/index.html","io.github.djha-skin.github-oras-packages.autoindex.visible":"true"}},{"mediaType":"application/octet-stream","digest":"sha256:d0995fbab28019f357bfaa8021396aa90224dafc0b6bda07afeeb2a83097fdd6","size":12,"annotations":{"org.opencontainers.image.title":"pypi/packages/capturepkg.whl","io.github.djha-skin.github-oras-packages.autoindex.visible":"true"}},{"mediaType":"application/octet-stream","digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1,"annotations":{"org.opencontainers.image.title":"private.bin"}}]}"#;
const INDEX_BYTES: &[u8] = b"<html><body>capturepkg</body></html>\n";
const WHEEL_BYTES: &[u8] = b"wheel bytes\n";
const INDEX_DIGEST: &str =
    "sha256:69c998b199efb04029017e6205c766e087757b15c584a161db3c83838981f9e4";
const WHEEL_DIGEST: &str =
    "sha256:d0995fbab28019f357bfaa8021396aa90224dafc0b6bda07afeeb2a83097fdd6";

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
    .expect("fixture configuration must validate")
}

async fn register_manifest(fixture: &RegistryFixture) {
    fixture
        .register(
            Resource::Manifest {
                repository: REPOSITORY.to_owned(),
                reference: "autoindex.v1".to_owned(),
            },
            ResourceResponse::Body {
                body: bytes::Bytes::from_static(MANIFEST),
                content_type: "application/vnd.oci.image.manifest.v1+json",
            },
        )
        .await;
    fixture
        .register(
            Resource::Blob {
                repository: REPOSITORY.to_owned(),
                digest: INDEX_DIGEST.to_owned(),
            },
            ResourceResponse::Body {
                body: bytes::Bytes::from_static(INDEX_BYTES),
                content_type: "text/html; charset=utf-8",
            },
        )
        .await;
    fixture
        .register(
            Resource::Blob {
                repository: REPOSITORY.to_owned(),
                digest: WHEEL_DIGEST.to_owned(),
            },
            ResourceResponse::Body {
                body: bytes::Bytes::from_static(WHEEL_BYTES),
                content_type: "application/octet-stream",
            },
        )
        .await;
}

async fn request(address: std::net::SocketAddr, method: Method, target: &str) -> Vec<u8> {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!("{method} {target} HTTP/1.1\r\nHost: proxy\r\nConnection: close\r\n\r\n")
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
async fn serves_standard_oci_objects_at_natural_autoindex_paths() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig::default()).await;
    register_manifest(&fixture).await;
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
            async move { proxy::handle_autoindex(request, client, repository, limits).await }
        }
    };
    let server = Server::start(&config, handler).await.unwrap();

    let root = request(server.address(), Method::GET, "/").await;
    assert!(String::from_utf8_lossy(&root).starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(String::from_utf8_lossy(body(&root)).contains("/pypi/"));

    let nested = request(server.address(), Method::GET, "/pypi/simple/").await;
    let nested_text = String::from_utf8_lossy(&nested);
    assert!(nested_text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(nested_text.contains("/pypi/simple/index.html"));

    let file = request(server.address(), Method::GET, "/pypi/simple/index.html").await;
    assert!(String::from_utf8_lossy(&file).starts_with("HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&file), INDEX_BYTES);

    let head = request(server.address(), Method::HEAD, "/pypi/simple/index.html").await;
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(body(&head).is_empty());

    let redirect = request(server.address(), Method::GET, "/pypi/simple").await;
    let redirect_text = String::from_utf8_lossy(&redirect);
    assert!(redirect_text.starts_with("HTTP/1.1 308 Permanent Redirect\r\n"));
    assert!(redirect_text.contains("location: /pypi/simple/\r\n"));

    let hidden = request(server.address(), Method::GET, "/private.bin").await;
    assert!(String::from_utf8_lossy(&hidden).starts_with("HTTP/1.1 404 Not Found\r\n"));

    let traversal = request(server.address(), Method::GET, "/pypi/../private.bin").await;
    assert!(String::from_utf8_lossy(&traversal).starts_with("HTTP/1.1 404 Not Found\r\n"));

    let observed = fixture.observed().await;
    assert!(
        observed
            .iter()
            .all(|request| request.path.starts_with("/v2/acme/fixture/"))
    );
    assert!(
        observed
            .iter()
            .any(|request| request.path == "/v2/acme/fixture/manifests/autoindex.v1")
    );
    assert!(
        observed
            .iter()
            .any(|request| request.path == format!("/v2/acme/fixture/blobs/{INDEX_DIGEST}"))
    );
    server.shutdown().await;
}
