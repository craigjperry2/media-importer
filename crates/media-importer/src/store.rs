use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ffi::{CStr, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail, eyre};
use tracing::debug;

use crate::hashing::{hash_reader_to_writer_with_chunk_observer, hash_reader_with_chunk_observer};
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

/// Metadata-only inspection result used by idempotent import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CasMetadataCheck {
    Present,
    Missing,
    SizeMismatch,
    Invalid,
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    device: u64,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    inode: u64,
}

impl Store {
    pub fn new(root: StoreRoot) -> Self {
        Self { root }
    }

    pub fn prepare_staging_for_import(&self) -> Result<()> {
        let _ = open_validated_staging_directory(&self.root, true)?;
        self.purge_existing_staging()?;
        ensure_blob_parent(&self.root, None)
    }

    pub fn ingest_file(
        &self,
        source_path: &Path,
        expected_size: u64,
        chunk_size: NonZeroUsize,
    ) -> Result<StoredBlob> {
        self.ingest_file_with_read_observer(source_path, expected_size, chunk_size, |_| Ok(()))
    }

    /// Import one file while reporting the completed source-read boundary.
    ///
    /// The callback runs after each chunk reaches staging, before the next
    /// source read. This provides exact source/staging byte facts and a safe
    /// cooperative cancellation point without a second source read.
    pub(crate) fn ingest_file_with_read_observer(
        &self,
        source_path: &Path,
        expected_size: u64,
        chunk_size: NonZeroUsize,
        mut on_chunk_written: impl FnMut(u64) -> Result<()>,
    ) -> Result<StoredBlob> {
        let before = fs::symlink_metadata(source_path)
            .wrap_err_with(|| format!("stat source file before import {:?}", source_path))?;
        if !before.is_file() {
            bail!("source file is not a regular file: {source_path:?}");
        }
        let before_identity = BlobFileIdentity::from_metadata(&before);
        let before_mtime = before.modified().ok();
        let staging_name = StagingFileName::new();
        let staging_path = self.root.staging_path(&staging_name);

        let mut source = open_regular_no_follow(source_path)
            .wrap_err_with(|| format!("open source file for import {:?}", source_path))?;
        if BlobFileIdentity::from_metadata(
            &source
                .metadata()
                .wrap_err_with(|| format!("read opened source metadata {source_path:?}"))?,
        ) != before_identity
        {
            bail!("source file changed before its no-follow handle was opened: {source_path:?}");
        }
        let mut staging = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging_path)
            .wrap_err_with(|| format!("create staging file {:?}", staging_path))?;
        crate::test_probe::pause_or_fail("store-after-staging-created")?;
        // From this point on every early return, including metadata checks and
        // an existing-blob verification failure, removes the private staging
        // path.  Successful installs remove it explicitly before returning.
        let _staging_cleanup = StagingCleanup::new(&staging_path);

        let hash_result = match hash_reader_to_writer_with_chunk_observer(
            &mut source,
            &mut staging,
            chunk_size,
            |size_bytes| {
                // This seam is inside the one-pass source-to-staging loop: a
                // failure here leaves only a private staging artifact.
                crate::test_probe::pause_or_fail("store-during-source-copy")?;
                on_chunk_written(size_bytes)
            },
        ) {
            Ok(result) => result,
            Err(error) => {
                remove_staging_best_effort(&staging_path);
                return Err(error)
                    .wrap_err_with(|| format!("stream source into staging {:?}", source_path));
            }
        };
        if hash_result.size_bytes != expected_size {
            remove_staging_best_effort(&staging_path);
            bail!(
                "source file size changed while reading {:?}: expected {}, read {}",
                source_path,
                expected_size,
                hash_result.size_bytes
            );
        }

        let after = source.metadata().wrap_err_with(|| {
            format!("read source handle metadata after import {source_path:?}")
        })?;
        if !after.is_file() || BlobFileIdentity::from_metadata(&after) != before_identity {
            remove_staging_best_effort(&staging_path);
            bail!("source file changed during import {source_path:?}");
        }
        if let (Some(before), Ok(after)) = (before_mtime, after.modified())
            && before != after
        {
            remove_staging_best_effort(&staging_path);
            bail!("source file modified during import {:?}", source_path);
        }

        // Set the final blob mode while this is still our private staging
        // handle. `hard_link` preserves inode permissions, so the CAS name is
        // never observable with a writable mode, even if the process crashes
        // immediately after installation.
        set_readonly_blob(&staging)
            .wrap_err_with(|| format!("make staged blob read-only {:?}", staging_path))?;
        drop(staging);

