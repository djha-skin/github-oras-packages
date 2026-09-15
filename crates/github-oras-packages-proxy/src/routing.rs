//! Strict parsing for the public package-repository route namespace.
//!
//! This module accepts only the raw origin-form target described by the v1
//! contract. It deliberately does not use a framework router: framework path
//! normalization could turn an invalid spelling into an authorized route.

use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

const ROUTE_PREFIX: &str = "/r/v1/";
const MAX_REPOSITORY_BYTES: usize = 255;
const MAX_REPOSITORY_SEGMENTS: usize = 32;
const MAX_PROTOCOL_PATH_BYTES: usize = 4_096;
const MAX_PROTOCOL_PATH_SEGMENTS: usize = 128;
const MAX_LOCATOR_BYTES: usize = 340;

/// A repository name that passed the v1 GHCR-compatible admission check.
///
/// The inner string is the exact decoded locator value. It is never
/// normalized, aliased, prefixed, or case-folded.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ValidatedRepository(String);

impl ValidatedRepository {
    /// Validates an exact repository name from trusted decoded locator bytes.
    pub fn parse(repository: &str) -> Result<Self, RouteError> {
        if repository.is_empty()
            || repository.len() > MAX_REPOSITORY_BYTES
            || !repository.is_ascii()
        {
            return Err(RouteError::InvalidRepository);
        }

        let segments = repository.split('/').collect::<Vec<_>>();
        if segments.len() > MAX_REPOSITORY_SEGMENTS
            || segments
                .iter()
                .any(|segment| !is_repository_segment(segment))
        {
            return Err(RouteError::InvalidRepository);
        }

        Ok(Self(repository.to_owned()))
    }

    /// Returns the exact repository string to use under an OCI `/v2/` path.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the one canonical unpadded base64url transport representation.
    pub fn locator(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.as_bytes())
    }
}

/// A supported package-protocol route namespace.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Protocol {
    /// PyPI's Simple API and distribution routes.
    Pypi,
    /// RPM-MD repository metadata and RPM archives.
    Rpm,
    /// Debian Release metadata, package indices, and Debian archives.
    Apt,
    /// Arch sync database metadata and package archives.
    Pacman,
}

impl Protocol {
    /// Returns the protocol's exact public path component.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pypi => "pypi",
            Self::Rpm => "rpm",
            Self::Apt => "apt",
            Self::Pacman => "pacman",
        }
    }

    fn parse(value: &str) -> Result<Self, RouteError> {
        match value {
            "pypi" => Ok(Self::Pypi),
            "rpm" => Ok(Self::Rpm),
            "apt" => Ok(Self::Apt),
            "pacman" => Ok(Self::Pacman),
            _ => Err(RouteError::UnsupportedProtocol),
        }
    }
}

/// A bit set that controls which compiled frontends are exposed by a listener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnabledProtocols(u8);

impl EnabledProtocols {
    const PYPI: u8 = 1 << 0;
    const RPM: u8 = 1 << 1;
    const APT: u8 = 1 << 2;
    const PACMAN: u8 = 1 << 3;

    /// Enables all v1 package protocol frontends.
    pub const fn all() -> Self {
        Self(Self::PYPI | Self::RPM | Self::APT | Self::PACMAN)
    }

    /// Enables no frontend. Useful as a safe configuration starting point.
    pub const fn none() -> Self {
        Self(0)
    }

    /// Enables one protocol while retaining any previously enabled protocols.
    pub const fn with(self, protocol: Protocol) -> Self {
        Self(self.0 | Self::bit(protocol))
    }

    /// Reports whether the protocol is enabled.
    pub const fn contains(self, protocol: Protocol) -> bool {
        self.0 & Self::bit(protocol) != 0
    }

    const fn bit(protocol: Protocol) -> u8 {
        match protocol {
            Protocol::Pypi => Self::PYPI,
            Protocol::Rpm => Self::RPM,
            Protocol::Apt => Self::APT,
            Protocol::Pacman => Self::PACMAN,
        }
    }
}

impl Default for EnabledProtocols {
    fn default() -> Self {
        Self::all()
    }
}

/// A canonical relative path below one protocol namespace.
///
/// A final slash is preserved because it is meaningful for package clients and
/// route-map matching.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalProtocolPath(String);

impl CanonicalProtocolPath {
    /// Returns the raw canonical relative path exactly as it appeared in the
    /// accepted request target.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A fully validated package request, ready for an exact immutable-map lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedRoute {
    repository: ValidatedRepository,
    protocol: Protocol,
    protocol_path: CanonicalProtocolPath,
}

