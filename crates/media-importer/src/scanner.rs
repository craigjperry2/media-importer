use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use tracing::debug;
use walkdir::{IntoIter, WalkDir};

use crate::paths::{SourceRelativePath, SourceRoot};

/// Opaque identity of the filesystem that supplied a candidate during one
/// import. It deliberately never reaches CLI output or durable catalog state.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MountId(u64);

impl MountId {
    #[cfg(test)]
    pub(crate) const fn for_test(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Debug for MountId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `st_dev` is a platform implementation detail. Keep it available for
        // scheduler equality, but never accidentally expose it through a
        // diagnostic or observer event rendering.
        formatter.write_str("MountId(<redacted>)")
    }
}

#[derive(Clone, Debug)]
pub struct SourceFileCandidate {
    pub absolute_path: PathBuf,
    pub relative_path: SourceRelativePath,
    pub size_bytes: u64,
    pub modified_at_ms: Option<i64>,
    pub mount_id: MountId,
}

/// Streaming source traversal. It keeps only WalkDir's traversal state and the
/// current candidate, never the entire source tree or a sorted path list.
pub struct SourceScanner {
    source_root: SourceRoot,
    entries: IntoIter,
}

pub fn scan_source(source_root: &SourceRoot) -> SourceScanner {
    SourceScanner {
        source_root: source_root.clone(),
        entries: WalkDir::new(source_root.path())
            .follow_links(false)
            .into_iter(),
    }
}

impl Iterator for SourceScanner {
    type Item = Result<SourceFileCandidate>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = match self.entries.next()? {
                Ok(entry) => entry,
                Err(error) => {
                    return Some(
                        Err(error)
                            .wrap_err_with(|| format!("walk source {:?}", self.source_root.path())),
                    );
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let absolute_path = entry.path().to_path_buf();
            let result = (|| {
                let metadata = fs::metadata(&absolute_path)
                    .wrap_err_with(|| format!("read metadata for source file {absolute_path:?}"))?;
                if !metadata.is_file() {
                    return Ok(None);
                }
                let relative = absolute_path
                    .strip_prefix(self.source_root.path())
                    .wrap_err_with(|| {
                        format!("compute relative source path for {absolute_path:?}")
                    })?;
                Ok(Some(SourceFileCandidate {
                    absolute_path: absolute_path.clone(),
                    relative_path: SourceRelativePath::from_path(relative)?,
                    size_bytes: metadata.len(),
                    modified_at_ms: modified_at_ms(&metadata, &absolute_path),
                    mount_id: mount_id(&metadata),
                }))
            })();
            match result {
                Ok(Some(candidate)) => return Some(Ok(candidate)),
                Ok(None) => continue,
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

fn mount_id(metadata: &fs::Metadata) -> MountId {
    #[cfg(unix)]
    {
        MountId(metadata.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        MountId(0)
    }
}

fn modified_at_ms(metadata: &fs::Metadata, path: &Path) -> Option<i64> {
    match metadata.modified() {
        Ok(modified) => match modified.duration_since(UNIX_EPOCH) {
            Ok(duration) => Some(duration.as_millis().try_into().unwrap_or(i64::MAX)),
            Err(error) => {
                debug!(?path, %error, "source mtime is before unix epoch");
                None
            }
        },
        Err(error) => {
            debug!(?path, %error, "source mtime is unavailable");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MountId;

    #[test]
    fn mount_id_debug_redacts_the_platform_device_value() {
        let rendered = format!("{:?}", MountId::for_test(9_876_543_210));
        assert_eq!(rendered, "MountId(<redacted>)");
        assert!(!rendered.contains("9876543210"));
    }
}
