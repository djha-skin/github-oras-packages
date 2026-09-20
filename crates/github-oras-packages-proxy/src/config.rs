//! Validated runtime configuration for the proxy.
//!
//! Configuration is deliberately small and explicit.  Environment variables
//! are suitable for the service process and a simple `KEY=VALUE` file is
//! supported for local/CI setup.  Neither source can select an arbitrary
//! upstream after validation: the origin is absolute, its host is allowlisted,
//! and cleartext HTTP is permitted only for loopback fixture tests when the
//! explicit opt-in is present.

use std::{collections::BTreeMap, net::IpAddr, time::Duration};

use crate::{inbound::InboundLimits, routing::Protocol};

const ENV_PREFIX: &str = "ORAS_PROXY_";
const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
const DEFAULT_UPSTREAM: &str = "https://ghcr.io";
const DEFAULT_ALLOWED_HOSTS: &str = "ghcr.io";
const DEFAULT_FRONTENDS: &str = "none";
const MAX_TIMEOUT_MS: u64 = 10 * 60 * 1_000;
const MAX_TARGET_BYTES: usize = 64 * 1024;
const MAX_HEADER_COUNT: usize = 256;
const MAX_HEADER_BYTES: usize = 128 * 1024;

/// A validated immutable origin authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpstreamOrigin {
    scheme: Scheme,
    host: String,
    port: Option<u16>,
}

impl UpstreamOrigin {
    /// Returns the complete origin including a non-default port.
    pub fn as_str(&self) -> String {
        let authority = match self.port {
            Some(port) => format!("{}:{port}", host_for_authority(&self.host)),
            None => host_for_authority(&self.host),
        };
        format!("{}://{authority}", self.scheme.as_str())
    }

    /// Returns the scheme, either HTTPS or explicitly approved loopback HTTP.
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Returns the validated upstream host without a port.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the configured port, if one was provided.
    pub const fn port(&self) -> Option<u16> {
        self.port
    }
}

/// An upstream URL scheme accepted by the configuration boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scheme {
    /// TLS-protected upstream, required for non-loopback origins.
    Https,
    /// Cleartext upstream, allowed only for loopback fixtures with opt-in.
    Http,
}

impl Scheme {
    /// Returns the lowercase URL spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Http => "http",
        }
    }
}

/// Validated service configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    listen: String,
    upstream: UpstreamOrigin,
    allowed_hosts: Vec<String>,
    connect_timeout: Duration,
    request_timeout: Duration,
    inbound_limits: InboundLimits,
    enabled_protocols: crate::routing::EnabledProtocols,
    log_level: LogLevel,
    allow_insecure_loopback: bool,
}

impl Config {
    /// Loads configuration from the process environment and optional file.
    ///
    /// If `ORAS_PROXY_CONFIG_FILE` is set, the file is read first and
    /// `ORAS_PROXY_*` environment values override keys from that file.
    pub fn from_env() -> Result<Self, ConfigError> {
        let file_values = match std::env::var("ORAS_PROXY_CONFIG_FILE") {
            Ok(path) => {
                parse_config_file(&std::fs::read_to_string(path).map_err(|_| ConfigError::File)?)?
            }
            Err(std::env::VarError::NotPresent) => BTreeMap::new(),
            Err(_) => return Err(ConfigError::Environment),
        };
        let environment = std::env::vars()
            .filter_map(|(key, value)| {
                key.strip_prefix(ENV_PREFIX)
                    .map(|key| (key.to_owned(), value))
            })
            .filter(|(key, _)| key != "CONFIG_FILE")
            .collect::<BTreeMap<_, _>>();
        Self::from_maps(&file_values, &environment)
    }

