//! End-to-end coverage for the anonymous, fixture-backed PyPI MVP.

mod support;

use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use github_oras_packages_proxy::{
    config::Config, oci::OciClient, proxy, routing::EnabledProtocols, server::Server,
};
use hyper::{Method, header::HeaderValue};
use support::registry::{RegistryFixture, RegistryFixtureConfig, Resource, ResourceResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/corpus/");
const REPOSITORY: &str = "acme/fixture";
const LOCATOR: &str = "YWNtZS9maXh0dXJl";

fn fixture(relative: &str) -> Bytes {
    Bytes::from(std::fs::read(format!("{CORPUS}{relative}")).expect("fixture must exist"))
}

fn config(upstream: &str) -> Config {
    Config::from_maps(
        &BTreeMap::new(),
        &BTreeMap::from([
            ("LISTEN_ADDR".to_owned(), "127.0.0.1:0".to_owned()),
            ("UPSTREAM".to_owned(), upstream.to_owned()),
            ("ALLOWED_HOSTS".to_owned(), "127.0.0.1".to_owned()),
            ("ALLOW_INSECURE_LOOPBACK".to_owned(), "true".to_owned()),
            ("ENABLED_FRONTENDS".to_owned(), "pypi".to_owned()),
        ]),
    )
    .expect("fixture configuration must validate")
}

async fn register_layout(fixture_server: &RegistryFixture, include_wheel: bool) {
    fixture_server
        .register(
            Resource::Manifest {
                repository: REPOSITORY.to_owned(),
                reference: "oras-packages.v1".to_owned(),
            },
            ResourceResponse::Body {
                body: fixture("manifest.json"),
                content_type: "application/vnd.oci.image.manifest.v1+json",
            },
        )
        .await;
    fixture_server
        .register(
            Resource::Blob {
                repository: REPOSITORY.to_owned(),
                digest: "sha256:e0b3f01577ab7157bd76e3a36f16e9c242d25d2bd9d10551ef644d417caa9b8d"
                    .to_owned(),
            },
            ResourceResponse::Body {
                body: fixture("config.json"),
                content_type: "application/vnd.github.oras-packages.layout.v1+json",
            },
        )
        .await;
    fixture_server
        .register(
            Resource::Blob {
                repository: REPOSITORY.to_owned(),
                digest: "sha256:d6e7dfece7cb4597dff26a4b8e8cc20a671bd1d4a081c8ec465259cea522614a"
                    .to_owned(),
            },
            ResourceResponse::Body {
                body: fixture("route-map.json"),
                content_type: "application/vnd.github.oras-packages.route-map.v1+json",
            },
        )
        .await;
    fixture_server
        .register(
            Resource::Blob {
                repository: REPOSITORY.to_owned(),
                digest: "sha256:6a6feac08bdc59252504816d8631bd2b18b5490d34686027b3982a0a92e3237c"
                    .to_owned(),
            },
            ResourceResponse::Body {
                body: fixture(
                    "blobs/sha256/6a6feac08bdc59252504816d8631bd2b18b5490d34686027b3982a0a92e3237c",
                ),
                content_type: "text/html; charset=utf-8",
            },
        )
        .await;
    fixture_server
        .register(
            Resource::Blob {
                repository: REPOSITORY.to_owned(),
                digest: "sha256:9fc3b9d35c987c6b90e99016c555dd49770f48053f80cce69621834dfe877524"
                    .to_owned(),
            },
            ResourceResponse::Body {
                body: fixture(
                    "blobs/sha256/9fc3b9d35c987c6b90e99016c555dd49770f48053f80cce69621834dfe877524",
                ),
                content_type: "text/html; charset=utf-8",
            },
        )
        .await;
    if include_wheel {
        fixture_server
            .register(
                Resource::Blob {
                    repository: REPOSITORY.to_owned(),
                    digest:
                        "sha256:cf091ce5ee9278d9ef9034cbd7e19acb2cdc13b2bc6cb1c6bd37c466e9bce836"
                            .to_owned(),
                },
                ResourceResponse::Body {
                    body: fixture("packages/capturepkg-1.0.0-py3-none-any.whl"),
                    content_type: "application/octet-stream",
                },
            )
            .await;
    }
}

async fn request(
    address: std::net::SocketAddr,
    method: Method,
    target: &str,
    authorization: Option<&str>,
) -> Vec<u8> {
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("proxy must accept loopback connections");
    let auth = authorization.map_or(String::new(), |value| format!("Authorization: {value}\r\n"));
    let request =
        format!("{method} {target} HTTP/1.1\r\nHost: proxy\r\n{auth}Connection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("request must be written");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("response must be readable");
    response
}

fn response_text(response: &[u8]) -> String {
    String::from_utf8(response.to_vec()).expect("fixture response is UTF-8")
}

fn response_headers(response: &[u8]) -> String {
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response must have a header terminator");
    response_text(&response[..end + 4])
}

fn response_body(response: &[u8]) -> &[u8] {
    let start = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response must have a header terminator")
        + 4;
    &response[start..]
}

#[tokio::test]
async fn serves_prebuilt_pypi_html_and_wheel_from_one_exact_repository() {
    let registry = RegistryFixture::start(RegistryFixtureConfig::default()).await;
    register_layout(&registry, true).await;
    let config = config(&registry.origin());
    let limits = config.inbound_limits();
    let client = Arc::new(OciClient::new(&config).expect("OCI client must validate"));
    let handler = {
        let client = Arc::clone(&client);
        move |request| {
            let client = Arc::clone(&client);
            async move { proxy::handle(request, client, EnabledProtocols::all(), limits).await }
        }
    };
    // Keep the closure independent of the registry handle after configuration;
    // only the fixed origin is retained by the client.
    let server = Server::start(&config, handler)
        .await
        .expect("proxy must bind a loopback port");

    let root = request(
        server.address(),
        Method::GET,
        &format!("/r/v1/{LOCATOR}/pypi/simple/"),
        None,
    )
    .await;
    let root_text = response_text(&root);
    assert!(root_text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(
        root_text.ends_with("capturepkg</a></body></html>\n"),
        "{root_text:?}"
    );

    let project = request(
        server.address(),
        Method::GET,
        &format!("/r/v1/{LOCATOR}/pypi/simple/capturepkg/"),
        None,
    )
    .await;
    let project_text = response_text(&project);
    assert!(project_text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(
        project_text
            .contains("sha256=cf091ce5ee9278d9ef9034cbd7e19acb2cdc13b2bc6cb1c6bd37c466e9bce836")
    );

    let wheel = request(
        server.address(),
        Method::GET,
        &format!("/r/v1/{LOCATOR}/pypi/packages/capturepkg-1.0.0-py3-none-any.whl"),
        None,
    )
    .await;
    assert!(response_headers(&wheel).starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response_body(&wheel).starts_with(b"PK"));

    let observed = registry.observed().await;
    assert!(
        observed
            .iter()
            .all(|request| request.path.starts_with("/v2/acme/fixture/"))
    );
    assert!(
        observed
            .iter()
            .all(|request| request.path != "/v2/acme/other/")
    );
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_project_and_missing_mapped_blob_fail_safely() {
    let registry = RegistryFixture::start(RegistryFixtureConfig::default()).await;
    register_layout(&registry, false).await;
    let config = config(&registry.origin());
    let client = Arc::new(OciClient::new(&config).unwrap());
    let handler_config = config.clone();
    let handler = move |request| {
        let client = Arc::clone(&client);
        let limits = handler_config.inbound_limits();
        async move { proxy::handle(request, client, EnabledProtocols::all(), limits).await }
    };
    let server = Server::start(&config, handler).await.unwrap();

    let unknown = request(
        server.address(),
        Method::GET,
        &format!("/r/v1/{LOCATOR}/pypi/simple/unknown/"),
        None,
    )
    .await;
    let unknown_text = response_text(&unknown);
    assert!(unknown_text.starts_with("HTTP/1.1 404 Not Found\r\n"));
    assert!(unknown_text.contains("\"error\":\"not_found\""));
    assert!(!unknown_text.contains(REPOSITORY));

    let missing = request(
        server.address(),
        Method::GET,
        &format!("/r/v1/{LOCATOR}/pypi/packages/capturepkg-1.0.0-py3-none-any.whl"),
        None,
    )
    .await;
    let missing_text = response_text(&missing);
    assert!(missing_text.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
    assert!(missing_text.contains("\"error\":\"upstream_missing_content\""));
    server.shutdown().await;
}

#[tokio::test]
async fn upstream_authentication_is_forwarded_only_to_fixed_fixture() {
    let registry = RegistryFixture::start(RegistryFixtureConfig {
        private: true,
        expected_authorization: Some(HeaderValue::from_static("Bearer test-canary")),
        ..RegistryFixtureConfig::default()
    })
    .await;
    register_layout(&registry, true).await;
    let config = config(&registry.origin());
    let client = Arc::new(OciClient::new(&config).unwrap());
    let handler_config = config.clone();
    let handler = move |request| {
        let client = Arc::clone(&client);
        let limits = handler_config.inbound_limits();
        async move { proxy::handle(request, client, EnabledProtocols::all(), limits).await }
    };
    let server = Server::start(&config, handler).await.unwrap();

    let response = request(
        server.address(),
        Method::GET,
        &format!("/r/v1/{LOCATOR}/pypi/simple/"),
        Some("Bearer test-canary"),
    )
    .await;
    assert!(response_text(&response).starts_with("HTTP/1.1 200 OK\r\n"));
    let observed = registry.observed().await;
    assert!(observed.iter().all(|request| request.authorization_present));
    assert!(
        observed
            .iter()
            .all(|request| request.path.starts_with("/v2/acme/fixture/"))
    );
    assert!(!format!("{observed:?}").contains("test-canary"));
    server.shutdown().await;
}