        let final_path = self.root.blob_path(&hash_result.hash);
        if let Err(error) = ensure_blob_parent(&self.root, Some(&hash_result.hash)) {
            remove_staging_best_effort(&staging_path);
            return Err(error).wrap_err_with(|| {
                format!("create blob parent directory for {}", hash_result.hash)
            });
        }

        // Private integration-test seam: a deterministic barrier immediately
        // before the atomic no-replace install proves this is the dedup race
        // boundary. It is inert unless the private probe environment is set.
        crate::test_probe::pause_or_panic("store-before-cas-install")?;
        crate::test_probe::pause_or_fail("store-before-cas-install")?;
        match fs::hard_link(&staging_path, &final_path) {
            Ok(()) => {
                if let Err(error) = fs::remove_file(&staging_path) {
                    remove_staging_best_effort(&staging_path);
                    return Err(error).wrap_err_with(|| {
                        format!("remove installed staging file {:?}", staging_path)
                    });
                }
                Ok(StoredBlob {
                    hash: hash_result.hash,
                    size_bytes: hash_result.size_bytes,
                    outcome: StoreOutcome::Created,
                })
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                // Test-only seam for the final-path replacement regression.
                // Production behavior is unchanged unless the private probe
                // environment is configured.
                crate::test_probe::pause_or_fail("store-before-existing-cas-open")?;
                verify_and_make_existing_blob_readonly(&final_path, hash_result.size_bytes)?;
                if let Err(error) = fs::remove_file(&staging_path) {
                    remove_staging_best_effort(&staging_path);
                    return Err(error).wrap_err_with(|| {
                        format!("remove reused staging file {:?}", staging_path)
                    });
                }
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
        match verify_existing_blob(&path, size_bytes) {
            Ok(()) => Ok(BlobPresence::Present),
            Err(error) if is_not_found_error(&error) => Ok(BlobPresence::Missing),
            Err(error) => Err(error),
        }
    }

    /// Check a cataloged CAS entry without following the final path component
    /// and without reading blob content.
    pub fn check_blob_metadata(
        &self,
        hash: &BlobHash,
        expected_size: u64,
    ) -> Result<CasMetadataCheck> {
        let path = self.root.blob_path(hash);
        let file = match safe_open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(CasMetadataCheck::Missing);
            }
            Err(error) => {
                return Err(error).wrap_err_with(|| format!("open CAS blob metadata {path:?}"));
            }
        };
        let metadata = file
            .metadata()
            .wrap_err_with(|| format!("read CAS blob metadata {path:?}"))?;
        if !metadata.is_file() {
            return Ok(CasMetadataCheck::Invalid);
        }
        Ok(if metadata.len() == expected_size {
            CasMetadataCheck::Present
        } else {
            CasMetadataCheck::SizeMismatch
        })
    }

    pub fn hash_file_read_only(
        &self,
        source_path: &Path,
        chunk_size: NonZeroUsize,
    ) -> Result<StoredBlob> {
        let mut source = open_regular_no_follow(source_path)
            .wrap_err_with(|| format!("open source file for read-only hashing {source_path:?}"))?;
        let hash_result = hash_reader_with_chunk_observer(&mut source, chunk_size, |_| Ok(()))?;
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

    pub(crate) fn hash_file_read_only_with_chunk_observer(
        &self,
        source_path: &Path,
        chunk_size: NonZeroUsize,
        on_chunk_read: impl FnMut(u64) -> Result<()>,
    ) -> Result<StoredBlob> {
        let mut source = open_regular_no_follow(source_path)
            .wrap_err_with(|| format!("open source file for read-only hashing {source_path:?}"))?;
        let hash_result = hash_reader_with_chunk_observer(&mut source, chunk_size, on_chunk_read)?;
        let outcome = match self.check_blob_presence(&hash_result.hash, hash_result.size_bytes)? {
            BlobPresence::Missing => StoreOutcome::Created,
            BlobPresence::Present => StoreOutcome::Reused,
        };
        Ok(StoredBlob {
            hash: hash_result.hash,
            size_bytes: hash_result.size_bytes,
            outcome,
        })
    }

    /// Remove entries which already exist in the application-owned staging
    /// directory. Missing staging is intentionally an empty state.
    pub fn purge_existing_staging(&self) -> Result<()> {
        let Some(directory) = open_validated_staging_directory(&self.root, false)? else {
            return Ok(());
        };
        purge_directory_entries(&directory).wrap_err_with(|| {
            format!(
                "purge stale staging directory {:?}",
                self.root.staging_dir()
            )
        })
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

pub(crate) fn revalidate_blob_for_removal(
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
    Ok(())
}

pub(crate) fn remove_blob_file(root: &StoreRoot, hash: &BlobHash) -> Result<()> {
    let path = root.blob_path(hash);
    fs::remove_file(&path).wrap_err_with(|| format!("remove GC candidate {hash}"))?;
    Ok(())
}

pub(crate) fn sync_blob_parent(root: &StoreRoot, hash: &BlobHash) -> Result<()> {
    let path = root.blob_path(hash);
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                len: metadata.len(),
                device: metadata.dev(),
                inode: metadata.ino(),
            }
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn compare_os_str(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::unix::ffi::OsStrExt;
    left.as_bytes().cmp(right.as_bytes())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn safe_open(path: &Path) -> std::io::Result<File> {
    open_regular_no_follow(path)
}

/// Open a regular file without following its final path component. Parent
/// directories are separately validated at store-owned CAS boundaries.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_regular_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_no_follow(path: &Path, flags: libc::c_int) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(flags | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_validated_staging_directory(root: &StoreRoot, create: bool) -> Result<Option<File>> {
    match fs::symlink_metadata(root.staging_dir()) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "staging directory must not be a symlink: {:?}",
                root.staging_dir()
            );
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("staging path is not a directory: {:?}", root.staging_dir());
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound && create => {
            fs::create_dir(root.staging_dir())
                .wrap_err_with(|| format!("create staging directory {:?}", root.staging_dir()))?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .wrap_err_with(|| format!("stat staging directory {:?}", root.staging_dir()));
        }
    }
    let directory = open_no_follow(&root.staging_dir(), libc::O_DIRECTORY).wrap_err_with(|| {
        format!(
            "open staging directory without following links {:?}",
            root.staging_dir()
        )
    })?;
    if !directory.metadata()?.is_dir() {
        bail!("staging path is not a directory: {:?}", root.staging_dir());
    }
    Ok(Some(directory))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ensure_blob_parent(root: &StoreRoot, hash: Option<&BlobHash>) -> Result<()> {
    let mut current = root.path().to_path_buf();
    for component in std::iter::once("blobs").chain(
        hash.into_iter()
            .flat_map(|hash| [&hash.as_str()[0..2], &hash.as_str()[2..4]]),
    ) {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("CAS directory must not be a symlink: {current:?}")
            }
            Ok(metadata) if !metadata.is_dir() => bail!("CAS path is not a directory: {current:?}"),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if let Err(error) = fs::create_dir(&current)
                    && error.kind() != ErrorKind::AlreadyExists
                {
                    return Err(error)
                        .wrap_err_with(|| format!("create CAS directory {current:?}"));
                }
            }
            Err(error) => {
                return Err(error).wrap_err_with(|| format!("stat CAS directory {current:?}"));
            }
        }
        open_no_follow(&current, libc::O_DIRECTORY)
            .wrap_err_with(|| format!("open CAS directory without following links {current:?}"))?;
    }
    Ok(())
}