    /// Validates explicit file and environment key/value maps.
    ///
    /// `environment` uses names without the `ORAS_PROXY_` prefix, such as
    /// `LISTEN_ADDR` and `ENABLED_FRONTENDS`.  Environment values win over
    /// file values.  This pure entry point keeps configuration tests isolated
    /// from global process state.
    pub fn from_maps(
        file_values: &BTreeMap<String, String>,
        environment: &BTreeMap<String, String>,
    ) -> Result<Self, ConfigError> {
        let value = |key: &str, default: &'static str| {
            environment
                .get(key)
                .or_else(|| file_values.get(key))
                .map(String::as_str)
                .unwrap_or(default)
        };
        reject_unknown_environment_keys(environment)?;
        let listen = parse_listen(value("LISTEN_ADDR", DEFAULT_LISTEN))?;
        let allow_insecure_loopback = parse_bool(
            value("ALLOW_INSECURE_LOOPBACK", "false"),
            "ALLOW_INSECURE_LOOPBACK",
        )?;
        let upstream =
            parse_upstream(value("UPSTREAM", DEFAULT_UPSTREAM), allow_insecure_loopback)?;
        let allowed_hosts =
            parse_allowed_hosts(value("ALLOWED_HOSTS", DEFAULT_ALLOWED_HOSTS), &upstream)?;
        let connect_timeout =
            parse_timeout(value("CONNECT_TIMEOUT_MS", "5000"), "CONNECT_TIMEOUT_MS")?;
        let request_timeout =
            parse_timeout(value("REQUEST_TIMEOUT_MS", "60000"), "REQUEST_TIMEOUT_MS")?;
        if connect_timeout > request_timeout {
            return Err(ConfigError::InvalidValue {
                key: "CONNECT_TIMEOUT_MS",
                reason: "must not exceed REQUEST_TIMEOUT_MS",
            });
        }
        let inbound_limits = InboundLimits::new(
            parse_bounded_usize(
                value("MAX_TARGET_BYTES", "8192"),
                "MAX_TARGET_BYTES",
                1,
                MAX_TARGET_BYTES,
            )?,
            parse_bounded_usize(
                value("MAX_HEADER_COUNT", "64"),
                "MAX_HEADER_COUNT",
                1,
                MAX_HEADER_COUNT,
            )?,
            parse_bounded_usize(
                value("MAX_HEADER_BYTES", "16384"),
                "MAX_HEADER_BYTES",
                1,
                MAX_HEADER_BYTES,
            )?,
            parse_bounded_usize(
                value("MAX_BODY_BYTES", "0"),
                "MAX_BODY_BYTES",
                0,
                64 * 1024 * 1024,
            )?,
        );
        let enabled_protocols = parse_protocols(value("ENABLED_FRONTENDS", DEFAULT_FRONTENDS))?;
        let log_level = LogLevel::parse(value("LOG_LEVEL", "info"))?;
        Ok(Self {
            listen,
            upstream,
            allowed_hosts,
            connect_timeout,
            request_timeout,
            inbound_limits,
            enabled_protocols,
            log_level,
            allow_insecure_loopback,
        })
    }

    /// Returns the validated listen address string.
    pub fn listen_addr(&self) -> &str {
        &self.listen
    }

    /// Returns the fixed upstream origin.
    pub const fn upstream(&self) -> &UpstreamOrigin {
        &self.upstream
    }

    /// Returns the case-insensitive upstream host allowlist.
    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }

    /// Returns the connect deadline.
    pub const fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Returns the complete request deadline.
    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// Returns the configured inbound request limits.
    pub const fn inbound_limits(&self) -> InboundLimits {
        self.inbound_limits
    }

    /// Returns the explicitly enabled protocol set.
    pub const fn enabled_protocols(&self) -> crate::routing::EnabledProtocols {
        self.enabled_protocols
    }

    /// Returns the configured diagnostic level.
    pub const fn log_level(&self) -> LogLevel {
        self.log_level
    }

    /// Reports whether cleartext loopback upstream mode was explicitly opted in.
    pub const fn allow_insecure_loopback(&self) -> bool {
        self.allow_insecure_loopback
    }
}

/// Supported safe log levels.  Logging itself is implemented separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    /// Only errors.
    Error,
    /// Warnings and errors.
    Warn,
    /// Normal operational events.
    Info,
    /// Detailed operational events without request/credential values.
    Debug,
}

impl LogLevel {
    fn parse(value: &str) -> Result<Self, ConfigError> {
        match value {
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            _ => Err(ConfigError::InvalidValue {
                key: "LOG_LEVEL",
                reason: "must be error, warn, info, or debug",
            }),
        }
    }
}

