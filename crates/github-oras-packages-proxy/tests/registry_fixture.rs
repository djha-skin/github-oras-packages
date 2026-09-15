mod support;

use std::time::Duration;

use bytes::Bytes;
use hyper::{
    Method, StatusCode,
    header::{self, HeaderValue},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use support::registry::{
    ObservedRequest, RegistryFixture, RegistryFixtureConfig, RequestExpectation, Resource,
    ResourceResponse,
};

#[test]
fn registry_fixture_support_module_is_available() {
    let _ = RegistryFixtureConfig::default();
}

#[tokio::test]
async fn private_fixture_challenges_then_accepts_exact_authorization() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig {
        private: true,
        expected_authorization: Some(HeaderValue::from_static("Bearer test-canary")),
        ..RegistryFixtureConfig::default()
    })
    .await;
    fixture
        .register(
            Resource::Manifest {
                repository: "acme/private".into(),
                reference: "oras-packages.v1".into(),
            },
            ResourceResponse::Body {
                body: Bytes::from_static(b"manifest"),
                content_type: "application/vnd.oci.image.manifest.v1+json",
            },
        )
        .await;

    let without_auth = raw_request(
        &fixture,
        "GET /v2/acme/private/manifests/oras-packages.v1 HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(without_auth.starts_with("HTTP/1.1 401 Unauthorized"));
    assert!(without_auth.contains("www-authenticate: Bearer realm="));

    let with_auth = raw_request(
        &fixture,
        "GET /v2/acme/private/manifests/oras-packages.v1 HTTP/1.1\r\nHost: fixture\r\nAuthorization: Bearer test-canary\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(with_auth.starts_with("HTTP/1.1 200 OK"));
    assert!(with_auth.ends_with("\r\n\r\nmanifest"));

    let observed = fixture.observed().await;
    assert_eq!(observed.len(), 2);
    assert!(!observed[0].authorization_present);
    assert!(observed[1].authorization_present);
}

#[tokio::test]
async fn fixture_supports_head_range_and_etag_validation() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig::default()).await;
    fixture
        .register(
            Resource::Blob {
                repository: "acme/cache".into(),
                digest: "sha256:abc".into(),
            },
            ResourceResponse::Body {
                body: Bytes::from_static(b"01234567"),
                content_type: "application/octet-stream",
            },
        )
        .await;

    let head = raw_request(
        &fixture,
        "HEAD /v2/acme/cache/blobs/sha256:abc HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    assert!(head.contains("content-length: 8"));
    assert!(head.ends_with("\r\n\r\n"));

    let range = raw_request(
        &fixture,
        "GET /v2/acme/cache/blobs/sha256:abc HTTP/1.1\r\nHost: fixture\r\nRange: bytes=2-5\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(range.starts_with("HTTP/1.1 206 Partial Content"));
    assert!(range.contains("content-range: bytes 2-5/8"));
    assert!(range.ends_with("\r\n\r\n2345"));

    let not_modified = raw_request(
        &fixture,
        "GET /v2/acme/cache/blobs/sha256:abc HTTP/1.1\r\nHost: fixture\r\nIf-None-Match: \"fixture-etag\"\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(not_modified.starts_with("HTTP/1.1 304 Not Modified"));
}

#[tokio::test]
async fn fixture_can_delay_large_streams_and_advertise_truncation() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig {
        large_stream_delay: Some(Duration::from_millis(1)),
        ..RegistryFixtureConfig::default()
    })
    .await;
    fixture
        .register(
            Resource::Blob {
                repository: "acme/faults".into(),
                digest: "sha256:large".into(),
            },
            ResourceResponse::Body {
                body: Bytes::from(vec![b'x'; 512]),
                content_type: "application/octet-stream",
            },
        )
        .await;
    fixture
        .register(
            Resource::Blob {
                repository: "acme/faults".into(),
                digest: "sha256:short".into(),
            },
            ResourceResponse::Truncated {
                body: Bytes::from_static(b"short"),
                advertised_length: 100,
                content_type: "application/octet-stream",
            },
        )
        .await;

    let large = raw_request(
        &fixture,
        "GET /v2/acme/faults/blobs/sha256:large HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(large.starts_with("HTTP/1.1 200 OK"));
    assert!(large.ends_with(&format!("\r\n\r\n{}", "x".repeat(512))));

    let truncated = raw_request(
        &fixture,
        "GET /v2/acme/faults/blobs/sha256:short HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(truncated.starts_with("HTTP/1.1 200 OK"));
    assert!(truncated.contains("content-length: 100"));
    // Hyper closes the connection before emitting a body that cannot satisfy
    // the advertised Content-Length; this is the intended truncated fault.
    assert!(truncated.ends_with("\r\n\r\n"), "{truncated:?}");
}

#[tokio::test]
async fn expectation_matching_is_exact_and_observations_are_redacted() {
    let fixture = RegistryFixture::start(RegistryFixtureConfig::default()).await;
    fixture
        .register(
            Resource::Blob {
                repository: "acme/expect".into(),
                digest: "sha256:abc".into(),
            },
            ResourceResponse::Status {
                status: StatusCode::NO_CONTENT,
                headers: Vec::new(),
            },
        )
        .await;
    fixture
        .expect(
            RequestExpectation::new(Method::GET, "/v2/acme/expect/blobs/sha256:abc").with_header(
                header::AUTHORIZATION,
                HeaderValue::from_static("Bearer secret-canary"),
            ),
        )
        .await;

    let _ = raw_request(
        &fixture,
        "GET /v2/acme/expect/blobs/sha256:abc HTTP/1.1\r\nHost: fixture\r\nAuthorization: Bearer secret-canary\r\nConnection: close\r\n\r\n",
    )
    .await;
    let observed: Vec<ObservedRequest> = fixture.observed().await;
    assert_eq!(observed[0].path, "/v2/acme/expect/blobs/sha256:abc");
    assert!(observed[0].matched_expectation);
    let debug = format!("{observed:?}");
    assert!(!debug.contains("secret-canary"));
}

async fn raw_request(fixture: &RegistryFixture, request: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(fixture.address())
        .await
        .unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    String::from_utf8(response).unwrap()
}
