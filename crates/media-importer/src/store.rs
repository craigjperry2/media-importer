use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::num::NonZeroUsize;
use std::path::Path;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail, eyre};
use tracing::debug;

use crate::hashing::{hash_file, hash_reader_to_writer};
use crate::integrity::{IntegrityFinding, push_finding};
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

#[derive(Debug)]
pub struct CasInspection {
    pub valid_blobs: BTreeSet<BlobHash>,
    pub findings: Vec<IntegrityFinding>,
    pub complete: bool,
}

pub struct OpenedBlob {
    pub file: File,
    pub identity: BlobFileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobFileIdentity {
    pub len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
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

pub fn inspect_cas(root: &StoreRoot) -> Result<CasInspection> {
    let mut inspection = CasInspection {
        valid_blobs: BTreeSet::new(),
        findings: Vec::new(),
        complete: true,
    };
    let blobs = root.blobs_dir();
    if let Err(error) = walk_cas(&blobs, &blobs, &mut inspection, true) {
        inspection.complete = false;
        push_finding(
            &mut inspection.findings,
            IntegrityFinding::for_path(
                "CAS_IO_ERROR",
                Path::new("blobs"),
                format!("reason=enumeration-failed context={error}"),
            ),
        )?;
    }
    Ok(inspection)
}

pub fn open_blob_no_follow(root: &StoreRoot, hash: &BlobHash) -> std::io::Result<OpenedBlob> {
    let path = root.blob_path(hash);
    let file = safe_open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::other("opened blob is not a regular file"));
    }
    Ok(OpenedBlob {
        file,
        identity: BlobFileIdentity::from_metadata(&metadata),
    })
}

pub fn remove_revalidated_blob(
    root: &StoreRoot,
    hash: &BlobHash,
    expected: &BlobFileIdentity,
) -> Result<()> {
    let path = root.blob_path(hash);
    let metadata =
        fs::symlink_metadata(&path).wrap_err_with(|| format!("revalidate GC candidate {hash}"))?;
    if !metadata.is_file() {
        bail!("GC candidate {hash} is no longer a regular file");
    }
    let actual = BlobFileIdentity::from_metadata(&metadata);
    if &actual != expected {
        bail!("GC candidate {hash} changed after preflight");
    }
    fs::remove_file(&path).wrap_err_with(|| format!("remove GC candidate {hash}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("GC candidate {hash} has no containing directory"))?;
    sync_directory(parent)
        .wrap_err_with(|| format!("sync containing directory after removing GC candidate {hash}"))
}

pub fn sync_nearest_existing_blob_parent(root: &StoreRoot, hash: &BlobHash) -> Result<()> {
    let path = root.blob_path(hash);
    let mut current = path
        .parent()
        .ok_or_else(|| eyre!("GC candidate {hash} has no containing directory"))?;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) if metadata.is_dir() => {
                return sync_directory(current).wrap_err_with(|| {
                    format!("sync nearest existing directory for absent GC candidate {hash}")
                });
            }
            Ok(_) => bail!(
                "canonical parent for absent GC candidate {hash} is not a directory: {:?}",
                current
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                current = current.parent().ok_or_else(|| {
                    eyre!("no existing canonical parent for absent GC candidate {hash}")
                })?;
                if !current.starts_with(root.path()) {
                    bail!("absent GC candidate {hash} escaped the validated store root");
                }
            }
            Err(error) => {
                return Err(error).wrap_err_with(|| {
                    format!("stat canonical parent for absent GC candidate {hash}")
                });
            }
        }
    }
}

impl BlobFileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                len: metadata.len(),
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(not(unix))]
        Self {
            len: metadata.len(),
        }
    }
}

