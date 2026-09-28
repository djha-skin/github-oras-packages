//! Safe OCI bearer-token challenge handling and bounded credential caching.
//!
//! This module deliberately never retains an upstream response body, a raw
//! challenge field, or a token in any diagnostic type. Tokens live only in a
//! small in-memory cache and are always derived for one configured repository.

use std::{
    collections::BTreeMap,
    fmt,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use hyper::header::HeaderValue;

const MAX_CHALLENGE_BYTES: usize = 8 * 1024;
const MAX_TOKEN_BYTES: usize = 8 * 1024;
const CACHE_CAPACITY: usize = 16;
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const EXPIRY_SKEW: Duration = Duration::from_secs(30);

/// Credentials used only for a configured OCI token service exchange.
///
/// The custom `Debug` implementation is deliberately content-free so a
/// configuration value cannot reach diagnostics through a derived formatter.
#[derive(Clone, Eq, PartialEq)]
pub struct TokenCredentials {
    username: String,
    password: String,
}

impl TokenCredentials {
    /// Validates bounded, single-line credentials from a secret source.
    pub(crate) fn new(username: &str, password: &str) -> Option<Self> {
        if username.is_empty()
            || password.is_empty()
            || username.len() > 1_024
            || password.len() > 4_096
            || username.bytes().any(|byte| byte.is_ascii_control())
            || password.bytes().any(|byte| byte.is_ascii_control())
        {
            return None;
        }
        Some(Self {
            username: username.to_owned(),
            password: password.to_owned(),
        })
    }

    /// Produces the Basic authorization field for the token endpoint.
    pub(crate) fn basic_authorization(&self) -> Option<HeaderValue> {
        let encoded = STANDARD.encode(format!("{}:{}", self.username, self.password));
        HeaderValue::from_str(&format!("Basic {encoded}")).ok()
    }
}

impl fmt::Debug for TokenCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TokenCredentials { configured: true }")
    }
}

/// A validated Bearer challenge whose realm is pinned to the configured origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BearerChallenge {
    realm: String,
    service: String,
}

impl BearerChallenge {
    /// Returns the fixed, validated token endpoint URI.
    pub(crate) fn realm(&self) -> &str {
        &self.realm
    }

    /// Returns the validated registry service name.
    pub(crate) fn service(&self) -> &str {
        &self.service
    }
}

/// Returns the first usable Bearer challenge from a fixed-origin registry.
///
/// A challenge realm must be a path under exactly `origin`; this rejects token
/// service redirects and arbitrary authority injection before any credential is
/// sent. The caller derives the scope from its validated repository rather
/// than accepting a challenge-provided scope.
pub(crate) fn bearer_challenge(
    values: impl IntoIterator<Item = HeaderValue>,
    origin: &str,
) -> Option<BearerChallenge> {
    for value in values {
        let value = value.to_str().ok()?;
        if value.len() > MAX_CHALLENGE_BYTES {
            continue;
        }
        let Some(parameters) = bearer_parameters(value) else {
            continue;
        };
        let (Some(realm), Some(service)) = (parameters.get("realm"), parameters.get("service"))
        else {
            continue;
        };
        let Some(path) = realm.strip_prefix(origin) else {
            continue;
        };
        if !path.starts_with('/')
            || path.contains(['?', '#', '\\'])
            || path.bytes().any(|byte| byte.is_ascii_control())
            || !service_is_safe(service)
        {
            continue;
        }
        return Some(BearerChallenge {
            realm: realm.clone(),
            service: service.clone(),
        });
    }
    None
}

fn service_is_safe(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// Parses the auth-params belonging to the first Bearer challenge field.
///
/// This accepts standard quoted strings, including escaped quote/backslash,
/// and distinguishes a comma followed by another challenge scheme from a
/// comma followed by another Bearer parameter. Duplicate parameters are
/// rejected, avoiding ambiguous realm or service selection.
fn bearer_parameters(value: &str) -> Option<BTreeMap<String, String>> {
    let bytes = value.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        skip_ows_and_commas(bytes, &mut cursor);
        let scheme = parse_token(bytes, &mut cursor)?;
        skip_ows(bytes, &mut cursor);
        if !scheme.eq_ignore_ascii_case("Bearer") {
            skip_challenge(bytes, &mut cursor);
            continue;
        }
        let mut parameters = BTreeMap::new();
        loop {
            skip_ows(bytes, &mut cursor);
            let key = parse_token(bytes, &mut cursor)?;
            skip_ows(bytes, &mut cursor);
            if bytes.get(cursor) != Some(&b'=') {
                return None;
            }
            cursor += 1;
            skip_ows(bytes, &mut cursor);
            let parameter = if bytes.get(cursor) == Some(&b'"') {
                parse_quoted_string(bytes, &mut cursor)?
            } else {
                parse_token(bytes, &mut cursor)?.to_owned()
            };
            if parameters
                .insert(key.to_ascii_lowercase(), parameter)
                .is_some()
            {
                return None;
            }
            skip_ows(bytes, &mut cursor);
            if bytes.get(cursor) != Some(&b',') {
                return Some(parameters);
            }
            cursor += 1;
            let next = cursor;
            skip_ows(bytes, &mut cursor);
            let mut lookahead = cursor;
            let _next_token = parse_token(bytes, &mut lookahead)?;
            skip_ows(bytes, &mut lookahead);
            if bytes.get(lookahead) != Some(&b'=') {
                return Some(parameters);
            }
            cursor = next;
        }
    }
    None
}