/// A configuration failure that never stores the supplied value.
#[derive(Debug)]
pub enum ConfigError {
    /// An explicitly named config file could not be read.
    File,
    /// An environment source failed to load.
    Environment,
    /// A key/value file line was malformed.
    FileSyntax { line: usize },
    /// A known value failed validation; `reason` is fixed text.
    InvalidValue {
        /// Configuration key, never its value.
        key: &'static str,
        /// Fixed validation reason.
        reason: &'static str,
    },
    /// An unknown key was supplied.
    UnknownKey,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File => formatter.write_str("configuration file could not be read"),
            Self::Environment => formatter.write_str("configuration environment could not be read"),
            Self::FileSyntax { line } => {
                write!(formatter, "configuration file syntax error on line {line}")
            }
            Self::InvalidValue { key, reason } => {
                write!(formatter, "invalid configuration {key}: {reason}")
            }
            Self::UnknownKey => formatter.write_str("unknown configuration key"),
        }
    }
}

impl std::error::Error for ConfigError {}

fn reject_unknown_environment_keys(values: &BTreeMap<String, String>) -> Result<(), ConfigError> {
    if values.keys().any(|key| !known_key(key)) {
        return Err(ConfigError::UnknownKey);
    }
    Ok(())
}

fn parse_config_file(contents: &str) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut values = BTreeMap::new();
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ConfigError::FileSyntax { line: index + 1 });
        };
        let key = key.trim();
        let value = value.trim();
        if !known_key(key) {
            return Err(ConfigError::UnknownKey);
        }
        values.insert(key.to_owned(), value.to_owned());
    }
    Ok(values)
}

fn known_key(key: &str) -> bool {
    matches!(
        key,
        "LISTEN_ADDR"
            | "UPSTREAM"
            | "ALLOWED_HOSTS"
            | "CONNECT_TIMEOUT_MS"
            | "REQUEST_TIMEOUT_MS"
            | "MAX_TARGET_BYTES"
            | "MAX_HEADER_COUNT"
            | "MAX_HEADER_BYTES"
            | "MAX_BODY_BYTES"
            | "ENABLED_FRONTENDS"
            | "LOG_LEVEL"
            | "ALLOW_INSECURE_LOOPBACK"
    )
}

fn parse_listen(value: &str) -> Result<String, ConfigError> {
    let parsed = value
        .parse::<std::net::SocketAddr>()
        .map_err(|_| ConfigError::InvalidValue {
            key: "LISTEN_ADDR",
            reason: "must be a host:port socket address",
        })?;
    if parsed.port() == 0 && !parsed.ip().is_loopback() {
        return Err(ConfigError::InvalidValue {
            key: "LISTEN_ADDR",
            reason: "port zero is allowed only on loopback",
        });
    }
    Ok(value.to_owned())
}

fn parse_upstream(
    value: &str,
    allow_insecure_loopback: bool,
) -> Result<UpstreamOrigin, ConfigError> {
    let Some((scheme, authority)) = value.split_once("://") else {
        return Err(ConfigError::InvalidValue {
            key: "UPSTREAM",
            reason: "must be an absolute https URL or approved loopback http URL",
        });
    };
    let scheme = match scheme {
        "https" => Scheme::Https,
        "http" if allow_insecure_loopback => Scheme::Http,
        "http" => {
            return Err(ConfigError::InvalidValue {
                key: "UPSTREAM",
                reason: "http requires ALLOW_INSECURE_LOOPBACK=true",
            });
        }
        _ => {
            return Err(ConfigError::InvalidValue {
                key: "UPSTREAM",
                reason: "scheme must be https or approved http",
            });
        }
    };
    if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
        return Err(ConfigError::InvalidValue {
            key: "UPSTREAM",
            reason: "authority must contain only one host and optional port",
        });
    }
    let (host, port) = parse_authority(authority)?;
    let ip = host.parse::<IpAddr>().ok();
    if host.contains(':') && ip.is_none() {
        return Err(ConfigError::InvalidValue {
            key: "UPSTREAM",
            reason: "IPv6 host is invalid",
        });
    }
    if scheme == Scheme::Http && !ip.is_some_and(|ip| ip.is_loopback()) {
        return Err(ConfigError::InvalidValue {
            key: "UPSTREAM",
            reason: "cleartext upstream must use a loopback IP",
        });
    }
    if !is_valid_host(&host) {
        return Err(ConfigError::InvalidValue {
            key: "UPSTREAM",
            reason: "host is invalid",
        });
    }
    Ok(UpstreamOrigin { scheme, host, port })
}

