use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use color_eyre::eyre::{Result, bail};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobHash(String);

impl BlobHash {
    pub fn new(value: String) -> Result<Self> {
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            Ok(Self(value))
        } else {
            bail!("blob hash must be exactly 64 lowercase hex characters")
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug)]
pub struct StoreRoot {
    path: PathBuf,
}

impl StoreRoot {
    pub fn validate(path: &Path) -> Result<Self> {
        if path.exists() {
            let canonical = path.canonicalize().map_err(|error| {
                color_eyre::eyre::eyre!("canonicalize store {:?}: {error}", path)
            })?;
            if !canonical.is_dir() {
                bail!("store path exists but is not a directory: {:?}", canonical);
            }
            return Ok(Self { path: canonical });
        }

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| color_eyre::eyre::eyre!("store path must have an existing parent"))?;
        let canonical_parent = parent.canonicalize().map_err(|error| {
            color_eyre::eyre::eyre!("canonicalize store parent {:?}: {error}", parent)
        })?;
        let file_name = path
            .file_name()
            .ok_or_else(|| color_eyre::eyre::eyre!("store path must name a directory"))?;
        Ok(Self {
            path: canonical_parent.join(file_name),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn blobs_dir(&self) -> PathBuf {
        self.path.join("blobs")
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.path.join("staging")
    }

    pub fn default_db_path(&self) -> PathBuf {
        self.path.join("catalog.sqlite")
    }

    pub fn blob_path(&self, hash: &BlobHash) -> PathBuf {
        self.blobs_dir()
            .join(&hash.as_str()[0..2])
            .join(&hash.as_str()[2..4])
            .join(hash.as_str())
    }

    pub fn staging_path(&self, name: &StagingFileName) -> PathBuf {
        self.staging_dir().join(name.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct SourceRoot {
    path: PathBuf,
}

impl SourceRoot {
    pub fn validate(path: &Path) -> Result<Self> {
        let canonical = path
            .canonicalize()
            .map_err(|error| color_eyre::eyre::eyre!("canonicalize source {:?}: {error}", path))?;
        if !canonical.is_dir() {
            bail!("source must be an existing directory: {:?}", canonical);
        }
        Ok(Self { path: canonical })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn durable_text(&self) -> Result<String> {
        path_to_utf8(&self.path, "source root")
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceRelativePath(String);

impl SourceRelativePath {
    pub fn from_path(path: &Path) -> Result<Self> {
        if path.as_os_str().is_empty() || path.is_absolute() {
            bail!(
                "source relative path must be non-empty and relative: {:?}",
                path
            );
        }

        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(value) => {
                    let text = os_str_to_utf8(value, "source relative path component")?;
                    parts.push(text);
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!("invalid source relative path component in {:?}", path);
                }
            }
        }

        if parts.is_empty() {
            bail!("source relative path must not be empty");
        }

        Ok(Self(parts.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct StagingFileName(String);

impl StagingFileName {
    pub fn new() -> Self {
        Self(format!("{}.tmp", Uuid::new_v4()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for StagingFileName {
    fn default() -> Self {
        Self::new()
    }
}

pub fn validate_db_path(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| color_eyre::eyre::eyre!("database path must have an existing parent"))?;
    let canonical_parent = parent.canonicalize().map_err(|error| {
        color_eyre::eyre::eyre!("canonicalize database parent {:?}: {error}", parent)
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| color_eyre::eyre::eyre!("database path must name a file"))?;
    Ok(canonical_parent.join(file_name))
}

pub fn validate_no_overlaps(
    source_root: &SourceRoot,
    store_root: &StoreRoot,
    db_path: &Path,
    db_was_explicit: bool,
) -> Result<()> {
    if paths_overlap(source_root.path(), store_root.path()) {
        bail!(
            "source and store paths must not overlap: source={:?}, store={:?}",
            source_root.path(),
            store_root.path()
        );
    }

    if db_path.starts_with(source_root.path()) {
        bail!(
            "database path must not be inside source directory: db={:?}, source={:?}",
            db_path,
            source_root.path()
        );
    }

    if db_was_explicit && !store_root.path().exists() && db_path.starts_with(store_root.path()) {
        bail!(
            "explicit database path must not be inside a not-yet-existing store path: db={:?}, store={:?}",
            db_path,
            store_root.path()
        );
    }

    Ok(())
}

pub fn path_to_utf8(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| color_eyre::eyre::eyre!("{label} contains non-UTF-8 path data: {:?}", path))
}

fn os_str_to_utf8(value: &OsStr, label: &str) -> Result<String> {
    value
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| color_eyre::eyre::eyre!("{label} contains non-UTF-8 path data: {:?}", value))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}