impl ValidatedRoute {
    /// The exact selected OCI repository.
    pub fn repository(&self) -> &ValidatedRepository {
        &self.repository
    }

    /// The selected package protocol frontend.
    pub const fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// The canonical protocol-relative path to look up exactly in the route map.
    pub fn protocol_path(&self) -> &CanonicalProtocolPath {
        &self.protocol_path
    }
}

/// A safe classification for a locally rejected request.
///
/// No variant retains caller input, preventing accidental inclusion of raw
/// targets or credentials in an error response or telemetry event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteError {
    /// The method is not GET or HEAD.
    UnsupportedMethod,
    /// The target is not a v1 origin-form package route.
    InvalidRoute,
    /// The repository locator is malformed or noncanonical.
    InvalidRepositoryLocator,
    /// The decoded repository violates the strict v1 subset.
    InvalidRepository,
    /// The protocol component is not one of the four v1 frontends.
    UnsupportedProtocol,
    /// The protocol is valid but disabled in local configuration.
    DisabledProtocol,
    /// The relative package path is ambiguous or outside v1 bounds.
    InvalidProtocolPath,
}

impl RouteError {
    /// A stable non-sensitive error code for a later safe response mapper.
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnsupportedMethod => "unsupported_method",
            Self::InvalidRoute => "invalid_route",
            Self::InvalidRepositoryLocator => "invalid_repository_locator",
            Self::InvalidRepository => "invalid_repository",
            Self::UnsupportedProtocol => "unsupported_protocol",
            Self::DisabledProtocol => "disabled_protocol",
            Self::InvalidProtocolPath => "invalid_protocol_path",
        }
    }
}

impl fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for RouteError {}

/// Parses one raw HTTP request target without path or percent-decoding.
///
/// The `method` argument must be the HTTP method token received by the
/// listener. A later Hyper adapter must pass the unnormalized raw target into
/// this function before any framework router runs.
pub fn parse_raw_target(
    method: &str,
    raw_target: &str,
    enabled_protocols: EnabledProtocols,
) -> Result<ValidatedRoute, RouteError> {
    if !matches!(method, "GET" | "HEAD") {
        return Err(RouteError::UnsupportedMethod);
    }

    if raw_target.is_empty()
        || !raw_target.is_ascii()
        || raw_target.bytes().any(|byte| byte.is_ascii_control())
        || raw_target.contains(['?', '#', '%', '\\'])
        || !raw_target.starts_with('/')
    {
        return Err(RouteError::InvalidRoute);
    }

    let route = raw_target
        .strip_prefix(ROUTE_PREFIX)
        .ok_or(RouteError::InvalidRoute)?;
    let mut parts = route.splitn(3, '/');
    let locator = parts.next().ok_or(RouteError::InvalidRoute)?;
    let protocol = parts.next().ok_or(RouteError::InvalidRoute)?;
    let protocol_path = parts.next().ok_or(RouteError::InvalidRoute)?;

    if locator.is_empty() {
        return Err(RouteError::InvalidRepositoryLocator);
    }

    let repository = decode_repository_locator(locator)?;
    let protocol = Protocol::parse(protocol)?;
    if !enabled_protocols.contains(protocol) {
        return Err(RouteError::DisabledProtocol);
    }

    let protocol_path = parse_protocol_path(protocol_path)?;
    Ok(ValidatedRoute {
        repository,
        protocol,
        protocol_path,
    })
}

fn decode_repository_locator(locator: &str) -> Result<ValidatedRepository, RouteError> {
    if locator.is_empty()
        || locator.len() > MAX_LOCATOR_BYTES
        || !locator.bytes().all(is_base64url_character)
    {
        return Err(RouteError::InvalidRepositoryLocator);
    }

    let decoded = URL_SAFE_NO_PAD
        .decode(locator)
        .map_err(|_| RouteError::InvalidRepositoryLocator)?;
    let repository =
        std::str::from_utf8(&decoded).map_err(|_| RouteError::InvalidRepositoryLocator)?;

    // A decoder can accept encodings with unused low bits. Re-encoding makes
    // one spelling authoritative and rejects aliases before any upstream work.
    if URL_SAFE_NO_PAD.encode(&decoded) != locator {
        return Err(RouteError::InvalidRepositoryLocator);
    }

    ValidatedRepository::parse(repository)
}

