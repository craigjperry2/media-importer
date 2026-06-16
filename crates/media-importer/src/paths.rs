use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
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

    pub fn validate_existing_for_build_tree(path: &Path) -> Result<Self> {
        let link_metadata = std::fs::symlink_metadata(path)
            .wrap_err_with(|| format!("stat store path {:?}", path))?;
        if link_metadata.file_type().is_symlink() {
            bail!(
                "store path must be a real directory, not a symlink: {:?}",
                path
            );
        }
        if !link_metadata.is_dir() {
            bail!("store path must be an existing directory: {:?}", path);
        }
        let canonical = path
            .canonicalize()
            .wrap_err_with(|| format!("canonicalize store {:?}", path))?;
        let root = Self { path: canonical };
        validate_real_directory(&root.blobs_dir(), "store blobs directory")?;
        Ok(root)
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
pub struct BrowseTreeRoot {
    path: PathBuf,
}

impl BrowseTreeRoot {
    pub fn validate(path: &Path) -> Result<Self> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "browse tree path must be a real directory, not a symlink: {:?}",
                        path
                    );
                }
                if !metadata.is_dir() {
                    bail!("browse tree path exists but is not a directory: {:?}", path);
                }
                Ok(Self {
                    path: path
                        .canonicalize()
                        .wrap_err_with(|| format!("canonicalize browse tree {:?}", path))?,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .ok_or_else(|| eyre!("browse tree path must have an existing parent"))?;
                validate_real_directory(parent, "browse tree parent")?;
                let canonical_parent = parent
                    .canonicalize()
                    .wrap_err_with(|| format!("canonicalize browse tree parent {:?}", parent))?;
                let file_name = path
                    .file_name()
                    .ok_or_else(|| eyre!("browse tree path must name a directory"))?;
                Ok(Self {
                    path: canonical_parent.join(file_name),
                })
            }
            Err(error) => Err(error).wrap_err_with(|| format!("stat browse tree path {:?}", path)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
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

    pub fn from_catalog_text(value: &str) -> Result<Self> {
        if value.is_empty()
            || value.starts_with('/')
            || value.starts_with("//")
            || value.contains('\\')
            || value
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || looks_like_windows_drive(value)
        {
            bail!("invalid catalog relative path: {value:?}");
        }
        Ok(Self(value.to_owned()))
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

pub fn validate_build_tree_no_overlaps(
    store_root: &StoreRoot,
    browse_tree_root: &BrowseTreeRoot,
    db_path: &Path,
    db_was_explicit: bool,
) -> Result<()> {
    if paths_overlap(store_root.path(), browse_tree_root.path()) {
        bail!(
            "store and browse tree paths must not overlap: store={:?}, browse_tree={:?}",
            store_root.path(),
            browse_tree_root.path()
        );
    }
    if db_was_explicit && db_path.starts_with(browse_tree_root.path()) {
        bail!(
            "database path must not be inside browse tree: db={:?}, browse_tree={:?}",
            db_path,
            browse_tree_root.path()
        );
    }
    Ok(())
}

pub fn suffix_filename_with_hash(filename: &str, hash: &BlobHash, digits: usize) -> String {
    let prefix = &hash.as_str()[..digits];
    match filename.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => {
            format!("{stem}_{prefix}.{extension}")
        }
        _ => format!("{filename}_{prefix}"),
    }
}

pub fn output_relative_path(
    relative_path: &SourceRelativePath,
    hash: &BlobHash,
    digits: usize,
) -> PathBuf {
    let mut components: Vec<&str> = relative_path.as_str().split('/').collect();
    let filename = components
        .pop()
        .expect("validated relative path is non-empty");
    let suffixed = suffix_filename_with_hash(filename, hash, digits);
    components.into_iter().collect::<PathBuf>().join(suffixed)
}

pub fn relative_symlink_target(from_parent: &Path, to_file: &Path) -> Result<PathBuf> {
    if !from_parent.is_absolute() || !to_file.is_absolute() {
        bail!("relative symlink target inputs must be absolute paths");
    }
    let from = path_components_without_root(from_parent)?;
    let to = path_components_without_root(to_file)?;
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = PathBuf::new();
    for _ in common..from.len() {
        result.push("..");
    }
    for component in &to[common..] {
        result.push(component);
    }
    if result.as_os_str().is_empty() {
        Ok(PathBuf::from("."))
    } else {
        Ok(result)
    }
}

pub fn normalized_symlink_target(symlink_parent: &Path, target_text: &Path) -> Result<PathBuf> {
    let start = if target_text.is_absolute() {
        PathBuf::new()
    } else {
        symlink_parent.to_path_buf()
    };
    lexical_normalize(&start.join(target_text))
}

pub fn points_inside_blobs(
    symlink_parent: &Path,
    target_text: &Path,
    store_root: &StoreRoot,
) -> Result<bool> {
    Ok(normalized_symlink_target(symlink_parent, target_text)?.starts_with(store_root.blobs_dir()))
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

fn validate_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(path).wrap_err_with(|| format!("stat {label} {:?}", path))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "{label} must be a real directory, not a symlink: {:?}",
            path
        );
    }
    if !metadata.is_dir() {
        bail!("{label} must be an existing directory: {:?}", path);
    }
    Ok(())
}

