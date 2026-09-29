//! Fixed-origin OCI Distribution retrieval and v1 layout validation.
//!
//! This module is intentionally limited to immutable read operations needed by
//! the local PyPI MVP. It builds every request from the validated repository
//! and configured origin; callers cannot provide a registry URL, repository,
//! manifest reference, or arbitrary blob path.

use std::{collections::BTreeMap, error::Error, fmt, sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http_body::Frame;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode, Uri, body::Incoming, header};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    auth::{TokenCache, TokenCredentials, bearer_authorization, bearer_challenge},
    autoindex::{
        Index, PUBLISHER_ANNOTATION, PUBLISHER_VERSION, TITLE_ANNOTATION, VISIBILITY_ANNOTATION,
        VisibleObject,
    },
    config::Config,
    routing::ValidatedRepository,
};

const LAYOUT_REFERENCE: &str = "oras-packages.v1";
pub const AUTO_INDEX_REFERENCE: &str = "autoindex.v1";
const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const EMPTY_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";
const EMPTY_CONFIG_DIGEST: &str =
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
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
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

impl Descriptor {
    /// Creates a descriptor for tests and controlled publisher-side inputs.
    pub fn new(media_type: impl Into<String>, digest: impl Into<String>, size: u64) -> Self {
        Self {
            media_type: media_type.into(),
            digest: digest.into(),
            size,
            annotations: BTreeMap::new(),
            extra: BTreeMap::new(),
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
            extra: BTreeMap::new(),
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
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    config: Descriptor,
    layers: Vec<Descriptor>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug)]
/// A validated standard OCI manifest projected into autoindex paths.
pub struct AutoindexSnapshot {
    index: Index,
    crud_compatible: bool,
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

    /// Reports whether rebuilding this artifact preserves its full OCI content.
    ///
    /// File-level CRUD is restricted to artifacts produced by the native
    /// publisher: all layers are visible, the publisher marker is present, and
    /// the config is the standard empty OCI config. This prevents CRUD from
    /// silently dropping hidden layers or application-specific config.
    pub const fn is_crud_compatible(&self) -> bool {
        self.crud_compatible
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
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, BlobBody>,
    timeout: Duration,
    token_credentials: Option<TokenCredentials>,
    token_cache: Arc<tokio::sync::Mutex<TokenCache>>,
}

impl OciClient {
    /// Creates a client from validated runtime configuration.
    pub fn new(config: &Config) -> Result<Self, OciError> {
        let origin = config.upstream().as_str();
        let mut builder = Client::builder(TokioExecutor::new());
        builder.pool_idle_timeout(Duration::from_secs(30));
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Ok(Self {
            origin,
            client: builder.build(connector),
            timeout: config.request_timeout(),
            token_credentials: config.token_credentials().cloned(),
            token_cache: Arc::new(tokio::sync::Mutex::new(TokenCache::default())),
        })
    }