fn parse_protocol_path(path: &str) -> Result<CanonicalProtocolPath, RouteError> {
    if path.is_empty() || path.len() > MAX_PROTOCOL_PATH_BYTES || !path.is_ascii() {
        return Err(RouteError::InvalidProtocolPath);
    }

    let has_trailing_slash = path.ends_with('/');
    let segments = path.split('/').collect::<Vec<_>>();
    let meaningful_segments = if has_trailing_slash {
        &segments[..segments.len() - 1]
    } else {
        &segments[..]
    };

    if meaningful_segments.is_empty()
        || meaningful_segments.len() > MAX_PROTOCOL_PATH_SEGMENTS
        || meaningful_segments.iter().any(|segment| {
            segment.is_empty()
                || matches!(*segment, "." | "..")
                || segment.contains(';')
                || segment.bytes().any(|byte| byte.is_ascii_control())
        })
    {
        return Err(RouteError::InvalidProtocolPath);
    }

    Ok(CanonicalProtocolPath(path.to_owned()))
}

fn is_repository_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let Some((&first, remainder)) = bytes.split_first() else {
        return false;
    };

    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }

    let mut previous_was_separator = false;
    for &byte in remainder {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            previous_was_separator = false;
        } else if matches!(byte, b'.' | b'_' | b'-') {
            if previous_was_separator {
                return false;
            }
            previous_was_separator = true;
        } else {
            return false;
        }
    }

    !previous_was_separator
}

