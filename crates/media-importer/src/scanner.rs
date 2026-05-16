use std::fs;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use tracing::debug;
use walkdir::WalkDir;

use crate::paths::{SourceRelativePath, SourceRoot};

#[derive(Clone, Debug)]
pub struct SourceFileCandidate {
    pub absolute_path: PathBuf,
    pub relative_path: SourceRelativePath,
    pub size_bytes: u64,
    pub modified_at_ms: Option<i64>,
}

pub fn scan_source(source_root: &SourceRoot) -> Result<Vec<SourceFileCandidate>> {
    let mut candidates = Vec::new();

    for entry in WalkDir::new(source_root.path()).follow_links(false) {
        let entry = entry.wrap_err_with(|| format!("walk source {:?}", source_root.path()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let absolute_path = entry.path().to_path_buf();
        let metadata = fs::metadata(&absolute_path)
            .wrap_err_with(|| format!("read metadata for source file {:?}", absolute_path))?;
        if !metadata.is_file() {
            continue;
        }

        let relative = absolute_path
            .strip_prefix(source_root.path())
            .wrap_err_with(|| format!("compute relative source path for {:?}", absolute_path))?;
        let relative_path = SourceRelativePath::from_path(relative)?;
        let modified_at_ms = modified_at_ms(&metadata, &absolute_path);

        candidates.push(SourceFileCandidate {
            absolute_path,
            relative_path,
            size_bytes: metadata.len(),
            modified_at_ms,
        });
    }

    candidates.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(candidates)
}

fn modified_at_ms(metadata: &fs::Metadata, path: &std::path::Path) -> Option<i64> {
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
