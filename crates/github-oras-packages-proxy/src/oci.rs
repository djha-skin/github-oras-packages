//! Fixed-origin OCI Distribution retrieval and v1 layout validation.
//!
//! This module is intentionally limited to immutable read operations needed by
//! the local PyPI MVP. It builds every request from the validated repository
//! and configured origin; callers cannot provide a registry URL, repository,
//! manifest reference, or arbitrary blob path.

use std::{collections::BTreeMap, error::Error, fmt, time::Duration};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http_body::Frame;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, Uri, body::Incoming, header};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    autoindex::{Index, TITLE_ANNOTATION, VISIBILITY_ANNOTATION, VisibleObject},
    config::Config,
    routing::ValidatedRepository,
};

const LAYOUT_REFERENCE: &str = "oras-packages.v1";
pub const AUTO_INDEX_REFERENCE: &str = "autoindex.v1";
const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG_MEDIA_TYPE: &str = "application/vnd.github.oras-packages.layout.v1+json";
const ROUTE_MAP_MEDIA_TYPE: &str = "application/vnd.github.oras-packages.route-map.v1+json";
const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

/// A descriptor selected by the validated route map.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

impl Descriptor {
    /// Creates a descriptor for tests and controlled publisher-side inputs.
    pub fn new(media_type: impl Into<String>, digest: impl Into<String>, size: u64) -> Self {
        Self {
            media_type: media_type.into(),
            digest: digest.into(),
            size,
            annotations: BTreeMap::new(),
        }
    }

    /// Creates a descriptor with OCI annotations.
    pub fn with_annotations(
        media_type: impl Into<String>,
        digest: impl Into<String>,
        size: u64,
        annotations: BTreeMap<String, String>,
    ) -> Self {
        Self {
            media_type: media_type.into(),
            digest: digest.into(),
            size,
            annotations,
        }
    }

    /// Returns the descriptor media type.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Returns the immutable `sha256:` digest.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Creates a descriptor for a known blob digest and response size.
    pub fn for_blob(media_type: impl Into<String>, digest: impl Into<String>, size: u64) -> Self {
        Self::new(media_type, digest, size)
    }

    /// Returns the advertised byte length.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Returns the optional OCI descriptor annotations.
    pub fn annotations(&self) -> &BTreeMap<String, String> {
        &self.annotations
    }

    /// Returns the optional materialization title.
    pub fn title(&self) -> Option<&str> {
        self.annotations.get(TITLE_ANNOTATION).map(String::as_str)
    }

    /// Reports whether this descriptor is explicitly autoindex-visible.
    pub fn is_autoindex_visible(&self) -> bool {
        self.annotations
            .get(VISIBILITY_ANNOTATION)
            .is_some_and(|value| value == "true")
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Manifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u8,
    #[serde(rename = "mediaType")]
    media_type: String,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Clone, Debug)]
/// A validated standard OCI manifest projected into autoindex paths.
pub struct AutoindexSnapshot {
    index: Index,
}

impl AutoindexSnapshot {
    /// Resolves one exact human-readable file path.
    pub fn object(&self, path: &str) -> Option<&VisibleObject> {
        self.index.object(path)
    }

    /// Reports whether a projected directory exists.
    pub fn has_directory(&self, directory: &str) -> Result<bool, OciError> {
        self.index
            .has_directory(directory)
            .map_err(|_| OciError::InvalidLayout)
    }

    /// Returns deterministic direct children under an autoindex directory.
    pub fn children(
        &self,
        directory: &str,
    ) -> Result<Vec<crate::autoindex::DirectoryEntry>, OciError> {
        self.index
            .children(directory)
            .map_err(|_| OciError::InvalidLayout)
    }

    /// Returns the number of visible objects.
    pub fn object_count(&self) -> usize {
        self.index.objects().count()
    }