fn skip_challenge(bytes: &[u8], cursor: &mut usize) {
    let mut quoted = false;
    let mut escaped = false;
    while let Some(byte) = bytes.get(*cursor) {
        *cursor += 1;
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
        } else if *byte == b'"' {
            quoted = true;
        } else if *byte == b',' {
            return;
        }
    }
}

fn skip_ows_and_commas(bytes: &[u8], cursor: &mut usize) {
    loop {
        skip_ows(bytes, cursor);
        if bytes.get(*cursor) == Some(&b',') {
            *cursor += 1;
        } else {
            return;
        }
    }
}

fn skip_ows(bytes: &[u8], cursor: &mut usize) {
    while matches!(bytes.get(*cursor), Some(b' ' | b'\t')) {
        *cursor += 1;
    }
}

fn parse_token<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a str> {
    let start = *cursor;
    while let Some(byte) = bytes.get(*cursor) {
        if byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(byte) {
            *cursor += 1;
        } else {
            break;
        }
    }
    (start < *cursor)
        .then(|| std::str::from_utf8(&bytes[start..*cursor]).ok())
        .flatten()
}

fn parse_quoted_string(bytes: &[u8], cursor: &mut usize) -> Option<String> {
    if bytes.get(*cursor) != Some(&b'"') {
        return None;
    }
    *cursor += 1;
    let mut result = String::new();
    while let Some(byte) = bytes.get(*cursor) {
        *cursor += 1;
        match *byte {
            b'"' => return Some(result),
            b'\\' => {
                let escaped = *bytes.get(*cursor)?;
                *cursor += 1;
                if escaped.is_ascii_control() {
                    return None;
                }
                result.push(escaped as char);
            }
            byte if byte.is_ascii_control() => return None,
            byte => result.push(byte as char),
        }
    }
    None
}

struct CachedToken {
    authorization: HeaderValue,
    expires_at: Instant,
}

/// A small process-local bearer-token cache, bounded by count and lifetime.
#[derive(Default)]
pub(crate) struct TokenCache {
    entries: BTreeMap<String, CachedToken>,
}

impl TokenCache {
    /// Retrieves an unexpired authorization value for one literal repository.
    pub(crate) fn get(&mut self, repository: &str) -> Option<HeaderValue> {
        self.discard_expired();
        self.entries
            .get(repository)
            .map(|entry| entry.authorization.clone())
    }

    /// Removes the cached token for the rejected repository.
    pub(crate) fn remove(&mut self, repository: &str) {
        self.entries.remove(repository);
    }

    /// Stores a token until its bounded expiry, with skew before use.
    pub(crate) fn insert(
        &mut self,
        repository: &str,
        authorization: HeaderValue,
        expires_in: Option<u64>,
    ) {
        let lifetime = expires_in
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(60))
            .min(CACHE_TTL);
        let Some(lifetime) = lifetime.checked_sub(EXPIRY_SKEW) else {
            return;
        };
        if self.entries.len() >= CACHE_CAPACITY && !self.entries.contains_key(repository) {
            let _ = self.entries.pop_first();
        }
        self.entries.insert(
            repository.to_owned(),
            CachedToken {
                authorization,
                expires_at: Instant::now() + lifetime,
            },
        );
    }

    fn discard_expired(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, entry| entry.expires_at > now);
    }
}

/// Validates a token value before it is ever used in an HTTP header.
pub(crate) fn bearer_authorization(token: &str) -> Option<HeaderValue> {
    if token.is_empty()
        || token.len() > MAX_TOKEN_BYTES
        || token.bytes().any(|byte| byte.is_ascii_control())
    {
        return None;
    }
    HeaderValue::from_str(&format!("Bearer {token}")).ok()
}

#[cfg(test)]
mod tests {
    use hyper::header::HeaderValue;

    use super::{TokenCache, bearer_authorization, bearer_challenge};

    #[test]
    fn accepts_a_quoted_matching_bearer_challenge() {
        let challenge = bearer_challenge(
            [HeaderValue::from_static(
                "Basic realm=\"other\", Bearer realm=\"https://ghcr.io/token\",service=\"ghcr.io\",scope=\"ignored\"",
            )],
            "https://ghcr.io",
        )
        .unwrap();
        assert_eq!(challenge.realm(), "https://ghcr.io/token");
        assert_eq!(challenge.service(), "ghcr.io");
    }

    #[test]
    fn rejects_an_unsafe_realm_and_ambiguous_parameters() {
        assert!(
            bearer_challenge(
                [HeaderValue::from_static(
                    "Bearer realm=\"https://attacker.invalid/token\",service=\"ghcr.io\"",
                )],
                "https://ghcr.io",
            )
            .is_none()
        );
        assert!(bearer_challenge(
            [HeaderValue::from_static(
                "Bearer realm=\"https://ghcr.io/token\",realm=\"https://ghcr.io/other\",service=\"ghcr.io\"",
            )],
            "https://ghcr.io",
        )
        .is_none());
    }

    #[test]
    fn cache_does_not_retain_short_lived_tokens() {
        let mut cache = TokenCache::default();
        let value = bearer_authorization("canary").unwrap();
        cache.insert("acme/demo", value, Some(30));
        assert!(cache.get("acme/demo").is_none());
    }
}