fn walk_cas(
    root: &Path,
    dir: &Path,
    inspection: &mut CasInspection,
    ancestors_valid: bool,
) -> Result<()> {
    let entries =
        fs::read_dir(dir).wrap_err_with(|| format!("enumerate blobs directory {dir:?}"))?;
    let mut sorted_entries = Vec::new();
    for entry in entries {
        sorted_entries
            .try_reserve(1)
            .map_err(|error| eyre!("reserve CAS directory entries: {error}"))?;
        sorted_entries.push(entry.wrap_err("enumerate CAS entry")?);
    }
    sorted_entries.sort_by(|left, right| compare_os_str(&left.file_name(), &right.file_name()));
    for entry in sorted_entries {
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                inspection.complete = false;
                push_finding(
                    &mut inspection.findings,
                    IntegrityFinding::for_path("CAS_IO_ERROR", relative, "reason=stat-failed"),
                )?;
                continue;
            }
        };
        let mut components = Vec::new();
        components
            .try_reserve(relative.components().count())
            .map_err(|error| eyre!("reserve CAS path components: {error}"))?;
        components.extend(relative.components().map(|component| component.as_os_str()));
        let kind = EntryKind::from_metadata(&metadata);
        match classify_cas_entry(&components, kind, ancestors_valid) {
            CasClassification::ValidDirectory => {
                if let Err(error) = walk_cas(root, &path, inspection, true) {
                    inspection.complete = false;
                    push_finding(
                        &mut inspection.findings,
                        IntegrityFinding::for_path(
                            "CAS_IO_ERROR",
                            relative,
                            format!("reason=enumeration-failed context={error}"),
                        ),
                    )?;
                }
            }
            CasClassification::Invalid(reason) => {
                push_finding(
                    &mut inspection.findings,
                    IntegrityFinding::for_path(
                        "INVALID_CAS_ENTRY",
                        relative,
                        format!("reason={reason}"),
                    ),
                )?;
                if metadata.is_dir()
                    && let Err(error) = walk_cas(root, &path, inspection, false)
                {
                    inspection.complete = false;
                    push_finding(
                        &mut inspection.findings,
                        IntegrityFinding::for_path(
                            "CAS_IO_ERROR",
                            relative,
                            format!("reason=enumeration-failed context={error}"),
                        ),
                    )?;
                }
            }
            CasClassification::ValidBlob(hash) => {
                inspection.valid_blobs.insert(hash);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        let kind = metadata.file_type();
        if kind.is_symlink() {
            Self::Symlink
        } else if kind.is_dir() {
            Self::Directory
        } else if kind.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum CasClassification {
    ValidDirectory,
    ValidBlob(BlobHash),
    Invalid(&'static str),
}

fn classify_cas_entry(
    components: &[&OsStr],
    kind: EntryKind,
    ancestors_valid: bool,
) -> CasClassification {
    if kind == EntryKind::Symlink {
        return CasClassification::Invalid("symlink");
    }
    if kind == EntryKind::Other {
        return CasClassification::Invalid("non-regular");
    }
    if !ancestors_valid {
        return CasClassification::Invalid("malformed-ancestor");
    }
    match kind {
        EntryKind::Directory
            if components.len() <= 2 && components.iter().all(|value| valid_shard(value)) =>
        {
            CasClassification::ValidDirectory
        }
        EntryKind::Directory if components.len() >= 3 => {
            CasClassification::Invalid("directory-at-blob-depth")
        }
        EntryKind::Directory => CasClassification::Invalid("invalid-shard"),
        EntryKind::File if components.len() != 3 => {
            CasClassification::Invalid("file-at-invalid-depth")
        }
        EntryKind::File => {
            if !valid_shard(components[0]) || !valid_shard(components[1]) {
                return CasClassification::Invalid("invalid-shard");
            }
            let Some(name) = components[2].to_str() else {
                return CasClassification::Invalid("invalid-blob-name");
            };
            let Ok(hash) = BlobHash::new(name.to_owned()) else {
                return CasClassification::Invalid("invalid-blob-name");
            };
            if components[0].to_str() != Some(&hash.as_str()[0..2])
                || components[1].to_str() != Some(&hash.as_str()[2..4])
            {
                CasClassification::Invalid("shard-mismatch")
            } else {
                CasClassification::ValidBlob(hash)
            }
        }
        EntryKind::Symlink | EntryKind::Other => unreachable!("handled above"),
    }
}

fn valid_shard(value: &OsStr) -> bool {
    value.to_str().is_some_and(|text| {
        text.len() == 2
            && text
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

#[cfg(unix)]
fn compare_os_str(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::unix::ffi::OsStrExt;
    left.as_bytes().cmp(right.as_bytes())
}

#[cfg(not(unix))]
fn compare_os_str(left: &OsStr, right: &OsStr) -> Ordering {
    left.to_string_lossy()
        .as_bytes()
        .cmp(right.to_string_lossy().as_bytes())
}

#[cfg(unix)]
fn safe_open(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn safe_open(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
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

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn pure_classifier_covers_layout_depth_kind_and_shards() {
        let valid = [OsStr::new("01"), OsStr::new("23"), OsStr::new(HASH)];
        assert!(matches!(
            classify_cas_entry(&valid, EntryKind::File, true),
            CasClassification::ValidBlob(_)
        ));
        assert_eq!(
            classify_cas_entry(&valid[..1], EntryKind::Directory, true),
            CasClassification::ValidDirectory
        );
        assert_eq!(
            classify_cas_entry(&valid[..1], EntryKind::File, true),
            CasClassification::Invalid("file-at-invalid-depth")
        );
        assert_eq!(
            classify_cas_entry(&valid, EntryKind::Directory, true),
            CasClassification::Invalid("directory-at-blob-depth")
        );
        assert_eq!(
            classify_cas_entry(&valid, EntryKind::Symlink, true),
            CasClassification::Invalid("symlink")
        );
        assert_eq!(
            classify_cas_entry(&[OsStr::new("AA")], EntryKind::Directory, true),
            CasClassification::Invalid("invalid-shard")
        );
        assert_eq!(
            classify_cas_entry(&valid, EntryKind::File, false),
            CasClassification::Invalid("malformed-ancestor")
        );
        let mismatch = [OsStr::new("ff"), OsStr::new("23"), OsStr::new(HASH)];
        assert_eq!(
            classify_cas_entry(&mismatch, EntryKind::File, true),
            CasClassification::Invalid("shard-mismatch")
        );
    }
}
