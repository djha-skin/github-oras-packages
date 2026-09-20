//! Human-readable autoindex path validation and deterministic directory projection.
//!
//! This module is deliberately independent of package-manager semantics and
//! route maps. OCI descriptors provide relative materialization names; this
//! module validates those names and projects them into a filesystem-like HTTP
//! namespace.

use std::{collections::BTreeMap, fmt};

/// Maximum relative path length accepted by the autoindex projection.
pub const MAX_PATH_BYTES: usize = 4_096;
/// Maximum number of path components accepted by the autoindex projection.
pub const MAX_PATH_SEGMENTS: usize = 128;
/// The namespaced annotation that opts a descriptor into autoindex browsing.
pub const VISIBILITY_ANNOTATION: &str =
    "io.github.djha-skin.github-oras-packages.autoindex.visible";
/// The OCI title annotation used as the visible relative path.
pub const TITLE_ANNOTATION: &str = "org.opencontainers.image.title";

/// A validated relative path for a visible autoindex object.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RelativePath(String);

impl RelativePath {
    /// Validates a descriptor title or request path without normalizing it.
    pub fn parse(value: &str) -> Result<Self, PathError> {
        if value.is_empty()
            || value.len() > MAX_PATH_BYTES
            || !value.is_ascii()
            || value.starts_with('/')
            || value.contains(['?', '#', '%', '\\', ';'])
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(PathError::Invalid);
        }

        let segments = value.split('/').collect::<Vec<_>>();
        let trailing_slash = value.ends_with('/');
        let meaningful = if trailing_slash {
            &segments[..segments.len() - 1]
        } else {
            &segments[..]
        };
        if meaningful.is_empty()
            || meaningful.len() > MAX_PATH_SEGMENTS
            || meaningful
                .iter()
                .any(|segment| segment.is_empty() || matches!(*segment, "." | ".."))
        {
            return Err(PathError::Invalid);
        }

        Ok(Self(value.to_owned()))
    }

    /// Returns the exact validated spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reports whether this path is a directory spelling.
    pub fn is_directory(&self) -> bool {
        self.0.ends_with('/')
    }

    /// Returns the path's components without its optional trailing slash.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|component| !component.is_empty())
    }
}

/// A validated visible OCI object projected into autoindex.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleObject {
    path: RelativePath,
    digest: String,
    media_type: String,
    size: u64,
}

impl VisibleObject {
    /// Creates a visible object after validating its relative title.
    pub fn new(
        path: impl AsRef<str>,
        digest: impl Into<String>,
        media_type: impl Into<String>,
        size: u64,
    ) -> Result<Self, PathError> {
        let path = RelativePath::parse(path.as_ref())?;
        if path.is_directory() {
            return Err(PathError::Invalid);
        }
        Ok(Self {
            path,
            digest: digest.into(),
            media_type: media_type.into(),
            size,
        })
    }

    /// Returns the object's relative autoindex path.
    pub fn path(&self) -> &RelativePath {
        &self.path
    }

    /// Returns the content-addressed blob digest.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Returns the OCI media type.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Returns the advertised content length.
    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// A deterministic filesystem-like projection of visible objects.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Index {
    objects: BTreeMap<RelativePath, VisibleObject>,
}

impl Index {
    /// Inserts an object and rejects duplicate or file/directory collisions.
    pub fn insert(&mut self, object: VisibleObject) -> Result<(), PathError> {
        let path = object.path().clone();
        if self.objects.contains_key(&path) {
            return Err(PathError::Duplicate);
        }
        let path_without_slash = path.as_str().trim_end_matches('/');
        for existing in self.objects.keys() {
            let existing_without_slash = existing.as_str().trim_end_matches('/');
            if existing_without_slash == path_without_slash
                || existing_without_slash.starts_with(&format!("{path_without_slash}/"))
                || path_without_slash.starts_with(&format!("{existing_without_slash}/"))
            {
                return Err(PathError::Collision);
            }
        }
        self.objects.insert(path, object);
        Ok(())
    }

    /// Finds a visible object by its exact file path.
    pub fn object(&self, path: &str) -> Option<&VisibleObject> {
        self.objects.get(&RelativePath(path.to_owned()))
    }

