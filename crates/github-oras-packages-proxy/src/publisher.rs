//! Build standard OCI image layouts from ordinary autoindex files.
//!
//! The publisher has no package-manager knowledge and emits no route map.
//! File names become descriptor titles, the autoindex visibility bit is added
//! to each file layer, and file contents remain content-addressed OCI blobs.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::{Component, Path, PathBuf},
};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::autoindex::{MAX_PATH_SEGMENTS, RelativePath, TITLE_ANNOTATION, VISIBILITY_ANNOTATION};

const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const EMPTY_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";
const EMPTY_CONFIG: &[u8] = b"{}";
const OCI_LAYOUT_VERSION: &str = "1.0.0";
const MAX_PUBLICATION_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PUBLICATION_FILES: usize = 10_000;
const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";

/// A stable publisher failure that never retains caller paths or file contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublisherError {
    /// An input root was missing, not a directory, or was itself a symlink.
    InvalidRoot,
    /// A file or layout operation failed.
    Io,
    /// An input contains a symlink or a non-file/non-directory entry.
    UnsupportedFileType,
    /// An input title is unsafe for OCI autoindex materialization.
    InvalidPath,
    /// The same relative title was supplied more than once.
    DuplicatePath,
    /// The publication exceeds the configured bounded size or file count.
    LimitExceeded,
    /// No files were selected; empty directories have no OCI representation.
    EmptyPublication,
    /// The OCI layout destination already exists.
    DestinationExists,
    /// The OCI reference is not a valid tag spelling.
    InvalidReference,
    /// The command arguments do not match the publisher interface.
    InvalidArguments,
}

impl fmt::Display for PublisherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRoot => "publisher input root is invalid",
            Self::Io => "publisher filesystem operation failed",
            Self::UnsupportedFileType => "publisher accepts regular files and directories only",
            Self::InvalidPath => "publisher file path is not safe for autoindex",
            Self::DuplicatePath => "publisher input contains a duplicate relative path",
            Self::LimitExceeded => "publisher input exceeds configured limits",
            Self::EmptyPublication => "publisher input contains no files",
            Self::DestinationExists => "OCI layout destination already exists",
            Self::InvalidReference => "OCI layout reference is invalid",
            Self::InvalidArguments => "publisher arguments are invalid",
        })
    }
}

impl Error for PublisherError {}

/// One immutable blob included in an OCI publication.
#[derive(Clone, Eq, PartialEq)]
pub struct PublicationBlob {
    digest: String,
    bytes: Vec<u8>,
}

impl fmt::Debug for PublicationBlob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublicationBlob")
            .field("digest", &self.digest)
            .field("size", &self.bytes.len())
            .finish()
    }
}

impl PublicationBlob {
    /// Returns this blob's immutable `sha256:` digest.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Returns the content-addressed bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// A standard OCI manifest and content-addressed blobs ready for layout output.
#[derive(Clone, Eq, PartialEq)]
pub struct Publication {
    manifest: Vec<u8>,
    manifest_digest: String,
    manifest_size: u64,
    blobs: Vec<PublicationBlob>,
    paths: Vec<String>,
}

impl fmt::Debug for Publication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let blob_bytes = self
            .blobs
            .iter()
            .map(|blob| blob.bytes.len())
            .sum::<usize>();
        formatter
            .debug_struct("Publication")
            .field("manifest_digest", &self.manifest_digest)
            .field("manifest_size", &self.manifest_size)
            .field("blob_count", &self.blobs.len())
            .field("blob_bytes", &blob_bytes)
            .field("path_count", &self.paths.len())
            .finish()
    }
}

impl Publication {
    /// Builds one autoindex-visible artifact from every regular file below `root`.
    ///
    /// Files are sorted by relative path for deterministic manifest bytes.
    /// Empty directories are ignored because OCI descriptors name objects, not
    /// directories; any symlink aborts the publication rather than being followed.
    pub fn from_directory(root: impl AsRef<Path>) -> Result<Self, PublisherError> {
        let root = root.as_ref();
        validate_root(root)?;
        let mut relative_files = Vec::new();
        collect_files(root, root, &mut relative_files, 0)?;
        relative_files.sort();
        Self::from_files(root, relative_files)
    }

