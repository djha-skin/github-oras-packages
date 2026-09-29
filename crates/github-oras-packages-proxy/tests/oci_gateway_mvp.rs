//! End-to-end coverage for standard OCI Distribution reads and writes.

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
const PUSH_AUTHORIZATION: &str = "Bearer fixture-push-token";

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
    request_with_body(address, method, path, &[], b"").await
}

async fn request_with_body(
    address: std::net::SocketAddr,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: proxy\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
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

    let blob_head = request(
        server.address(),
        Method::HEAD,
        "/v2/acme/fixture/blobs/sha256:69c998b199efb04029017e6205c766e087757b15c584a161db3c83838981f9e4",
    )
    .await;
    assert!(String::from_utf8_lossy(&blob_head).starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(body(&blob_head).is_empty());

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
    assert!(
        observed
            .iter()
            .any(|request| request.method == Method::HEAD && request.path.ends_with(BLOB_DIGEST))
    );
    server.shutdown().await;
}

#[tokio::test]
async fn streams_oci_uploads_and_manifest_publication_to_the_fixed_repository() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig {
        private: true,
        expected_authorization: Some(hyper::header::HeaderValue::from_static(PUSH_AUTHORIZATION)),
        ..RegistryFixtureConfig::default()
    })
    .await;
    let base = format!("/v2/{REPOSITORY}/blobs/uploads");
    let start_location = format!(
        "{}{base}/session-123?_state=fixture-state",
        fixture.origin()
    );
    fixture
        .register(
            Resource::UploadStart {
                repository: REPOSITORY.to_owned(),
            },
            ResourceResponse::Status {
                status: hyper::StatusCode::ACCEPTED,
                headers: vec![(
                    hyper::header::LOCATION,
                    hyper::header::HeaderValue::from_str(&start_location).unwrap(),
                )],
            },
        )
        .await;
    fixture
        .register(
            Resource::Upload {
                repository: REPOSITORY.to_owned(),
                upload_id: "session-123".to_owned(),
            },
            ResourceResponse::Status {
                status: hyper::StatusCode::ACCEPTED,
                headers: vec![(
                    hyper::header::LOCATION,
                    hyper::header::HeaderValue::from_static(
                        "/v2/acme/fixture/blobs/uploads/session-123?_state=fixture-state",
                    ),
                )],
            },
        )
        .await;
    fixture
        .register(
            Resource::Manifest {
                repository: REPOSITORY.to_owned(),
                reference: "published".to_owned(),
            },
            ResourceResponse::Status {
                status: hyper::StatusCode::CREATED,
                headers: vec![(
                    hyper::header::HeaderName::from_static("docker-content-digest"),
                    hyper::header::HeaderValue::from_static(BLOB_DIGEST),
                )],
            },
        )
        .await;

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

    let foreign_mount = request(
        server.address(),
        Method::POST,
        &format!("{base}/?mount={BLOB_DIGEST}&from=another/private-repo"),
    )
    .await;
    assert!(String::from_utf8_lossy(&foreign_mount).starts_with("HTTP/1.1 404 Not Found\r\n"));

    let started = request_with_body(
        server.address(),
        Method::POST,
        &format!("{base}/"),
        &[("Authorization", PUSH_AUTHORIZATION)],
        b"",
    )
    .await;
    assert!(String::from_utf8_lossy(&started).starts_with("HTTP/1.1 202 Accepted\r\n"));
    let started_text = String::from_utf8_lossy(&started);
    assert!(started_text.contains(&format!(
        "location: {base}/session-123?_state=fixture-state"
    )));
    assert!(!started_text.contains(&fixture.origin()));

    let patched = request_with_body(
        server.address(),
        Method::PATCH,
        &format!("{base}/session-123?_state=fixture-state"),
        &[
            ("Content-Type", "application/octet-stream"),
            ("Authorization", PUSH_AUTHORIZATION),
        ],
        b"chunk",
    )
    .await;
    assert!(
        String::from_utf8_lossy(&patched).starts_with("HTTP/1.1 202 Accepted\r\n"),
        "unexpected chunked upload response: {}",
        String::from_utf8_lossy(&patched)
    );

    let completed = request_with_body(
        server.address(),
        Method::PUT,
        &format!("{base}/session-123?_state=fixture-state&digest={BLOB_DIGEST}"),
        &[
            ("Content-Type", "application/octet-stream"),
            ("Authorization", PUSH_AUTHORIZATION),
        ],
        b"blobdata",
    )
    .await;
    assert!(String::from_utf8_lossy(&completed).starts_with("HTTP/1.1 202 Accepted\r\n"));

    let published = request_with_body(
        server.address(),
        Method::PUT,
        "/v2/acme/fixture/manifests/published",
        &[
            ("Content-Type", "application/vnd.oci.image.manifest.v1+json"),
            ("Authorization", PUSH_AUTHORIZATION),
        ],
        MANIFEST,
    )
    .await;
    assert!(String::from_utf8_lossy(&published).starts_with("HTTP/1.1 201 Created\r\n"));

    let observed = fixture.observed().await;
    assert!(observed.iter().any(|request| {
        request.method == Method::PATCH && request.body_bytes == b"chunk".len()
    }));
    assert!(observed.iter().any(|request| {
        request.method == Method::PUT
            && request.path.contains("digest=sha256:")
            && request.body_bytes == b"blobdata".len()
    }));
    assert!(observed.iter().any(|request| {
        request.method == Method::PUT
            && request.path.ends_with("/manifests/published")
            && request.body_bytes == MANIFEST.len()
    }));
    assert!(
        observed
            .iter()
            .all(|request| request.path.starts_with("/v2/acme/fixture/"))
    );
    assert!(
        observed
            .iter()
            .all(|request| !request.path.contains("another/private-repo"))
    );
    assert!(
        observed
            .iter()
            .filter(|request| matches!(request.method, Method::POST | Method::PATCH | Method::PUT))
            .all(|request| request.authorization_present)
    );
    server.shutdown().await;
}