/// Purge through an already-open directory descriptor. Every lookup and
/// deletion is relative to that descriptor, so a concurrent replacement of
/// the `staging` pathname cannot redirect cleanup outside the validated root.
fn purge_directory_entries(directory: &File) -> Result<()> {
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error()).wrap_err("duplicate staging directory handle");
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(std::io::Error::last_os_error()).wrap_err("open staging directory stream");
    }
    let result = (|| {
        loop {
            clear_errno();
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(0) {
                    break;
                }
                return Err(error).wrap_err("enumerate staging directory handle");
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            purge_directory_entry(directory.as_raw_fd(), name)?;
        }
        Ok(())
    })();
    unsafe { libc::closedir(stream) };
    result
}

#[cfg(target_os = "linux")]
fn clear_errno() {
    unsafe { *libc::__errno_location() = 0 };
}

#[cfg(target_os = "macos")]
fn clear_errno() {
    unsafe { *libc::__error() = 0 };
}

fn purge_directory_entry(parent_fd: libc::c_int, name: &CStr) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let stat_result = unsafe {
        libc::fstatat(
            parent_fd,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if stat_result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == ErrorKind::NotFound {
            return Ok(());
        }
        return Err(error).wrap_err("stat stale staging entry without following links");
    }
    let stat = unsafe { stat.assume_init() };
    if (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR {
        let child_fd = unsafe {
            libc::openat(
                parent_fd,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if child_fd >= 0 {
            let child = unsafe { File::from_raw_fd(child_fd) };
            purge_directory_entries(&child)?;
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() != ErrorKind::NotFound {
                return Err(error).wrap_err("open stale staging directory without following links");
            }
        }
        unlinkat(parent_fd, name, libc::AT_REMOVEDIR)
    } else {
        unlinkat(parent_fd, name, 0)
    }
}

fn unlinkat(parent_fd: libc::c_int, name: &CStr, flags: libc::c_int) -> Result<()> {
    if unsafe { libc::unlinkat(parent_fd, name.as_ptr(), flags) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error).wrap_err("remove stale staging entry relative to validated directory")
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

fn verify_existing_blob(path: &Path, expected_size: u64) -> Result<()> {
    let file = open_regular_no_follow(path)
        .wrap_err_with(|| format!("open existing blob without following links {path:?}"))?;
    verify_opened_blob(&file, path, expected_size)
}

/// Verify and harden an already-installed blob through one no-follow file
/// descriptor. In particular, do not re-resolve `path` between verification
/// and chmod: an attacker replacing that name with a symlink must not cause
/// the target to be permission-mutated.
fn verify_and_make_existing_blob_readonly(path: &Path, expected_size: u64) -> Result<()> {
    let file = open_regular_no_follow(path)
        .wrap_err_with(|| format!("open existing blob without following links {path:?}"))?;
    verify_opened_blob(&file, path, expected_size)?;
    set_readonly_blob(&file).wrap_err_with(|| format!("make existing blob read-only {path:?}"))
}

fn verify_opened_blob(file: &File, path: &Path, expected_size: u64) -> Result<()> {
    let metadata = file
        .metadata()
        .wrap_err_with(|| format!("stat opened existing blob {path:?}"))?;
    if !metadata.is_file() {
        bail!("CAS blob is not a regular file: {path:?}");
    }
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

fn is_not_found_error(error: &color_eyre::Report) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|cause| cause.kind() == ErrorKind::NotFound)
}

fn remove_staging_best_effort(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != ErrorKind::NotFound
    {
        debug!(?path, %error, "failed to clean up staging file");
    }
}

/// Owns best-effort cleanup for a staging path after it has been created.
/// This deliberately remains armed after successful explicit removal: a
/// second removal is harmless, while an unexpected error path cannot leak a
/// temporary file into the next import run.
struct StagingCleanup<'a> {
    path: &'a Path,
}

impl<'a> StagingCleanup<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path }
    }
}