    /// Builds an artifact from an explicit set of relative file paths.
    ///
    /// Duplicate names, paths escaping `root`, symlinks, non-files, and unsafe
    /// autoindex titles are rejected. This is useful when a caller selects only
    /// generated package indexes and archives from a larger work directory.
    pub fn from_files<I, P>(root: impl AsRef<Path>, paths: I) -> Result<Self, PublisherError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let root = root.as_ref();
        validate_root(root)?;
        let mut titles = BTreeSet::new();
        let mut file_blobs = BTreeMap::<String, PublicationBlob>::new();
        let mut layers = Vec::new();
        let mut total_bytes = 0_u64;

        for relative in paths {
            let relative = relative.as_ref();
            let title = relative_title(relative)?;
            RelativePath::parse(&title).map_err(|_| PublisherError::InvalidPath)?;
            if !titles.insert(title.clone()) {
                return Err(PublisherError::DuplicatePath);
            }
            if titles.len() > MAX_PUBLICATION_FILES {
                return Err(PublisherError::LimitExceeded);
            }

            let source = validate_source_path(root, relative)?;
            let metadata = fs::symlink_metadata(&source).map_err(|_| PublisherError::Io)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(PublisherError::UnsupportedFileType);
            }
            let file_size = metadata.len();
            if file_size > MAX_PUBLICATION_BYTES
                || total_bytes
                    .checked_add(file_size)
                    .is_none_or(|size| size > MAX_PUBLICATION_BYTES)
            {
                return Err(PublisherError::LimitExceeded);
            }

            let bytes = fs::read(&source).map_err(|_| PublisherError::Io)?;
            if bytes.len() as u64 != file_size {
                return Err(PublisherError::Io);
            }
            let digest = digest_bytes(&bytes);
            if !file_blobs.contains_key(&digest) {
                total_bytes += file_size;
                file_blobs.insert(
                    digest.clone(),
                    PublicationBlob {
                        digest: digest.clone(),
                        bytes,
                    },
                );
            }
            let mut annotations = BTreeMap::new();
            annotations.insert(TITLE_ANNOTATION.to_owned(), title.clone());
            annotations.insert(VISIBILITY_ANNOTATION.to_owned(), "true".to_owned());
            layers.push(Descriptor {
                media_type: media_type_for(&title).to_owned(),
                digest,
                size: file_size,
                annotations,
            });
        }

        if layers.is_empty() {
            return Err(PublisherError::EmptyPublication);
        }
        layers.sort_by(|left, right| {
            left.annotations
                .get(TITLE_ANNOTATION)
                .cmp(&right.annotations.get(TITLE_ANNOTATION))
        });
        let paths = layers
            .iter()
            .filter_map(|layer| layer.annotations.get(TITLE_ANNOTATION).cloned())
            .collect::<Vec<_>>();
        let config_bytes = EMPTY_CONFIG.to_vec();
        let config = Descriptor::plain(
            EMPTY_CONFIG_MEDIA_TYPE,
            digest_bytes(&config_bytes),
            config_bytes.len() as u64,
        );
        let manifest = Manifest {
            schema_version: 2,
            media_type: MANIFEST_MEDIA_TYPE,
            config,
            layers,
        };
        let manifest = serde_json::to_vec(&manifest).map_err(|_| PublisherError::Io)?;
        let manifest_digest = digest_bytes(&manifest);
        let manifest_size = manifest.len() as u64;
        let mut blobs = file_blobs.into_values().collect::<Vec<_>>();
        blobs.push(PublicationBlob {
            digest: digest_bytes(config_bytes.as_slice()),
            bytes: config_bytes,
        });
        blobs.sort_by(|left, right| left.digest.cmp(&right.digest));

        Ok(Self {
            manifest,
            manifest_digest,
            manifest_size,
            blobs,
            paths,
        })
    }

    /// Returns the raw standard OCI image manifest.
    pub fn manifest(&self) -> &[u8] {
        &self.manifest
    }

    /// Returns the raw manifest's `sha256:` digest.
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    /// Returns content blobs, including the empty config blob.
    pub fn blobs(&self) -> &[PublicationBlob] {
        &self.blobs
    }

    /// Returns visible descriptor titles in deterministic order.
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// Writes an OCI image layout that ORAS can copy to a registry.
    ///
    /// The destination must not already exist. The reference is stored using
    /// the standard `org.opencontainers.image.ref.name` index annotation.
    pub fn write_oci_layout(
        &self,
        destination: impl AsRef<Path>,
        reference: &str,
    ) -> Result<(), PublisherError> {
        validate_reference(reference)?;
        let destination = destination.as_ref();
        fs::create_dir(destination).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                PublisherError::DestinationExists
            } else {
                PublisherError::Io
            }
        })?;
        let result = self.write_layout_contents(destination, reference);
        if result.is_err() {
            let _ = fs::remove_dir_all(destination);
        }
        result
    }

    fn write_layout_contents(
        &self,
        destination: &Path,
        reference: &str,
    ) -> Result<(), PublisherError> {
        let blob_directory = destination.join("blobs").join("sha256");
        fs::create_dir_all(&blob_directory).map_err(|_| PublisherError::Io)?;
        for blob in &self.blobs {
            write_blob(&blob_directory, &blob.digest, &blob.bytes)?;
        }
        write_blob(&blob_directory, &self.manifest_digest, &self.manifest)?;
        fs::write(
            destination.join("oci-layout"),
            serde_json::to_vec(&OciLayout {
                image_layout_version: OCI_LAYOUT_VERSION,
            })
            .map_err(|_| PublisherError::Io)?,
        )
        .map_err(|_| PublisherError::Io)?;
        let mut annotations = BTreeMap::new();
        annotations.insert(REF_NAME_ANNOTATION.to_owned(), reference.to_owned());
        let index = LayoutIndex {
            schema_version: 2,
            manifests: vec![LayoutDescriptor {
                media_type: MANIFEST_MEDIA_TYPE,
                digest: self.manifest_digest.clone(),
                size: self.manifest_size,
                annotations,
            }],
        };
        fs::write(
            destination.join("index.json"),
            serde_json::to_vec(&index).map_err(|_| PublisherError::Io)?,
        )
        .map_err(|_| PublisherError::Io)
    }
}