fn looks_like_windows_drive(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

fn path_components_without_root(path: &Path) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => parts.push(os_str_to_utf8(value, "path component")?),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                bail!("absolute path contains unsupported component: {:?}", path);
            }
        }
    }
    Ok(parts)
}

fn lexical_normalize(path: &Path) -> Result<PathBuf> {
    let absolute = path.is_absolute();
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::Normal(value) => parts.push(os_str_to_utf8(value, "path component")?),
            Component::ParentDir => {
                if parts.last().is_some_and(|last| last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..".to_owned());
                }
            }
            Component::Prefix(_) => bail!("unsupported path prefix in {:?}", path),
        }
    }
    let mut normalized = if absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };
    for part in parts {
        normalized.push(part);
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(value: char) -> BlobHash {
        BlobHash::new(value.to_string().repeat(64)).expect("valid hash")
    }

    #[test]
    fn suffix_filename_preserves_only_final_extension() {
        assert_eq!(
            suffix_filename_with_hash("photo.jpg", &hash('a'), 6),
            "photo_aaaaaa.jpg"
        );
        assert_eq!(
            suffix_filename_with_hash("archive.tar.gz", &hash('b'), 4),
            "archive.tar_bbbb.gz"
        );
        assert_eq!(
            suffix_filename_with_hash("README", &hash('c'), 3),
            "README_ccc"
        );
        assert_eq!(suffix_filename_with_hash(".env", &hash('d'), 2), ".env_dd");
        assert_eq!(
            suffix_filename_with_hash("東京.mov", &hash('e'), 5),
            "東京_eeeee.mov"
        );
    }

    #[test]
    fn relative_symlink_targets_cover_common_shapes() {
        assert_eq!(
            relative_symlink_target(Path::new("/a/b"), Path::new("/a/b/blob")).expect("target"),
            PathBuf::from("blob")
        );
        assert_eq!(
            relative_symlink_target(Path::new("/a/b/c"), Path::new("/a/b/blob")).expect("target"),
            PathBuf::from("../blob")
        );
        assert_eq!(
            relative_symlink_target(Path::new("/a/b"), Path::new("/a/b/c/blob")).expect("target"),
            PathBuf::from("c/blob")
        );
        assert_eq!(
            relative_symlink_target(Path::new("/a/b/c"), Path::new("/a/b/d/blob")).expect("target"),
            PathBuf::from("../d/blob")
        );
    }

    #[test]
    fn catalog_relative_path_rejects_non_portable_values() {
        assert!(SourceRelativePath::from_catalog_text("Movies/a.mov").is_ok());
        assert!(SourceRelativePath::from_catalog_text("").is_err());
        assert!(SourceRelativePath::from_catalog_text("/abs").is_err());
        assert!(SourceRelativePath::from_catalog_text("a//b").is_err());
        assert!(SourceRelativePath::from_catalog_text("a/../b").is_err());
        assert!(SourceRelativePath::from_catalog_text("a\\b").is_err());
        assert!(SourceRelativePath::from_catalog_text("C:/a").is_err());
    }
}