fn parse_authority(authority: &str) -> Result<(String, Option<u16>), ConfigError> {
    if authority.starts_with('[') {
        let end = authority.find(']').ok_or(ConfigError::InvalidValue {
            key: "UPSTREAM",
            reason: "IPv6 authority is malformed",
        })?;
        let host = authority[1..end].to_owned();
        let remainder = &authority[end + 1..];
        let port = if remainder.is_empty() {
            None
        } else {
            let Some(port) = remainder.strip_prefix(':') else {
                return Err(ConfigError::InvalidValue {
                    key: "UPSTREAM",
                    reason: "authority port is malformed",
                });
            };
            Some(parse_port(port)?)
        };
        return Ok((host, port));
    }
    match authority.split_once(':') {
        None => Ok((authority.to_owned(), None)),
        Some((host, port)) if !host.is_empty() => Ok((host.to_owned(), Some(parse_port(port)?))),
        _ => Err(ConfigError::InvalidValue {
            key: "UPSTREAM",
            reason: "authority host is malformed",
        }),
    }
}

fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
        && !host.starts_with('.')
        && !host.ends_with('.')
        && !host.starts_with('-')
        && !host.ends_with('-')
        && !host.contains("..")
}

fn parse_port(value: &str) -> Result<u16, ConfigError> {
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(ConfigError::InvalidValue {
            key: "UPSTREAM",
            reason: "port must be a nonzero integer",
        })
}

fn host_for_authority(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

fn parse_allowed_hosts(value: &str, upstream: &UpstreamOrigin) -> Result<Vec<String>, ConfigError> {
    let hosts = value
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if hosts.is_empty()
        || !hosts
            .iter()
            .any(|host| host == &upstream.host.to_ascii_lowercase())
    {
        return Err(ConfigError::InvalidValue {
            key: "ALLOWED_HOSTS",
            reason: "must include the configured upstream host",
        });
    }
    if hosts
        .iter()
        .any(|host| !is_valid_host(host) || host.contains(':'))
    {
        return Err(ConfigError::InvalidValue {
            key: "ALLOWED_HOSTS",
            reason: "must contain hostnames only",
        });
    }
    Ok(hosts)
}

fn parse_timeout(value: &str, key: &'static str) -> Result<Duration, ConfigError> {
    let milliseconds = value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0 && *value <= MAX_TIMEOUT_MS)
        .ok_or(ConfigError::InvalidValue {
            key,
            reason: "must be a positive bounded millisecond value",
        })?;
    Ok(Duration::from_millis(milliseconds))
}

fn parse_bounded_usize(
    value: &str,
    key: &'static str,
    minimum: usize,
    maximum: usize,
) -> Result<usize, ConfigError> {
    let parsed = value
        .parse::<usize>()
        .ok()
        .filter(|value| *value >= minimum && *value <= maximum)
        .ok_or(ConfigError::InvalidValue {
            key,
            reason: "is outside the allowed bounded range",
        })?;
    Ok(parsed)
}

fn parse_bool(value: &str, key: &'static str) -> Result<bool, ConfigError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ConfigError::InvalidValue {
            key,
            reason: "must be true or false",
        }),
    }
}

