//! Scriptable native autoindex CRUD backed by the configured OCI repository.
//!
//! Operations materialize the current visible files into a bounded temporary
//! directory, rebuild a normal OCI layout, and use ORAS for registry writes.
//! The registry remains authoritative and temporary package bytes are removed
//! when the command exits.

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    auth::TokenCredentials,
    autoindex::RelativePath,
    cli::AutoindexCommand,
    config::{Config, Scheme},
    oci::{AUTO_INDEX_REFERENCE, AutoindexSnapshot, Descriptor, OciClient, OciError},
    publisher::{MAX_PUBLICATION_BYTES, Publication},
};
use http_body_util::BodyExt;

const CLI_TEMP_PREFIX: &str = "oras-autoindex-cli";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A credential-safe error from an autoindex CLI operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoindexCliError {
    /// The requested relative title is invalid for autoindex paths.
    InvalidPath,
    /// The source is not a regular file or exceeds publication limits.
    InvalidSource,
    /// Create targeted a path that already exists.
    AlreadyExists,
    /// Update or delete targeted a path that does not exist.
    NotFound,
    /// The configured OCI manifest is not a supported autoindex artifact.
    InvalidArtifact,
    /// A registry request failed or was rejected.
    Registry,
    /// A bounded local staging operation failed.
    TemporaryStorage,
    /// The publisher rejected the resulting file set.
    Publication,
    /// An external ORAS operation failed.
    Oras,
}

impl fmt::Display for AutoindexCliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPath => "autoindex path is invalid",
            Self::InvalidSource => "source must be a regular file within publication limits",
            Self::AlreadyExists => "autoindex path already exists",
            Self::NotFound => "autoindex path does not exist",
            Self::InvalidArtifact => "configured reference is not a valid autoindex artifact",
            Self::Registry => "configured registry operation failed",
            Self::TemporaryStorage => "temporary publication storage failed",
            Self::Publication => "autoindex publication could not be built",
            Self::Oras => "ORAS registry operation failed",
        })
    }
}

impl Error for AutoindexCliError {}