    /// Returns all visible objects in path order.
    pub fn objects(&self) -> impl Iterator<Item = &VisibleObject> {
        self.index.objects()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteMap {
    version: u8,
    routes: Vec<RouteEntry>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteEntry {
    protocol: String,
    path: String,
    descriptor: Descriptor,
}

/// A fully validated immutable v1 snapshot.
#[derive(Clone, Debug)]
pub struct Snapshot {
    routes: BTreeMap<(String, String), Descriptor>,
}

impl Snapshot {
    /// Resolves one exact protocol/path pair from the map.
    pub fn descriptor(&self, protocol: &str, path: &str) -> Option<&Descriptor> {
        self.routes.get(&(protocol.to_owned(), path.to_owned()))
    }

    /// Returns the number of validated route entries.
    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    /// Returns all validated routes for controlled fixture seeding and tests.
    pub fn routes(&self) -> impl Iterator<Item = (&str, &str, &Descriptor)> {
        self.routes
            .iter()
            .map(|((protocol, path), descriptor)| (protocol.as_str(), path.as_str(), descriptor))
    }
}

/// A response body that preserves upstream backpressure while checking one OCI descriptor.
pub type BlobBody = http_body_util::combinators::UnsyncBoxBody<Bytes, Box<dyn Error + Send + Sync>>;

/// A bounded fixed-origin OCI client.
#[derive(Clone)]
pub struct OciClient {
    origin: String,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>,
    timeout: Duration,
}

impl OciClient {
    /// Creates a client from validated runtime configuration.
    pub fn new(config: &Config) -> Result<Self, OciError> {
        let origin = config.upstream().as_str();
        let mut builder = Client::builder(TokioExecutor::new());
        builder.pool_idle_timeout(Duration::from_secs(30));
        Ok(Self {
            origin,
            client: builder.build_http(),
            timeout: config.request_timeout(),
        })
    }

    /// Fetches and validates the legacy fixed `oras-packages.v1` snapshot.
    pub async fn snapshot(
        &self,
        repository: &ValidatedRepository,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<Snapshot, OciError> {
        let manifest = self
            .get_json(
                repository,
                LAYOUT_REFERENCE,
                MANIFEST_MEDIA_TYPE,
                authorization,
            )
            .await?;
        let manifest: Manifest = parse_json(&manifest)?;
        validate_manifest(&manifest)?;
        let config = self
            .get_blob_bytes(repository, &manifest.config, authorization)
            .await?;
        verify_descriptor(&manifest.config, &config)?;
        validate_config(&config)?;
        let map_bytes = self
            .get_blob_bytes(repository, &manifest.layers[0], authorization)
            .await?;
        verify_descriptor(&manifest.layers[0], &map_bytes)?;
        parse_route_map(&map_bytes)
    }

    /// Fetches one manifest as bounded metadata bytes.
    pub async fn manifest(
        &self,
        repository: &ValidatedRepository,
        reference: &str,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<Bytes, OciError> {
        if reference.is_empty()
            || reference.len() > 256
            || !reference.is_ascii()
            || reference.contains(['/', '?', '#', '%', '\\'])
        {
            return Err(OciError::InvalidDescriptor);
        }
        self.get_json(repository, reference, MANIFEST_MEDIA_TYPE, authorization)
            .await
    }

    /// Fetches one descriptor-bound blob without buffering its artifact bytes.
    pub async fn blob_by_digest(
        &self,
        repository: &ValidatedRepository,
        digest: &str,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<(BlobBody, u64, String), OciError> {
        let digest = valid_digest(digest)?;
        let response = self
            .request_response(
                repository,
                format!("/blobs/{digest}"),
                "application/octet-stream",
                authorization,
            )
            .await?;
        let size = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|size| *size <= MAX_ARTIFACT_BYTES)
            .ok_or(OciError::InvalidDescriptor)?;
        let media_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let descriptor = Descriptor::new(media_type.clone(), digest, size);
        Ok((
            verified_body(response.into_body(), &descriptor),
            size,
            media_type,
        ))
    }

    pub async fn blob(
        &self,
        repository: &ValidatedRepository,
        descriptor: &Descriptor,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<BlobBody, OciError> {
        validate_descriptor(descriptor)?;
        let digest = valid_digest(&descriptor.digest)?;
        let response = self
            .request_response(
                repository,
                format!("/blobs/{digest}"),
                &descriptor.media_type,
                authorization,
            )
            .await?;
        Ok(verified_body(response.into_body(), descriptor))
    }

    async fn get_json(
        &self,
        repository: &ValidatedRepository,
        reference: &str,
        accept: &str,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<Bytes, OciError> {
        let response = self
            .request_response(
                repository,
                format!("/manifests/{reference}"),
                accept,
                authorization,
            )
            .await?;
        tokio::time::timeout(
            self.timeout,
            http_body_util::Limited::new(response.into_body(), MAX_METADATA_BYTES).collect(),
        )
        .await
        .map_err(|_| OciError::Timeout)?
        .map_err(|_| OciError::InvalidLayout)
        .map(|body| body.to_bytes())
    }

    /// Fetches and validates the standard autoindex manifest projection.
    pub async fn autoindex_snapshot(
        &self,
        repository: &ValidatedRepository,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<AutoindexSnapshot, OciError> {
        let manifest = self
            .get_json(
                repository,
                AUTO_INDEX_REFERENCE,
                MANIFEST_MEDIA_TYPE,
                authorization,
            )
            .await?;
        parse_autoindex_manifest(&manifest)
    }

    async fn get_blob_bytes(
        &self,
        repository: &ValidatedRepository,
        descriptor: &Descriptor,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<Bytes, OciError> {
        validate_descriptor(descriptor)?;
        let response = self
            .request_response(
                repository,
                format!("/blobs/{}", valid_digest(&descriptor.digest)?),
                &descriptor.media_type,
                authorization,
            )
            .await?;
        let limit = descriptor
            .size
            .checked_add(1)
            .filter(|size| *size <= MAX_ARTIFACT_BYTES + 1)
            .ok_or(OciError::InvalidDescriptor)?;
        let bytes = tokio::time::timeout(
            self.timeout,
            http_body_util::Limited::new(response.into_body(), limit as usize).collect(),
        )
        .await
        .map_err(|_| OciError::Timeout)?
        .map_err(|_| OciError::InvalidDescriptor)?
        .to_bytes();
        verify_descriptor(descriptor, &bytes)?;
        Ok(bytes)
    }

    async fn request_response(
        &self,
        repository: &ValidatedRepository,
        suffix: String,
        accept: &str,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<hyper::Response<Incoming>, OciError> {
        let uri: Uri = format!("{}/v2/{}{suffix}", self.origin, repository.as_str())
            .parse()
            .map_err(|_| OciError::Configuration)?;
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::ACCEPT, accept);
        if let Some(authorization) = authorization {
            builder = builder.header(header::AUTHORIZATION, authorization);
        }
        let request = builder
            .body(Full::new(Bytes::new()))
            .map_err(|_| OciError::Configuration)?;
        let response = tokio::time::timeout(self.timeout, self.client.request(request))
            .await
            .map_err(|_| OciError::Timeout)?
            .map_err(|_| OciError::Transport)?;
        classify_response(&response)?;
        Ok(response)
    }
}

fn classify_response(response: &hyper::Response<Incoming>) -> Result<(), OciError> {
    if response.status() == StatusCode::OK {
        return Ok(());
    }
    let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
    let challenges = response
        .headers()
        .get_all(header::WWW_AUTHENTICATE)
        .iter()
        .cloned()
        .collect();
    Err(match response.status() {
        StatusCode::NOT_FOUND => OciError::NotFound,
        StatusCode::UNAUTHORIZED => OciError::Unauthorized { challenges },
        StatusCode::FORBIDDEN => OciError::Forbidden,
        StatusCode::TOO_MANY_REQUESTS => OciError::RateLimited { retry_after },
        status if status.is_server_error() => OciError::Server {
            temporary: status == StatusCode::SERVICE_UNAVAILABLE,
            retry_after,
        },
        _ => OciError::Upstream,
    })
}

fn verified_body(body: Incoming, descriptor: &Descriptor) -> BlobBody {
    let state = std::sync::Arc::new(std::sync::Mutex::new(Verifier {
        hasher: Sha256::new(),
        size: 0,
        failed: false,
    }));
    let scan_state = std::sync::Arc::clone(&state);
    let expected_size = descriptor.size;
    let expected_digest = descriptor.digest.clone();
    let stream = body
        .into_data_stream()
        .scan(scan_state, move |state, item| {
            let state = std::sync::Arc::clone(state);
            async move {
                let mut verifier = state.lock().expect("verifier lock is not poisoned");
                match item {
                    Ok(bytes) => {
                        let next_size = verifier.size.saturating_add(bytes.len() as u64);
                        if verifier.failed || next_size > expected_size {
                            verifier.failed = true;
                            Some(Err(stream_error(OciError::InvalidDescriptor)))
                        } else {
                            verifier.hasher.update(&bytes);
                            verifier.size = next_size;
                            Some(Ok(bytes))
                        }
                    }
                    Err(_) => {
                        verifier.failed = true;
                        Some(Err(stream_error(OciError::Transport)))
                    }
                }
            }
        });
    let final_state = std::sync::Arc::clone(&state);
    let final_check = stream::once(async move {
        let verifier = final_state.lock().expect("verifier lock is not poisoned");
        if verifier.failed || verifier.size != expected_size {
            return Some(Err(stream_error(OciError::InvalidDescriptor)));
        }
        let actual = format!("sha256:{:x}", verifier.hasher.clone().finalize());
        if actual != expected_digest {
            return Some(Err(stream_error(OciError::InvalidDescriptor)));
        }
        None
    });
    http_body_util::StreamBody::new(
        stream
            .map(|item| item.map(Frame::data))
            .chain(final_check.filter_map(|item| async move { item })),
    )
    .boxed_unsync()
}

struct Verifier {
    hasher: Sha256,
    size: u64,
    failed: bool,
}

fn stream_error(error: OciError) -> Box<dyn Error + Send + Sync> {
    Box::new(error)
}

/// A credential-safe OCI failure classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OciError {
    /// Runtime configuration cannot form a safe request.
    Configuration,
    /// The fixed origin timed out.
    Timeout,
    /// The fixed origin could not be reached or returned an invalid body.
    Transport,
    /// The requested OCI object was absent.
    NotFound,
    /// The origin requires credentials.
    Unauthorized {
        /// Validated challenges to relay to the package client.
        challenges: Vec<hyper::header::HeaderValue>,
    },
    /// The origin rejected access.
    Forbidden,
    /// The origin imposed a request limit.
    RateLimited {
        /// A candidate Retry-After value.
        retry_after: Option<hyper::header::HeaderValue>,
    },
    /// The origin returned a server failure.
    Server {
        /// Whether clients may retry the operation.
        temporary: bool,
        /// A candidate Retry-After value.
        retry_after: Option<hyper::header::HeaderValue>,
    },
    /// The origin returned another failure.
    Upstream,
    /// A manifest/config/path projection violated the OCI contract.
    InvalidLayout,
    /// A descriptor digest or size was invalid.
    InvalidDescriptor,
}

impl fmt::Display for OciError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Configuration => "configuration",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::NotFound => "not_found",
            Self::Unauthorized { .. } => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::RateLimited { .. } => "rate_limited",
            Self::Server { .. } => "server",
            Self::Upstream => "upstream",
            Self::InvalidLayout => "invalid_layout",
            Self::InvalidDescriptor => "invalid_descriptor",
        })
    }
}

impl std::error::Error for OciError {}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, OciError> {
    serde_json::from_slice(bytes).map_err(|_| OciError::InvalidLayout)
}

fn validate_manifest(manifest: &Manifest) -> Result<(), OciError> {
    if manifest.schema_version != 2
        || manifest.media_type != MANIFEST_MEDIA_TYPE
        || manifest.layers.len() != 1
        || manifest.config.media_type != CONFIG_MEDIA_TYPE
        || manifest.layers[0].media_type != ROUTE_MAP_MEDIA_TYPE
        || manifest.config.digest == manifest.layers[0].digest
    {
        return Err(OciError::InvalidLayout);
    }
    validate_descriptor(&manifest.config)?;
    validate_descriptor(&manifest.layers[0])
}

fn validate_config(bytes: &[u8]) -> Result<(), OciError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LayoutConfig {
        kind: String,
        version: u8,
    }
    let config: LayoutConfig = parse_json(bytes)?;
    if config.kind == "github-oras-packages-layout" && config.version == 1 {
        Ok(())
    } else {
        Err(OciError::InvalidLayout)
    }
}

/// Parses a standard OCI manifest and projects explicitly visible layers.
///
/// This parser does not fetch or inspect package-manager metadata. A visible
/// descriptor must carry both the namespaced visibility annotation and a safe
/// OCI title; all other descriptors are intentionally ignored by autoindex.
pub fn parse_autoindex_manifest(bytes: &[u8]) -> Result<AutoindexSnapshot, OciError> {
    let manifest: Manifest = parse_json(bytes)?;
    validate_standard_manifest(&manifest)?;
    let mut index = Index::default();
    for descriptor in &manifest.layers {
        if !descriptor.is_autoindex_visible() {
            continue;
        }
        let Some(title) = descriptor.title() else {
            return Err(OciError::InvalidLayout);
        };
        validate_descriptor(descriptor)?;
        let object = VisibleObject::new(
            title,
            descriptor.digest.clone(),
            descriptor.media_type.clone(),
            descriptor.size,
        )
        .map_err(|_| OciError::InvalidLayout)?;
        index.insert(object).map_err(|_| OciError::InvalidLayout)?;
    }
    Ok(AutoindexSnapshot { index })
}

fn validate_standard_manifest(manifest: &Manifest) -> Result<(), OciError> {
    if manifest.schema_version != 2 || manifest.media_type != MANIFEST_MEDIA_TYPE {
        return Err(OciError::InvalidLayout);
    }
    validate_descriptor(&manifest.config)
}

fn parse_route_map(bytes: &[u8]) -> Result<Snapshot, OciError> {
    let map: RouteMap = parse_json(bytes)?;
    if map.version != 1 || map.routes.is_empty() {
        return Err(OciError::InvalidLayout);
    }
    let mut routes = BTreeMap::new();
    for entry in map.routes {
        if !matches!(entry.protocol.as_str(), "pypi" | "rpm" | "apt" | "pacman")
            || entry.path.is_empty()
            || entry.path.starts_with('/')
            || entry.path.contains("//")
            || entry.path.contains(['\\', '%', '?', '#'])
            || !entry.path.is_ascii()
            || entry.path.bytes().any(|byte| byte.is_ascii_control())
            || entry
                .path
                .split('/')
                .filter(|segment| !segment.is_empty())
                .any(|segment| matches!(segment, "." | ".."))
            || entry.path.split('/').any(|segment| segment.is_empty()) && !entry.path.ends_with('/')
        {
            return Err(OciError::InvalidLayout);
        }
        validate_descriptor(&entry.descriptor)?;
        if routes
            .insert((entry.protocol, entry.path), entry.descriptor)
            .is_some()
        {
            return Err(OciError::InvalidLayout);
        }
    }
    Ok(Snapshot { routes })
}

fn valid_digest(value: &str) -> Result<&str, OciError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(OciError::InvalidDescriptor);
    };
    if hex.len() != 64
        || !hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        || hex.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        return Err(OciError::InvalidDescriptor);
    }
    Ok(value)
}