    /// Returns direct children under a directory, sorted lexicographically.
    pub fn children(&self, directory: &str) -> Result<Vec<DirectoryEntry>, PathError> {
        let directory = if directory.is_empty() {
            String::new()
        } else {
            let path = RelativePath::parse(directory)?;
            if !path.is_directory() {
                return Err(PathError::DirectoryNeedsTrailingSlash);
            }
            path.as_str().to_owned()
        };
        let mut entries = BTreeMap::<String, DirectoryEntry>::new();
        for object in self.objects.values() {
            let Some(remainder) = object.path().as_str().strip_prefix(&directory) else {
                continue;
            };
            if remainder.is_empty() {
                continue;
            }
            let remainder = remainder.trim_end_matches('/');
            let Some((name, rest)) = remainder.split_once('/') else {
                if !remainder.is_empty() {
                    entries.insert(remainder.to_owned(), DirectoryEntry::File(object.clone()));
                }
                continue;
            };
            if !name.is_empty() && !rest.is_empty() {
                entries
                    .entry(name.to_owned())
                    .or_insert_with(|| DirectoryEntry::Directory(format!("{directory}{name}/")));
            }
        }
        Ok(entries.into_values().collect())
    }

    /// Returns all visible objects in deterministic path order.
    pub fn objects(&self) -> impl Iterator<Item = &VisibleObject> {
        self.objects.values()
    }
}

/// A direct child displayed in an autoindex directory listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectoryEntry {
    /// A nested directory, including its trailing slash.
    Directory(String),
    /// A visible file object.
    File(VisibleObject),
}

impl DirectoryEntry {
    /// Returns the displayed child name.
    pub fn name(&self) -> &str {
        match self {
            Self::Directory(path) => path.rsplit('/').nth(1).unwrap_or_default(),
            Self::File(object) => object
                .path()
                .as_str()
                .rsplit('/')
                .next()
                .unwrap_or_default(),
        }
    }

    /// Returns the child URL path relative to the service root.
    pub fn path(&self) -> &str {
        match self {
            Self::Directory(path) => path,
            Self::File(object) => object.path().as_str(),
        }
    }
}

/// A safe path/projection error with no caller input retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathError {
    /// The path is empty, absolute, ambiguous, or unsafe.
    Invalid,
    /// The same visible path was inserted twice.
    Duplicate,
    /// A file and directory would occupy the same path namespace.
    Collision,
    /// A directory operation omitted the required trailing slash.
    DirectoryNeedsTrailingSlash,
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "invalid_autoindex_path",
            Self::Duplicate => "duplicate_autoindex_path",
            Self::Collision => "autoindex_path_collision",
            Self::DirectoryNeedsTrailingSlash => "directory_needs_trailing_slash",
        })
    }
}

impl std::error::Error for PathError {}

#[cfg(test)]
mod tests {
    use super::{DirectoryEntry, Index, PathError, RelativePath, VisibleObject};

    fn object(path: &str) -> VisibleObject {
        VisibleObject::new(path, "sha256:digest", "application/octet-stream", 1).unwrap()
    }

    #[test]
    fn preserves_human_readable_paths_without_normalizing() {
        let path = RelativePath::parse("pypi/simple/capturepkg/index.html").unwrap();
        assert_eq!(path.as_str(), "pypi/simple/capturepkg/index.html");
        assert_eq!(
            path.components().collect::<Vec<_>>(),
            ["pypi", "simple", "capturepkg", "index.html"]
        );
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_paths() {
        for path in [
            "", "/root", "a//b", "a/./b", "a/../b", "a%2Fb", "a\\b", "a?b", "a#b",
        ] {
            assert_eq!(
                RelativePath::parse(path),
                Err(PathError::Invalid),
                "{path:?}"
            );
        }
        assert!(RelativePath::parse("directory/").unwrap().is_directory());
    }

    #[test]
    fn projects_direct_files_and_nested_directories_in_order() {
        let mut index = Index::default();
        index.insert(object("pypi/simple/index.html")).unwrap();
        index
            .insert(object("pypi/simple/capturepkg/index.html"))
            .unwrap();
        index.insert(object("README.txt")).unwrap();

        let root = index.children("").unwrap();
        assert_eq!(root.len(), 2);
        assert!(
            matches!(&root[0], DirectoryEntry::File(file) if file.path().as_str() == "README.txt")
        );
        assert!(matches!(&root[1], DirectoryEntry::Directory(path) if path == "pypi/"));
        let pypi = index.children("pypi/").unwrap();
        assert_eq!(pypi.len(), 1);
        assert_eq!(pypi[0].path(), "pypi/simple/");
    }

    #[test]
    fn rejects_duplicates_and_file_directory_collisions() {
        let mut index = Index::default();
        index.insert(object("a")).unwrap();
        assert_eq!(index.insert(object("a")), Err(PathError::Duplicate));
        assert_eq!(index.insert(object("a/b")), Err(PathError::Collision));
    }
}
