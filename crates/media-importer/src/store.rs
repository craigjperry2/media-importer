use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::num::NonZeroUsize;
use std::path::Path;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail};
use tracing::debug;

use crate::hashing::{hash_file, hash_reader_to_writer};
use crate::paths::{BlobHash, StagingFileName, StoreRoot};

pub struct Store {
    root: StoreRoot,
}

#[derive(Clone, Debug)]
pub struct StoredBlob {
    pub hash: BlobHash,
    pub size_bytes: u64,
    pub outcome: StoreOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreOutcome {
    Created,
    Reused,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlobPresence {
    Missing,
    Present,
}

impl Store {
    pub fn new(root: StoreRoot) -> Self {
        Self { root }
    }

    pub fn prepare_for_import(&self) -> Result<()> {
        fs::create_dir_all(self.root.blobs_dir())
            .wrap_err_with(|| format!("create blobs directory {:?}", self.root.blobs_dir()))?;
        fs::create_dir_all(self.root.staging_dir())
            .wrap_err_with(|| format!("create staging directory {:?}", self.root.staging_dir()))?;
        self.purge_staging()
    }

    pub fn ingest_file(
        &self,
        source_path: &Path,
        expected_size: u64,
        chunk_size: NonZeroUsize,
    ) -> Result<StoredBlob> {
        let before = fs::metadata(source_path)
            .wrap_err_with(|| format!("stat source file before import {:?}", source_path))?;
        let before_mtime = before.modified().ok();
        let staging_name = StagingFileName::new();
        let staging_path = self.root.staging_path(&staging_name);

        let mut source = File::open(source_path)
            .wrap_err_with(|| format!("open source file for import {:?}", source_path))?;
        let mut staging = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging_path)
            .wrap_err_with(|| format!("create staging file {:?}", staging_path))?;

        let hash_result = match hash_reader_to_writer(&mut source, &mut staging, chunk_size) {
            Ok(result) => result,
            Err(error) => {
                remove_staging_best_effort(&staging_path);
                return Err(error)
                    .wrap_err_with(|| format!("stream source into staging {:?}", source_path));
            }
        };
        drop(staging);

        if hash_result.size_bytes != expected_size {
            remove_staging_best_effort(&staging_path);
            bail!(
                "source file size changed while reading {:?}: expected {}, read {}",
                source_path,
                expected_size,
                hash_result.size_bytes
            );
        }

        let after = fs::metadata(source_path)
            .wrap_err_with(|| format!("stat source file after import {:?}", source_path))?;
        if after.len() != expected_size {
            remove_staging_best_effort(&staging_path);
            bail!(
                "source file size changed during import {:?}: expected {}, now {}",
                source_path,
                expected_size,
                after.len()
            );
        }
        if let (Some(before), Ok(after)) = (before_mtime, after.modified())
            && before != after
        {
            remove_staging_best_effort(&staging_path);
            bail!("source file modified during import {:?}", source_path);
        }

        let final_path = self.root.blob_path(&hash_result.hash);
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("create blob parent directory {:?}", parent))?;
        }

        match fs::hard_link(&staging_path, &final_path) {
            Ok(()) => {
                fs::remove_file(&staging_path).wrap_err_with(|| {
                    format!("remove installed staging file {:?}", staging_path)
                })?;
                set_readonly_blob(&final_path)
                    .wrap_err_with(|| format!("make blob read-only {:?}", final_path))?;
                Ok(StoredBlob {
                    hash: hash_result.hash,
                    size_bytes: hash_result.size_bytes,
                    outcome: StoreOutcome::Created,
                })
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                verify_existing_blob(&final_path, hash_result.size_bytes)?;
                set_readonly_blob(&final_path)
                    .wrap_err_with(|| format!("make existing blob read-only {:?}", final_path))?;
                fs::remove_file(&staging_path)
                    .wrap_err_with(|| format!("remove reused staging file {:?}", staging_path))?;
                Ok(StoredBlob {
                    hash: hash_result.hash,
                    size_bytes: hash_result.size_bytes,
                    outcome: StoreOutcome::Reused,
                })
            }
            Err(error) => {
                remove_staging_best_effort(&staging_path);
                Err(error).wrap_err_with(|| {
                    format!(
                        "install staging file {:?} to {:?}",
                        staging_path, final_path
                    )
                })
            }
        }
    }

    pub fn check_blob_presence(&self, hash: &BlobHash, size_bytes: u64) -> Result<BlobPresence> {
        let path = self.root.blob_path(hash);
        if !path.exists() {
            return Ok(BlobPresence::Missing);
        }
        verify_existing_blob(&path, size_bytes)?;
        Ok(BlobPresence::Present)
    }

    pub fn hash_file_read_only(
        &self,
        source_path: &Path,
        chunk_size: NonZeroUsize,
    ) -> Result<StoredBlob> {
        let hash_result = hash_file(source_path, chunk_size)?;
        let presence = self.check_blob_presence(&hash_result.hash, hash_result.size_bytes)?;
        let outcome = match presence {
            BlobPresence::Missing => StoreOutcome::Created,
            BlobPresence::Present => StoreOutcome::Reused,
        };
        Ok(StoredBlob {
            hash: hash_result.hash,
            size_bytes: hash_result.size_bytes,
            outcome,
        })
    }

    fn purge_staging(&self) -> Result<()> {
        for entry in fs::read_dir(self.root.staging_dir())
            .wrap_err_with(|| format!("read staging directory {:?}", self.root.staging_dir()))?
        {
            let entry = entry
                .wrap_err_with(|| format!("read staging entry {:?}", self.root.staging_dir()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .wrap_err_with(|| format!("read staging entry type {:?}", path))?;
            if file_type.is_dir() {
                fs::remove_dir_all(&path)
                    .wrap_err_with(|| format!("remove stale staging directory {:?}", path))?;
            } else {
                fs::remove_file(&path)
                    .wrap_err_with(|| format!("remove stale staging file {:?}", path))?;
            }
        }
        Ok(())
    }
}

fn verify_existing_blob(path: &Path, expected_size: u64) -> Result<()> {
    let metadata = fs::metadata(path).wrap_err_with(|| format!("stat existing blob {:?}", path))?;
    if metadata.len() != expected_size {
        bail!(
            "CAS blob exists with unexpected size: {:?}, expected {}, found {}",
            path,
            expected_size,
            metadata.len()
        );
    }
    Ok(())
}

fn remove_staging_best_effort(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != ErrorKind::NotFound
    {
        debug!(?path, %error, "failed to clean up staging file");
    }
}

#[cfg(unix)]
fn set_readonly_blob(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = fs::Permissions::from_mode(0o444);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_readonly_blob(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)?;
    Ok(())
}