fn validate_descriptor(descriptor: &Descriptor) -> Result<(), OciError> {
    valid_digest(&descriptor.digest)?;
    if descriptor.size > MAX_ARTIFACT_BYTES {
        return Err(OciError::InvalidDescriptor);
    }
    let media_type = descriptor.media_type.as_bytes();
    if media_type.is_empty()
        || media_type.len() > 256
        || !descriptor.media_type.is_ascii()
        || descriptor.media_type.trim() != descriptor.media_type
        || media_type.iter().any(|byte| *byte < 0x20 || *byte == 0x7f)
    {
        return Err(OciError::InvalidDescriptor);
    }
    Ok(())
}

fn verify_descriptor(descriptor: &Descriptor, bytes: &[u8]) -> Result<(), OciError> {
    if descriptor.size != bytes.len() as u64 {
        return Err(OciError::InvalidDescriptor);
    }
    let digest = valid_digest(&descriptor.digest)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = format!("sha256:{:x}", hasher.finalize());
    if actual != digest {
        return Err(OciError::InvalidDescriptor);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Descriptor, OciError, parse_autoindex_manifest, parse_route_map, valid_digest,
        verify_descriptor,
    };

    #[test]
    fn validates_digest_and_length() {
        let bytes = b"fixture";
        let descriptor = Descriptor::new(
            "application/octet-stream",
            "sha256:0e8e3a5f5f1f857e5e7e8b5e0f2e5eaaec2dc6b3cc9b5b04d925f0e6e7ea5b2c",
            bytes.len() as u64,
        );
        assert_eq!(
            verify_descriptor(&descriptor, bytes),
            Err(OciError::InvalidDescriptor)
        );
        assert_eq!(valid_digest("md5:abc"), Err(OciError::InvalidDescriptor));
    }

    #[test]
    fn projects_only_visible_annotated_layers_by_title() {
        let bytes = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.empty.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":0},"layers":[{"mediaType":"text/html","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","size":3,"annotations":{"org.opencontainers.image.title":"pypi/simple/index.html","io.github.djha-skin.github-oras-packages.autoindex.visible":"true"}},{"mediaType":"application/octet-stream","digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222","size":4,"annotations":{"org.opencontainers.image.title":"secret.bin"}}]}"#;
        let snapshot = parse_autoindex_manifest(bytes).unwrap();
        assert_eq!(snapshot.object_count(), 1);
        let object = snapshot.object("pypi/simple/index.html").unwrap();
        assert_eq!(
            object.digest(),
            "sha256:1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert!(snapshot.object("secret.bin").is_none());
    }

    #[test]
    fn rejects_visible_layers_without_safe_titles_or_duplicate_paths() {
        let bytes = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.empty.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":0},"layers":[{"mediaType":"text/plain","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","size":1,"annotations":{"io.github.djha-skin.github-oras-packages.autoindex.visible":"true"}}]}"#;
        assert!(matches!(
            parse_autoindex_manifest(bytes),
            Err(OciError::InvalidLayout)
        ));
    }

    #[test]
    fn rejects_duplicate_or_non_pypi_routes() {
        let bytes = br#"{"version":1,"routes":[{"protocol":"pypi","path":"simple/","descriptor":{"mediaType":"text/html","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":1}},{"protocol":"rpm","path":"x","descriptor":{"mediaType":"x","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":1}}]}"#;
        assert!(parse_route_map(bytes).is_ok());
    }
}