fn parse_protocols(value: &str) -> Result<crate::routing::EnabledProtocols, ConfigError> {
    if value == "none" {
        return Ok(crate::routing::EnabledProtocols::none());
    }
    let mut protocols = crate::routing::EnabledProtocols::none();
    for protocol in value.split(',').map(str::trim) {
        let parsed = match protocol {
            "pypi" => Protocol::Pypi,
            "rpm" => Protocol::Rpm,
            "apt" => Protocol::Apt,
            "pacman" => Protocol::Pacman,
            _ => {
                return Err(ConfigError::InvalidValue {
                    key: "ENABLED_FRONTENDS",
                    reason: "must be none or a comma-separated list of pypi, rpm, apt, pacman",
                });
            }
        };
        protocols = protocols.with(parsed);
    }
    Ok(protocols)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use super::{Config, ConfigError, LogLevel, Scheme};
    use crate::routing::Protocol;

    fn config(overrides: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let environment = overrides
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>();
        Config::from_maps(&BTreeMap::new(), &environment)
    }

    #[test]
    fn defaults_are_loopback_bounded_and_fail_closed() {
        let config = config(&[]).unwrap();
        assert_eq!(config.listen_addr(), "127.0.0.1:8080");
        assert_eq!(config.upstream().as_str(), "https://ghcr.io");
        assert_eq!(config.allowed_hosts(), &["ghcr.io"]);
        assert_eq!(config.connect_timeout(), Duration::from_secs(5));
        assert_eq!(config.request_timeout(), Duration::from_secs(60));
        assert_eq!(config.log_level(), LogLevel::Info);
        assert_eq!(
            config.enabled_protocols(),
            crate::routing::EnabledProtocols::none()
        );
    }

    #[test]
    fn file_values_are_overridden_by_environment() {
        let file = BTreeMap::from([
            ("UPSTREAM".to_owned(), "https://ghcr.io".to_owned()),
            ("ALLOWED_HOSTS".to_owned(), "ghcr.io".to_owned()),
            ("ENABLED_FRONTENDS".to_owned(), "none".to_owned()),
        ]);
        let environment = BTreeMap::from([
            ("UPSTREAM".to_owned(), "http://127.0.0.1:42317".to_owned()),
            ("ALLOWED_HOSTS".to_owned(), "127.0.0.1".to_owned()),
            ("ALLOW_INSECURE_LOOPBACK".to_owned(), "true".to_owned()),
            ("ENABLED_FRONTENDS".to_owned(), "pypi".to_owned()),
        ]);
        let config = Config::from_maps(&file, &environment).unwrap();
        assert_eq!(config.upstream().scheme(), Scheme::Http);
        assert_eq!(config.upstream().as_str(), "http://127.0.0.1:42317");
        assert!(config.enabled_protocols().contains(Protocol::Pypi));
        assert!(!config.enabled_protocols().contains(Protocol::Rpm));
    }

    #[test]
    fn rejects_public_http_and_upstream_allowlist_mismatch() {
        assert!(matches!(
            config(&[("UPSTREAM", "http://example.invalid")]),
            Err(ConfigError::InvalidValue {
                key: "UPSTREAM",
                ..
            })
        ));
        assert!(matches!(
            config(&[
                ("UPSTREAM", "https://registry.example"),
                ("ALLOWED_HOSTS", "other.example")
            ]),
            Err(ConfigError::InvalidValue {
                key: "ALLOWED_HOSTS",
                ..
            })
        ));
    }

    #[test]
    fn rejects_unsafe_values_and_unknown_protocols() {
        for (key, value) in [
            ("CONNECT_TIMEOUT_MS", "0"),
            ("REQUEST_TIMEOUT_MS", "999999999"),
            ("MAX_TARGET_BYTES", "0"),
            ("MAX_HEADER_COUNT", "999"),
            ("MAX_HEADER_BYTES", "999999"),
            ("LOG_LEVEL", "trace"),
            ("ENABLED_FRONTENDS", "pypi,unknown"),
        ] {
            assert!(
                config(&[(key, value)]).is_err(),
                "{key}={value} should fail"
            );
        }
    }

    #[test]
    fn accepts_explicit_mvp_configuration() {
        let config = config(&[
            ("LISTEN_ADDR", "127.0.0.1:0"),
            ("UPSTREAM", "http://127.0.0.1:42317"),
            ("ALLOWED_HOSTS", "127.0.0.1"),
            ("ALLOW_INSECURE_LOOPBACK", "true"),
            ("ENABLED_FRONTENDS", "pypi"),
            ("MAX_BODY_BYTES", "0"),
        ])
        .unwrap();
        assert_eq!(config.listen_addr(), "127.0.0.1:0");
        assert_eq!(config.upstream().port(), Some(42317));
        assert!(config.enabled_protocols().contains(Protocol::Pypi));
    }

    #[test]
    fn file_parser_rejects_unknown_and_malformed_lines_without_values() {
        assert!(matches!(
            super::parse_config_file("UPSTREAM=https://ghcr.io\nunknown=value\n"),
            Err(ConfigError::UnknownKey)
        ));
        assert!(matches!(
            super::parse_config_file("UPSTREAM\n"),
            Err(ConfigError::FileSyntax { line: 1 })
        ));
    }
}