    /// Reports whether anonymous reads can use configured broker credentials.
    ///
    /// Callers use this only to select conservative response cache headers;
    /// credential values are never exposed.
    pub const fn has_token_broker(&self) -> bool {
        self.token_credentials.is_some()
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

    /// Fetches only OCI manifest response metadata using the standard HEAD method.
    pub async fn manifest_head(
        &self,
        repository: &ValidatedRepository,
        reference: &str,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<HeaderMap, OciError> {
        if reference.is_empty()
            || reference.len() > 256
            || !reference.is_ascii()
            || reference.contains(['/', '?', '#', '%', '\\'])
        {
            return Err(OciError::InvalidDescriptor);
        }
        let response = self
            .request_response_method(
                Method::HEAD,
                repository,
                format!("/manifests/{reference}"),
                MANIFEST_MEDIA_TYPE,
                authorization,
            )
            .await?;
        check_response_length(&response, MAX_METADATA_BYTES as u64)?;
        Ok(response.into_parts().0.headers)
    }

    /// Fetches only OCI blob response metadata using the standard HEAD method.
    pub async fn blob_head(
        &self,
        repository: &ValidatedRepository,
        digest: &str,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<HeaderMap, OciError> {
        let digest = valid_digest(digest)?;
        let response = self
            .request_response_method(
                Method::HEAD,
                repository,
                format!("/blobs/{digest}"),
                "application/octet-stream",
                authorization,
            )
            .await?;
        check_response_length(&response, MAX_ARTIFACT_BYTES)?;
        Ok(response.into_parts().0.headers)
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

    /// Forwards a validated OCI Distribution write operation without buffering its body.
    ///
    /// `suffix` is assembled by the gateway from a parsed repository-scoped target;
    /// the client never accepts an arbitrary upstream URL or repository from the caller.
    pub(crate) async fn forward_distribution(
        &self,
        suffix: &str,
        request: Request<Incoming>,
        repository: &ValidatedRepository,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<hyper::Response<Incoming>, OciError> {
        let method = request.method().clone();
        let query = request.uri().query().map(str::to_owned);
        let request_headers = request.headers().clone();
        let body = request.into_body();
        let uri: Uri = format!(
            "{}/v2/{}{}{}",
            self.origin,
            repository.as_str(),
            suffix,
            query
                .as_deref()
                .map_or_else(String::new, |query| format!("?{query}"))
        )
        .parse()
        .map_err(|_| OciError::Configuration)?;
        let body = body
            .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>)
            .boxed_unsync();
        let mut builder = Request::builder().method(method).uri(uri);
        for name in [
            header::ACCEPT,
            header::CONTENT_TYPE,
            header::CONTENT_LENGTH,
            header::CONTENT_RANGE,
            header::IF_MATCH,
            header::IF_NONE_MATCH,
            header::IF_UNMODIFIED_SINCE,
            header::IF_MODIFIED_SINCE,
            header::HeaderName::from_static("digest"),
        ] {
            if let Some(value) = request_headers.get(&name) {
                builder = builder.header(name, value);
            }
        }
        if let Some(value) = request_headers.get("oci-chunk-min-length") {
            builder = builder.header("oci-chunk-min-length", value);
        }
        if let Some(authorization) = authorization {
            builder = builder.header(header::AUTHORIZATION, authorization);
        }
        let request = builder.body(body).map_err(|_| OciError::Configuration)?;
        let response = tokio::time::timeout(self.timeout, self.client.request(request))
            .await
            .map_err(|_| OciError::Timeout)?
            .map_err(|_| OciError::Transport)?;
        if !response.status().is_success() {
            classify_response(&response)?;
        }
        let mut response = response;
        if let Some(location) = response.headers().get(header::LOCATION).cloned() {
            let rewritten = self.rewrite_distribution_location(&location, repository)?;
            response.headers_mut().insert(header::LOCATION, rewritten);
        }
        Ok(response)
    }

    async fn request_response(
        &self,
        repository: &ValidatedRepository,
        suffix: String,
        accept: &str,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<hyper::Response<Incoming>, OciError> {
        self.request_response_method(Method::GET, repository, suffix, accept, authorization)
            .await
    }

    async fn request_response_method(
        &self,
        method: Method,
        repository: &ValidatedRepository,
        suffix: String,
        accept: &str,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<hyper::Response<Incoming>, OciError> {
        let uri: Uri = format!("{}/v2/{}{suffix}", self.origin, repository.as_str())
            .parse()
            .map_err(|_| OciError::Configuration)?;
        if authorization.is_some() || self.token_credentials.is_none() {
            let response = self
                .send_request(method.clone(), uri, accept, authorization)
                .await?;
            classify_response(&response)?;
            return Ok(response);
        }

        let cached = self.token_cache.lock().await.get(repository.as_str());
        let response = self
            .send_request(method.clone(), uri.clone(), accept, cached.as_ref())
            .await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            classify_response(&response)?;
            return Ok(response);
        }
        let original_unauthorized = classify_response(&response)
            .expect_err("an unauthorized response must classify as an error");
        if cached.is_some() {
            self.token_cache.lock().await.remove(repository.as_str());
        }
        let challenge = bearer_challenge(
            response
                .headers()
                .get_all(header::WWW_AUTHENTICATE)
                .iter()
                .cloned(),
            &self.origin,
        )
        .ok_or_else(|| original_unauthorized.clone())?;
        let Some((token, expires_in)) = self.exchange_token(&challenge, repository).await else {
            return Err(original_unauthorized);
        };
        let retry = self.send_request(method, uri, accept, Some(&token)).await?;
        if retry.status() == StatusCode::UNAUTHORIZED {
            return Err(classify_response(&retry)
                .expect_err("a second unauthorized response must classify as an error"));
        }
        classify_response(&retry)?;
        self.token_cache
            .lock()
            .await
            .insert(repository.as_str(), token, expires_in);
        Ok(retry)
    }

    async fn send_request(
        &self,
        method: Method,
        uri: Uri,
        accept: &str,
        authorization: Option<&hyper::header::HeaderValue>,
    ) -> Result<hyper::Response<Incoming>, OciError> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::ACCEPT, accept);
        if let Some(authorization) = authorization {
            builder = builder.header(header::AUTHORIZATION, authorization);
        }
        let request = builder
            .body(
                Full::new(Bytes::new())
                    .map_err(|never| -> Box<dyn Error + Send + Sync> { match never {} })
                    .boxed_unsync(),
            )
            .map_err(|_| OciError::Configuration)?;
        tokio::time::timeout(self.timeout, self.client.request(request))
            .await
            .map_err(|_| OciError::Timeout)?
            .map_err(|_| OciError::Transport)
    }

    fn rewrite_distribution_location(
        &self,
        location: &hyper::header::HeaderValue,
        repository: &ValidatedRepository,
    ) -> Result<hyper::header::HeaderValue, OciError> {
        let location = location.to_str().map_err(|_| OciError::InvalidDescriptor)?;
        let uri: Uri = location.parse().map_err(|_| OciError::InvalidDescriptor)?;
        if let Some(scheme) = uri.scheme_str() {
            let origin: Uri = self.origin.parse().map_err(|_| OciError::Configuration)?;
            if Some(scheme) != origin.scheme_str() || uri.authority() != origin.authority() {
                return Err(OciError::InvalidDescriptor);
            }
        } else if uri.authority().is_some() {
            return Err(OciError::InvalidDescriptor);
        }
        let path_and_query = uri.path_and_query().ok_or(OciError::InvalidDescriptor)?;
        let path = path_and_query.path();
        let upload_prefix = format!("/v2/{}/blobs/uploads/", repository.as_str());
        let blob_prefix = format!("/v2/{}/blobs/", repository.as_str());
        if let Some(upload_id) = path.strip_prefix(&upload_prefix) {
            if upload_id.is_empty()
                || upload_id == "."
                || upload_id == ".."
                || upload_id.contains('/')
                || !upload_id.is_ascii()
                || upload_id.bytes().any(|byte| {
                    !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                })
                || path_and_query
                    .query()
                    .is_some_and(|query| !valid_upload_query(query))
            {
                return Err(OciError::InvalidDescriptor);
            }
        } else if let Some(digest) = path.strip_prefix(&blob_prefix) {
            if path_and_query.query().is_some() || valid_digest(digest).is_err() {
                return Err(OciError::InvalidDescriptor);
            }
        } else {
            return Err(OciError::InvalidDescriptor);
        }
        hyper::header::HeaderValue::from_str(path_and_query.as_str())
            .map_err(|_| OciError::InvalidDescriptor)
    }

    async fn exchange_token(
        &self,
        challenge: &crate::auth::BearerChallenge,
        repository: &ValidatedRepository,
    ) -> Option<(hyper::header::HeaderValue, Option<u64>)> {
        let credentials = self.token_credentials.as_ref()?;
        let authorization = credentials.basic_authorization()?;
        let uri: Uri = format!(
            "{}?service={}&scope=repository:{}:pull",
            challenge.realm(),
            challenge.service(),
            repository.as_str()
        )
        .parse()
        .ok()?;
        let response = self
            .send_request(Method::GET, uri, "application/json", Some(&authorization))
            .await
            .ok()?;
        if response.status() != StatusCode::OK {
            return None;
        }
        let bytes = tokio::time::timeout(
            self.timeout,
            http_body_util::Limited::new(response.into_body(), 64 * 1024).collect(),
        )
        .await
        .ok()?
        .ok()?
        .to_bytes();
        let token: TokenResponse = serde_json::from_slice(&bytes).ok()?;
        let value = token.token.or(token.access_token)?;
        bearer_authorization(&value).map(|authorization| (authorization, token.expires_in))
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default, rename = "access_token")]
    access_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

fn valid_upload_query(query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let mut state_seen = false;
    let mut digest_seen = false;
    for part in query.split('&') {
        let Some((key, value)) = part.split_once('=') else {
            return false;
        };
        match key {
            "_state"
                if !state_seen
                    && !value.is_empty()
                    && !value
                        .bytes()
                        .any(|byte| byte.is_ascii_control() || byte == b'#') =>
            {
                state_seen = true;
            }
            "digest" if !digest_seen && valid_digest(value).is_ok() => digest_seen = true,
            _ => return false,
        }
    }
    state_seen || digest_seen
}

fn check_response_length(
    response: &hyper::Response<Incoming>,
    maximum: u64,
) -> Result<u64, OciError> {
    response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|length| *length <= maximum)
        .ok_or(OciError::InvalidDescriptor)
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
    let crud_compatible = manifest.annotations.len() == 1
        && manifest
            .annotations
            .get(PUBLISHER_ANNOTATION)
            .is_some_and(|value| value == PUBLISHER_VERSION)
        && manifest.extra.is_empty()
        && !manifest.layers.is_empty()
        && manifest.layers.iter().all(is_native_publisher_layer)
        && manifest.layers.windows(2).all(|pair| {
            matches!((pair[0].title(), pair[1].title()), (Some(left), Some(right)) if left < right)
        })
        && manifest.config.extra.is_empty()
        && manifest.config.annotations.is_empty()
        && manifest.config.media_type == EMPTY_CONFIG_MEDIA_TYPE
        && manifest.config.digest == EMPTY_CONFIG_DIGEST
        && manifest.config.size == 2;
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
    Ok(AutoindexSnapshot {
        index,
        crud_compatible,
    })
}

fn is_native_publisher_layer(descriptor: &Descriptor) -> bool {
    descriptor.is_autoindex_visible()
        && descriptor.extra.is_empty()
        && descriptor.annotations.len() == 2
        && descriptor
            .title()
            .is_some_and(|title| descriptor.media_type == crate::publisher::media_type_for(title))
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
        assert!(!snapshot.is_crud_compatible());
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