impl Drop for StagingCleanup<'_> {
    fn drop(&mut self) {
        remove_staging_best_effort(self.path);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_readonly_blob(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = fs::Permissions::from_mode(0o444);
    file.set_permissions(permissions)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;

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

    #[test]
    fn reused_blob_verification_failure_removes_staging_file() {
        let temp = TempDir::new().expect("temporary directory");
        let store_path = temp.path().join("store");
        fs::create_dir(&store_path).expect("store root");
        let root = StoreRoot::validate(&store_path).expect("validated store root");
        let store = Store::new(root.clone());
        store.prepare_staging_for_import().expect("prepared store");

        let source = temp.path().join("source.bin");
        fs::write(&source, b"expected source bytes").expect("source fixture");
        let hash = BlobHash::new(blake3::hash(b"expected source bytes").to_hex().to_string())
            .expect("source hash");
        let final_path = root.blob_path(&hash);
        fs::create_dir_all(final_path.parent().expect("blob parent")).expect("blob parent");
        fs::write(&final_path, b"wrong size").expect("invalid existing blob");

        let error = store
            .ingest_file(&source, 21, NonZeroUsize::new(4).expect("chunk size"))
            .expect_err("invalid existing blob must fail");
        assert!(format!("{error:#}").contains("unexpected size"));
        assert_eq!(
            fs::read_dir(root.staging_dir())
                .expect("read staging")
                .count(),
            0,
            "verification failure must not leave staging artifacts"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn purging_staging_removes_a_symlink_without_following_its_target() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temporary directory");
        let store_path = temp.path().join("store");
        fs::create_dir(&store_path).expect("store root");
        let root = StoreRoot::validate(&store_path).expect("validated store root");
        let store = Store::new(root.clone());
        fs::create_dir(root.staging_dir()).expect("staging directory");
        let outside = temp.path().join("outside");
        fs::write(&outside, b"must survive").expect("outside fixture");
        let link = root.staging_dir().join("escaping-link");
        symlink(&outside, &link).expect("staging symlink");

        store.purge_existing_staging().expect("purge staging");

        assert!(!link.exists(), "staging link itself is removed");
        assert_eq!(fs::read(&outside).expect("outside target"), b"must survive");
    }
}