#[derive(Serialize)]
struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
}

impl Descriptor {
    fn plain(media_type: &str, digest: String, size: u64) -> Self {
        Self {
            media_type: media_type.to_owned(),
            digest,
            size,
            annotations: BTreeMap::new(),
        }
    }
}

#[derive(Serialize)]
struct Manifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u8,
    #[serde(rename = "mediaType")]
    media_type: &'static str,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Serialize)]
struct OciLayout {
    #[serde(rename = "imageLayoutVersion")]
    image_layout_version: &'static str,
}

#[derive(Serialize)]
struct LayoutIndex {
    #[serde(rename = "schemaVersion")]
    schema_version: u8,
    manifests: Vec<LayoutDescriptor>,
}

#[derive(Serialize)]
struct LayoutDescriptor {
    #[serde(rename = "mediaType")]
    media_type: &'static str,
    digest: String,
    size: u64,
    annotations: BTreeMap<String, String>,
}

fn validate_root(root: &Path) -> Result<(), PublisherError> {
    let metadata = fs::symlink_metadata(root).map_err(|_| PublisherError::InvalidRoot)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PublisherError::InvalidRoot);
    }
    Ok(())
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<(), PublisherError> {
    if depth > MAX_PATH_SEGMENTS {
        return Err(PublisherError::LimitExceeded);
    }
    let entries = fs::read_dir(directory).map_err(|_| PublisherError::Io)?;
    let mut entries = entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PublisherError::Io)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|_| PublisherError::Io)?;
        if metadata.file_type().is_symlink() {
            return Err(PublisherError::UnsupportedFileType);
        }
        if metadata.is_dir() {
            collect_files(root, &path, files, depth + 1)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| PublisherError::InvalidPath)?;
            relative_title(relative)?;
            files.push(relative.to_owned());
            if files.len() > MAX_PUBLICATION_FILES {
                return Err(PublisherError::LimitExceeded);
            }
        } else {
            return Err(PublisherError::UnsupportedFileType);
        }
    }
    Ok(())
}

fn relative_title(path: &Path) -> Result<String, PublisherError> {
    if path.is_absolute() {
        return Err(PublisherError::InvalidPath);
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str().ok_or(PublisherError::InvalidPath)?;
                parts.push(part);
            }
            _ => return Err(PublisherError::InvalidPath),
        }
    }
    if parts.is_empty() {
        return Err(PublisherError::InvalidPath);
    }
    Ok(parts.join("/"))
}

fn validate_source_path(root: &Path, relative: &Path) -> Result<PathBuf, PublisherError> {
    let title = relative_title(relative)?;
    RelativePath::parse(&title).map_err(|_| PublisherError::InvalidPath)?;
    let mut current = root.to_owned();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(PublisherError::InvalidPath);
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current).map_err(|_| PublisherError::Io)?;
        if metadata.file_type().is_symlink() {
            return Err(PublisherError::UnsupportedFileType);
        }
    }
    Ok(current)
}