/// Runs one parsed CRUD operation and returns a safe one-line result.
pub async fn execute(
    operation: AutoindexCommand,
    config: &Config,
    client: &OciClient,
) -> Result<String, AutoindexCliError> {
    let (path, dry_run) = match &operation {
        AutoindexCommand::Create { path, dry_run, .. }
        | AutoindexCommand::Update { path, dry_run, .. } => (path.as_str(), *dry_run),
        AutoindexCommand::Delete { path, dry_run, .. } => (path.as_str(), *dry_run),
    };
    let relative = RelativePath::parse(path).map_err(|_| AutoindexCliError::InvalidPath)?;
    if !matches!(operation, AutoindexCommand::Delete { .. }) && relative.is_directory() {
        return Err(AutoindexCliError::InvalidPath);
    }
    if let AutoindexCommand::Create { source, .. } | AutoindexCommand::Update { source, .. } =
        &operation
    {
        validate_source(source)?;
    }

    let current = load_snapshot(config, client).await?;
    let current_paths = current
        .as_ref()
        .map(|snapshot| {
            snapshot
                .objects()
                .map(|object| object.path().as_str().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let affected = match &operation {
        AutoindexCommand::Create { .. } => {
            if current_paths.iter().any(|existing| existing == path) {
                return Err(AutoindexCliError::AlreadyExists);
            }
            1
        }
        AutoindexCommand::Update { .. } => {
            if !current_paths.iter().any(|existing| existing == path) {
                return Err(AutoindexCliError::NotFound);
            }
            1
        }
        AutoindexCommand::Delete { .. } => {
            let matches = matching_paths(path, &current_paths);
            if matches.is_empty() {
                return Err(AutoindexCliError::NotFound);
            }
            matches.len()
        }
    };

    if dry_run {
        let action = match operation {
            AutoindexCommand::Create { .. } => "create",
            AutoindexCommand::Update { .. } => "update",
            AutoindexCommand::Delete { .. } => "delete",
        };
        return Ok(format!("would {action} {path} ({affected} file(s))"));
    }

    let work = TemporaryDirectory::new()?;
    let content_root = work.path().join("content");
    fs::create_dir(&content_root).map_err(|_| AutoindexCliError::TemporaryStorage)?;
    if let Some(snapshot) = current.as_ref() {
        materialize_snapshot(config, client, snapshot, &content_root).await?;
    }

    match &operation {
        AutoindexCommand::Create { source, .. } => {
            copy_source(source, &content_root, &relative)?;
        }
        AutoindexCommand::Update { source, .. } => {
            copy_source(source, &content_root, &relative)?;
        }
        AutoindexCommand::Delete { .. } => {
            for existing in matching_paths(path, &current_paths) {
                let file = path_under(&content_root, &existing)?;
                fs::remove_file(file).map_err(|_| AutoindexCliError::TemporaryStorage)?;
            }
        }
    }

    let remaining = collect_regular_files(&content_root)?;
    if remaining.is_empty() {
        let target = registry_reference(config);
        let mut arguments = vec!["manifest", "delete", "--force"];
        if config.upstream().scheme() == Scheme::Http {
            arguments.push("--plain-http");
        }
        arguments.push(target.as_str());
        run_oras(config, &work, &arguments)?;
    } else {
        let publication = Publication::from_directory(&content_root)
            .map_err(|_| AutoindexCliError::Publication)?;
        let layout = work.path().join("layout");
        publication
            .write_oci_layout(&layout, AUTO_INDEX_REFERENCE)
            .map_err(|_| AutoindexCliError::Publication)?;
        let layout_reference = format!("{}:{AUTO_INDEX_REFERENCE}", layout.display());
        let target = registry_reference(config);
        let mut arguments = vec!["cp", "--from-oci-layout", "--no-tty"];
        if config.upstream().scheme() == Scheme::Http {
            arguments.push("--to-plain-http");
        }
        arguments.push(layout_reference.as_str());
        arguments.push(target.as_str());
        run_oras(config, &work, &arguments)?;
    }

    let action = match operation {
        AutoindexCommand::Create { .. } => "created",
        AutoindexCommand::Update { .. } => "updated",
        AutoindexCommand::Delete { .. } => "deleted",
    };
    Ok(format!("{action} {path} ({affected} file(s))"))
}

async fn load_snapshot(
    config: &Config,
    client: &OciClient,
) -> Result<Option<AutoindexSnapshot>, AutoindexCliError> {
    match client.autoindex_snapshot(config.repository(), None).await {
        Ok(snapshot) if snapshot.is_crud_compatible() => Ok(Some(snapshot)),
        Ok(_) => Err(AutoindexCliError::InvalidArtifact),
        Err(OciError::NotFound) => Ok(None),
        Err(OciError::Unauthorized { .. } | OciError::Forbidden) => {
            Err(AutoindexCliError::Registry)
        }
        Err(_) => Err(AutoindexCliError::InvalidArtifact),
    }
}

async fn materialize_snapshot(
    config: &Config,
    client: &OciClient,
    snapshot: &AutoindexSnapshot,
    root: &Path,
) -> Result<(), AutoindexCliError> {
    let mut total = 0_u64;
    for object in snapshot.objects() {
        total = total
            .checked_add(object.size())
            .filter(|size| *size <= MAX_PUBLICATION_BYTES)
            .ok_or(AutoindexCliError::InvalidArtifact)?;
        let descriptor = Descriptor::new(object.media_type(), object.digest(), object.size());
        let body = client
            .blob(config.repository(), &descriptor, None)
            .await
            .map_err(|_| AutoindexCliError::Registry)?;
        let limit = usize::try_from(object.size().saturating_add(1))
            .map_err(|_| AutoindexCliError::InvalidArtifact)?;
        let collected = tokio::time::timeout(
            config.request_timeout(),
            http_body_util::Limited::new(body, limit).collect(),
        )
        .await
        .map_err(|_| AutoindexCliError::Registry)?
        .map_err(|_| AutoindexCliError::InvalidArtifact)?;
        let bytes = collected.to_bytes();
        if bytes.len() as u64 != object.size() {
            return Err(AutoindexCliError::InvalidArtifact);
        }
        let file = path_under(root, object.path().as_str())?;
        let parent = file.parent().ok_or(AutoindexCliError::InvalidPath)?;
        fs::create_dir_all(parent).map_err(|_| AutoindexCliError::TemporaryStorage)?;
        fs::write(file, bytes).map_err(|_| AutoindexCliError::TemporaryStorage)?;
    }
    Ok(())
}

fn validate_source(source: &Path) -> Result<u64, AutoindexCliError> {
    let metadata = fs::symlink_metadata(source).map_err(|_| AutoindexCliError::InvalidSource)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_PUBLICATION_BYTES
    {
        return Err(AutoindexCliError::InvalidSource);
    }
    Ok(metadata.len())
}

fn copy_source(
    source: &Path,
    content_root: &Path,
    relative: &RelativePath,
) -> Result<(), AutoindexCliError> {
    use std::io::Read;

    let expected_size = validate_source(source)?;
    let destination = path_under(content_root, relative.as_str())?;
    let parent = destination.parent().ok_or(AutoindexCliError::InvalidPath)?;
    fs::create_dir_all(parent).map_err(|_| AutoindexCliError::TemporaryStorage)?;
    let input = fs::File::open(source).map_err(|_| AutoindexCliError::InvalidSource)?;
    let mut output =
        fs::File::create(&destination).map_err(|_| AutoindexCliError::TemporaryStorage)?;
    let copied = std::io::copy(
        &mut input.take(MAX_PUBLICATION_BYTES.saturating_add(1)),
        &mut output,
    )
    .map_err(|_| AutoindexCliError::InvalidSource)?;
    if copied != expected_size || copied > MAX_PUBLICATION_BYTES {
        drop(output);
        let _ = fs::remove_file(destination);
        return Err(AutoindexCliError::InvalidSource);
    }
    Ok(())
}

fn path_under(root: &Path, relative: &str) -> Result<PathBuf, AutoindexCliError> {
    let validated = RelativePath::parse(relative).map_err(|_| AutoindexCliError::InvalidPath)?;
    if validated.is_directory() {
        return Err(AutoindexCliError::InvalidPath);
    }
    let mut path = root.to_path_buf();
    for component in validated.components() {
        path.push(component);
    }
    Ok(path)
}

fn matching_paths(requested: &str, paths: &[String]) -> Vec<String> {
    if requested.ends_with('/') {
        paths
            .iter()
            .filter(|path| path.starts_with(requested))
            .cloned()
            .collect()
    } else {
        paths
            .iter()
            .filter(|path| path.as_str() == requested)
            .cloned()
            .collect()
    }
}

fn collect_regular_files(root: &Path) -> Result<Vec<PathBuf>, AutoindexCliError> {
    fn visit(
        root: &Path,
        current: &Path,
        files: &mut Vec<PathBuf>,
    ) -> Result<(), AutoindexCliError> {
        for entry in fs::read_dir(current).map_err(|_| AutoindexCliError::TemporaryStorage)? {
            let entry = entry.map_err(|_| AutoindexCliError::TemporaryStorage)?;
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|_| AutoindexCliError::TemporaryStorage)?;
            if metadata.file_type().is_symlink() {
                return Err(AutoindexCliError::TemporaryStorage);
            }
            if metadata.is_dir() {
                visit(root, &path, files)?;
            } else if metadata.is_file() {
                files.push(
                    path.strip_prefix(root)
                        .map_err(|_| AutoindexCliError::TemporaryStorage)?
                        .to_owned(),
                );
            } else {
                return Err(AutoindexCliError::TemporaryStorage);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn registry_reference(config: &Config) -> String {
    let origin = config.upstream().as_str();
    let authority = origin
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    format!(
        "{authority}/{}:{AUTO_INDEX_REFERENCE}",
        config.repository().as_str()
    )
}

fn run_oras(
    config: &Config,
    work: &TemporaryDirectory,
    arguments: &[&str],
) -> Result<(), AutoindexCliError> {
    let docker_config = if let Some(credentials) = config.token_credentials() {
        Some(login_oras(config, work, credentials)?)
    } else {
        None
    };
    let mut command = Command::new("oras");
    command
        .args(arguments)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(directory) = docker_config {
        command.env("DOCKER_CONFIG", directory);
    }
    let output = command.output().map_err(|_| AutoindexCliError::Oras)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AutoindexCliError::Oras)
    }
}

fn login_oras(
    config: &Config,
    work: &TemporaryDirectory,
    credentials: &TokenCredentials,
) -> Result<PathBuf, AutoindexCliError> {
    let directory = work.path().join("docker-config");
    fs::create_dir(&directory).map_err(|_| AutoindexCliError::TemporaryStorage)?;
    let origin = config.upstream().as_str();
    let authority = origin
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let mut command = Command::new("oras");
    command
        .arg("login")
        .arg("--username")
        .arg(credentials.username())
        .arg("--password-stdin");
    if config.upstream().scheme() == Scheme::Http {
        command.arg("--plain-http");
    }
    let mut child = command
        .arg(authority)
        .env("DOCKER_CONFIG", &directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| AutoindexCliError::Oras)?;
    let mut stdin = child.stdin.take().ok_or(AutoindexCliError::Oras)?;
    use std::io::Write;
    stdin
        .write_all(credentials.password().as_bytes())
        .and_then(|()| stdin.write_all(b"\n"))
        .map_err(|_| AutoindexCliError::Oras)?;
    drop(stdin);
    let status = child.wait().map_err(|_| AutoindexCliError::Oras)?;
    if status.success() {
        Ok(directory)
    } else {
        Err(AutoindexCliError::Oras)
    }
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new() -> Result<Self, AutoindexCliError> {
        for _ in 0..16 {
            let suffix = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("{CLI_TEMP_PREFIX}-{}-{suffix}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(AutoindexCliError::TemporaryStorage),
            }
        }
        Err(AutoindexCliError::TemporaryStorage)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::matching_paths;

    #[test]
    fn directory_delete_matches_only_explicit_descendants() {
        let paths = vec![
            "simple/index.html".to_owned(),
            "simple/demo/index.html".to_owned(),
            "simple-backup/index.html".to_owned(),
        ];
        assert_eq!(
            matching_paths("simple/", &paths),
            ["simple/index.html", "simple/demo/index.html"]
        );
        assert_eq!(
            matching_paths("simple/index.html", &paths),
            ["simple/index.html"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn temporary_package_data_is_private_to_the_process_user() {
        use std::os::unix::fs::PermissionsExt;

        let directory = super::TemporaryDirectory::new().unwrap();
        let mode = std::fs::metadata(directory.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
        assert_ne!(mode & 0o700, 0);
    }
}
