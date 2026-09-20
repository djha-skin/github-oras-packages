//! Standard OCI Distribution read-path parsing and proxy dispatch helpers.

use std::fmt;

/// A validated repository-scoped OCI read target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DistributionTarget {
    /// The registry version check endpoint.
    Version,
    /// A manifest reference, which may be a tag or digest.
    Manifest {
        repository: String,
        reference: String,
    },
    /// A content-addressed blob digest.
    Blob { repository: String, digest: String },
}

/// A safe parser error for OCI Distribution targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DistributionPathError {
    /// The path is not a supported exact OCI route.
    Invalid,
    /// The path names a repository other than the configured one.
    RepositoryMismatch,
}

impl fmt::Display for DistributionPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "invalid_distribution_path",
            Self::RepositoryMismatch => "repository_mismatch",
        })
    }
}

impl std::error::Error for DistributionPathError {}

/// Parses the exact `/v2/` read paths for one configured repository.
pub fn parse_path(
    path: &str,
    repository: &str,
) -> Result<DistributionTarget, DistributionPathError> {
    if path == "/v2/" {
        return Ok(DistributionTarget::Version);
    }
    let remainder = path
        .strip_prefix("/v2/")
        .ok_or(DistributionPathError::Invalid)?;
    let (path_repository, operation) = remainder
        .split_once("/manifests/")
        .map(|(repo, reference)| (repo, format!("manifests/{reference}")))
        .or_else(|| {
            remainder
                .split_once("/blobs/")
                .map(|(repo, digest)| (repo, format!("blobs/{digest}")))
        })
        .ok_or(DistributionPathError::Invalid)?;
    if path_repository != repository {
        return Err(DistributionPathError::RepositoryMismatch);
    }
    if path_repository.is_empty() || operation.contains(['?', '#', '%', '\\']) {
        return Err(DistributionPathError::Invalid);
    }
    let (kind, value) = operation
        .split_once('/')
        .ok_or(DistributionPathError::Invalid)?;
    if value.is_empty() || value.contains('/') || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(DistributionPathError::Invalid);
    }
    match kind {
        "manifests" if valid_reference(value) => Ok(DistributionTarget::Manifest {
            repository: path_repository.to_owned(),
            reference: value.to_owned(),
        }),
        "blobs" if valid_digest(value) => Ok(DistributionTarget::Blob {
            repository: path_repository.to_owned(),
            digest: value.to_owned(),
        }),
        _ => Err(DistributionPathError::Invalid),
    }
}

fn valid_reference(value: &str) -> bool {
    value.len() <= 256
        && value.is_ascii()
        && !value.is_empty()
        && !value.starts_with('.')
        && !value.ends_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

fn valid_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::{DistributionPathError, DistributionTarget, parse_path};

    #[test]
    fn parses_version_manifest_and_blob_paths_for_literal_repository() {
        assert_eq!(
            parse_path("/v2/", "acme/fixture"),
            Ok(DistributionTarget::Version)
        );
        assert_eq!(
            parse_path("/v2/acme/fixture/manifests/autoindex.v1", "acme/fixture"),
            Ok(DistributionTarget::Manifest {
                repository: "acme/fixture".to_owned(),
                reference: "autoindex.v1".to_owned()
            })
        );
        assert_eq!(
            parse_path(
                "/v2/acme/fixture/blobs/sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "acme/fixture"
            ),
            Ok(DistributionTarget::Blob {
                repository: "acme/fixture".to_owned(),
                digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_owned()
            })
        );
    }

    #[test]
    fn rejects_other_repositories_and_ambiguous_paths() {
        assert_eq!(
            parse_path("/v2/acme/other/manifests/demo", "acme/fixture"),
            Err(DistributionPathError::RepositoryMismatch)
        );
        for path in [
            "/v2/acme/fixture/manifests/",
            "/v2/acme/fixture/blobs/md5:abc",
            "/v2/acme/fixture/blobs/sha256:0000/extra",
            "/v2/acme/fixture/manifests/demo%2Fother",
            "/v1/",
        ] {
            assert_eq!(
                parse_path(path, "acme/fixture"),
                Err(DistributionPathError::Invalid)
            );
        }
    }
}