const fn is_base64url_character(byte: u8) -> bool {
    byte.is_ascii_uppercase()
        || byte.is_ascii_lowercase()
        || byte.is_ascii_digit()
        || matches!(byte, b'-' | b'_')
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use proptest::prelude::*;

    use super::{EnabledProtocols, Protocol, RouteError, ValidatedRepository, parse_raw_target};

    fn locator(repository: &str) -> String {
        ValidatedRepository::parse(repository)
            .expect("test repository must be valid")
            .locator()
    }

    fn target(repository: &str, protocol: Protocol, path: &str) -> String {
        format!(
            "/r/v1/{}/{}/{}",
            locator(repository),
            protocol.as_str(),
            path
        )
    }

    #[test]
    fn accepts_each_protocol_and_preserves_exact_repository() {
        let cases = [
            ("foo", Protocol::Pypi, "simple/example-project/"),
            ("acme/rpms", Protocol::Rpm, "repodata/repomd.xml"),
            ("acme/debs", Protocol::Apt, "dists/stable/InRelease"),
            ("acme/arch", Protocol::Pacman, "acme.db"),
        ];

        for (repository, protocol, path) in cases {
            let route = parse_raw_target(
                "GET",
                &target(repository, protocol, path),
                EnabledProtocols::all(),
            )
            .expect("example route should parse");
            assert_eq!(route.repository().as_str(), repository);
            assert_eq!(route.repository().locator(), locator(repository));
            assert_eq!(route.protocol(), protocol);
            assert_eq!(route.protocol_path().as_str(), path);
        }
    }

    #[test]
    fn accepts_head_without_changing_the_route() {
        let raw_target = target("acme/rpms", Protocol::Rpm, "repodata/repomd.xml");
        let get = parse_raw_target("GET", &raw_target, EnabledProtocols::all()).unwrap();
        let head = parse_raw_target("HEAD", &raw_target, EnabledProtocols::all()).unwrap();
        assert_eq!(head, get);
    }

    #[test]
    fn rejects_unsupported_methods_before_route_parsing() {
        for method in ["POST", "PUT", "DELETE", "OPTIONS", "get", ""] {
            assert_eq!(
                parse_raw_target(method, "/not-a-route", EnabledProtocols::all()),
                Err(RouteError::UnsupportedMethod)
            );
        }
    }

    #[test]
    fn rejects_noncanonical_locator_spellings() {
        for locator in ["Zm9v=", "Zh", "!", ""] {
            let target = format!("/r/v1/{locator}/pypi/simple/example/");
            assert_eq!(
                parse_raw_target("GET", &target, EnabledProtocols::all()),
                Err(RouteError::InvalidRepositoryLocator)
            );
        }

        for target in [
            "/r/v1/Zm9v%3D/pypi/simple/example/",
            "/r/v1/Zm9v/pypi/simple/example/%3D",
        ] {
            assert_eq!(
                parse_raw_target("GET", target, EnabledProtocols::all()),
                Err(RouteError::InvalidRoute)
            );
        }
    }

    #[test]
    fn rejects_invalid_decoded_repositories_without_normalizing() {
        for repository in [
            "Foo",
            "foo/",
            "foo//bar",
            "foo/../bar",
            "foo/.hidden",
            "foo/bar-",
            "foo/bar__baz",
            "foo/ba$r",
        ] {
            let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(repository);
            let target = format!("/r/v1/{encoded}/rpm/repodata/repomd.xml");
            assert_eq!(
                parse_raw_target("GET", &target, EnabledProtocols::all()),
                Err(RouteError::InvalidRepository)
            );
        }
    }

    #[test]
    fn rejects_non_origin_form_and_ambiguous_targets() {
        let valid = target("foo", Protocol::Pypi, "simple/example/");
        for raw_target in [
            "https://proxy.example/r/v1/Zm9v/pypi/simple/example/",
            "/r/v2/Zm9v/pypi/simple/example/",
            "/r/v1/Zm9v/pypi/simple/example/?ignored=true",
            "/r/v1/Zm9v/pypi/simple/example/#fragment",
            "/r/v1/Zm9v/pypi/simple/%65xample/",
            "/r/v1/Zm9v/pypi/simple/%2Fexample/",
            "/r/v1/Zm9v/pypi/simple/%5cexample/",
            "/r/v1/Zm9v/pypi/simple\\example/",
            "/r/v1/Zm9v/pypi/simple/example/\u{7f}",
        ] {
            assert_eq!(
                parse_raw_target("GET", raw_target, EnabledProtocols::all()),
                Err(RouteError::InvalidRoute)
            );
        }
        assert!(parse_raw_target("GET", &valid, EnabledProtocols::all()).is_ok());
    }

    #[test]
    fn rejects_ambiguous_protocol_paths() {
        for path in [
            "",
            "/simple/example",
            "simple//example",
            "simple/./example",
            "simple/../example",
            "simple/example;parameter",
            "/",
        ] {
            let raw_target = target("foo", Protocol::Pypi, path);
            assert_eq!(
                parse_raw_target("GET", &raw_target, EnabledProtocols::all()),
                Err(RouteError::InvalidProtocolPath),
                "path: {path:?}"
            );
        }
    }

    #[test]
    fn preserves_trailing_slash_for_exact_route_map_matching() {
        let without_slash = parse_raw_target(
            "GET",
            &target("foo", Protocol::Pypi, "simple/example"),
            EnabledProtocols::all(),
        )
        .unwrap();
        let with_slash = parse_raw_target(
            "GET",
            &target("foo", Protocol::Pypi, "simple/example/"),
            EnabledProtocols::all(),
        )
        .unwrap();

        assert_ne!(without_slash.protocol_path(), with_slash.protocol_path());
        assert_eq!(with_slash.protocol_path().as_str(), "simple/example/");
    }

    #[test]
    fn rejects_disabled_frontends() {
        let only_rpm = EnabledProtocols::none().with(Protocol::Rpm);
        assert_eq!(
            parse_raw_target("GET", &target("foo", Protocol::Pacman, "oras.db"), only_rpm,),
            Err(RouteError::DisabledProtocol)
        );
        assert!(
            parse_raw_target(
                "GET",
                &target("foo", Protocol::Rpm, "repodata/repomd.xml"),
                only_rpm,
            )
            .is_ok()
        );
    }

    proptest! {
        #[test]
        fn canonical_locator_round_trips_every_admitted_repository(
            segment_count in 1_usize..=8,
            segment_seed in proptest::collection::vec("[a-z0-9]{1,12}", 1..=8),
        ) {
            let repository = segment_seed
                .into_iter()
                .take(segment_count)
                .collect::<Vec<_>>()
                .join("/");
            let admitted = ValidatedRepository::parse(&repository).expect("generator emits admitted names");
            let raw_target = format!(
                "/r/v1/{}/rpm/repodata/repomd.xml",
                admitted.locator(),
            );

            let parsed = parse_raw_target("GET", &raw_target, EnabledProtocols::all())?;
            prop_assert_eq!(parsed.repository().as_str(), repository);
            prop_assert_eq!(parsed.repository().locator(), admitted.locator());
        }

        #[test]
        fn any_percent_escape_is_rejected_before_a_route_is_returned(
            prefix in "[A-Za-z0-9/_-]{0,32}",
            suffix in "[A-Za-z0-9/_-]{0,32}",
        ) {
            let raw_target = format!("/r/v1/Zm9v/pypi/{prefix}%2F{suffix}");
            prop_assert_eq!(
                parse_raw_target("GET", &raw_target, EnabledProtocols::all()),
                Err(RouteError::InvalidRoute),
            );
        }
    }

    #[test]
    fn repository_and_path_limits_reject_oversized_inputs() {
        let oversized_repository = "a".repeat(256);
        assert_eq!(
            ValidatedRepository::parse(&oversized_repository),
            Err(RouteError::InvalidRepository)
        );

        let oversized_path = "a".repeat(4_097);
        assert_eq!(
            parse_raw_target(
                "GET",
                &target("foo", Protocol::Pacman, &oversized_path),
                EnabledProtocols::all(),
            ),
            Err(RouteError::InvalidProtocolPath)
        );
    }
}