fn validate_reference(reference: &str) -> Result<(), PublisherError> {
    let mut bytes = reference.bytes();
    let first = bytes.next().ok_or(PublisherError::InvalidReference)?;
    if reference.len() > 128
        || !first.is_ascii_alphanumeric() && first != b'_'
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        return Err(PublisherError::InvalidReference);
    }
    Ok(())
}

fn media_type_for(title: &str) -> &'static str {
    let lower = title.to_ascii_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        "text/html"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".xml") {
        "application/xml"
    } else if lower.ends_with(".txt") || lower.ends_with(".list") {
        "text/plain; charset=utf-8"
    } else if lower.ends_with(".rpm") {
        "application/vnd.rpm"
    } else if lower.ends_with(".deb") {
        "application/vnd.debian.binary-package"
    } else if lower.ends_with(".zst") {
        "application/zstd"
    } else if lower.ends_with(".gz") {
        "application/gzip"
    } else {
        "application/octet-stream"
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn write_blob(directory: &Path, digest: &str, bytes: &[u8]) -> Result<(), PublisherError> {
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or(PublisherError::InvalidPath)?;
    let path = directory.join(hex);
    fs::write(path, bytes).map_err(|_| PublisherError::Io)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use serde_json::Value;

    use super::{Publication, PublisherError, VISIBILITY_ANNOTATION};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let suffix = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oras-publisher-test-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("temporary test directory should be created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn builds_deterministic_visible_manifest_and_oci_layout() {
        let temp = TempDir::new();
        let root = temp.path().join("files");
        fs::create_dir_all(root.join("simple/demo")).unwrap();
        fs::create_dir_all(root.join("packages")).unwrap();
        fs::write(
            root.join("simple/demo/index.html"),
            b"<a href='../../packages/demo.whl'>demo</a>",
        )
        .unwrap();
        fs::write(root.join("packages/demo.whl"), b"wheel bytes").unwrap();
        fs::create_dir_all(root.join("empty-directory")).unwrap();

        let publication = Publication::from_directory(&root).unwrap();
        assert!(!format!("{publication:?}").contains("wheel bytes"));
        assert!(!format!("{:?}", publication.blobs()).contains("wheel bytes"));
        assert_eq!(
            publication.paths(),
            &[
                "packages/demo.whl".to_owned(),
                "simple/demo/index.html".to_owned()
            ]
        );
        let manifest: Value = serde_json::from_slice(publication.manifest()).unwrap();
        assert_eq!(manifest["schemaVersion"], 2);
        assert_eq!(
            manifest["mediaType"],
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(
            manifest["layers"][1]["annotations"]["org.opencontainers.image.title"],
            "simple/demo/index.html"
        );
        assert_eq!(
            manifest["layers"][1]["annotations"][VISIBILITY_ANNOTATION],
            "true"
        );
        assert_eq!(
            manifest["config"]["mediaType"],
            "application/vnd.oci.empty.v1+json"
        );

        let layout = temp.path().join("layout");
        publication
            .write_oci_layout(&layout, "autoindex.v1")
            .unwrap();
        let index: Value =
            serde_json::from_slice(&fs::read(layout.join("index.json")).unwrap()).unwrap();
        assert_eq!(
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
            "autoindex.v1"
        );
        assert!(
            layout
                .join("blobs/sha256")
                .join(&publication.manifest_digest()[7..])
                .exists()
        );
        assert_eq!(
            Publication::from_directory(&root).unwrap().manifest(),
            publication.manifest()
        );
    }

    #[test]
    fn rejects_duplicates_unsafe_paths_and_empty_publications() {
        let temp = TempDir::new();
        let root = temp.path().join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("hello.txt"), b"hello").unwrap();
        assert_eq!(
            Publication::from_files(&root, ["hello.txt", "hello.txt"]),
            Err(PublisherError::DuplicatePath)
        );
        assert_eq!(
            Publication::from_files(&root, ["../hello.txt"]),
            Err(PublisherError::InvalidPath)
        );
        let empty_root = temp.path().join("empty");
        fs::create_dir(&empty_root).unwrap();
        assert_eq!(
            Publication::from_directory(&empty_root),
            Err(PublisherError::EmptyPublication)
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let root = temp.path().join("files");
        fs::create_dir_all(&root).unwrap();
        fs::write(temp.path().join("outside.txt"), b"outside").unwrap();
        symlink(temp.path().join("outside.txt"), root.join("linked.txt")).unwrap();
        assert_eq!(
            Publication::from_directory(&root),
            Err(PublisherError::UnsupportedFileType)
        );
    }
}
