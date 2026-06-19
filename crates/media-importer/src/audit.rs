use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};

use crate::catalog::inspect_catalog_for_audit;
use crate::config::AuditConfig;
use crate::hashing::hash_open_file;
use crate::paths::BlobHash;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AuditFinding {
    pub category: &'static str,
    sort_identity: Vec<u8>,
    pub identity: String,
    pub details: String,
}

impl AuditFinding {
    pub(crate) fn new(
        category: &'static str,
        identity: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        let identity = identity.into();
        Self {
            category,
            sort_identity: identity.as_bytes().to_vec(),
            identity,
            details: details.into(),
        }
    }

    fn for_path(category: &'static str, path: &Path, details: impl Into<String>) -> Self {
        Self {
            category,
            sort_identity: path_bytes(path),
            identity: escape_path(path),
            details: details.into(),
        }
    }
}

pub(crate) fn push_finding(findings: &mut Vec<AuditFinding>, finding: AuditFinding) -> Result<()> {
    findings
        .try_reserve(1)
        .map_err(|error| eyre!("reserve audit finding: {error}"))?;
    findings.push(finding);
    Ok(())
}

#[derive(Debug)]
pub struct AuditReport {
    pub catalog_blobs: u64,
    pub cas_blob_files: u64,
    pub blobs_hashed: u64,
    pub gc_candidates: u64,
    pub findings: Vec<AuditFinding>,
}

impl AuditReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

pub fn audit_store(config: AuditConfig) -> Result<AuditReport> {
    let snapshot = inspect_catalog_for_audit(&config.db_path)?;
    let mut findings = snapshot.findings;
    let mut cas = HashMap::new();
    walk(
        &config.store_root.blobs_dir(),
        &config.store_root.blobs_dir(),
        &mut cas,
        &mut findings,
        true,
    )?;
    let cas_blob_files = u64::try_from(cas.len()).wrap_err("CAS blob count overflow")?;
    let mut keys = Vec::new();
    keys.try_reserve(snapshot.valid_blobs.len().saturating_add(cas.len()))
        .map_err(|error| eyre!("reserve reconciliation keys: {error}"))?;
    keys.extend(snapshot.valid_blobs.keys().cloned());
    keys.extend(cas.keys().cloned());
    keys.sort();
    keys.dedup();
    let mut blobs_hashed = 0_u64;
    for hash in keys {
        let catalog = snapshot.valid_blobs.get(&hash);
        let path = config.store_root.blob_path(&hash);
        let display = relative(config.store_root.path(), &path);
        let Some(discovered) = cas.get(&hash) else {
            match fs::symlink_metadata(&path) {
                Ok(metadata) if !metadata.is_file() => push_finding(
                    &mut findings,
                    AuditFinding::new(
                        "NON_REGULAR_BLOB",
                        hash.to_string(),
                        format!("path={display}"),
                    ),
                )?,
                Ok(_) => push_finding(
                    &mut findings,
                    AuditFinding::new("MISSING_BLOB", hash.to_string(), format!("path={display}")),
                )?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => push_finding(
                    &mut findings,
                    AuditFinding::new("MISSING_BLOB", hash.to_string(), format!("path={display}")),
                )?,
                Err(_) => push_finding(
                    &mut findings,
                    AuditFinding::new(
                        "BLOB_IO_ERROR",
                        hash.to_string(),
                        format!("path={display} reason=stat-failed"),
                    ),
                )?,
            }
            continue;
        };
        if catalog.is_none() {
            push_finding(
                &mut findings,
                AuditFinding::new("ORPHAN_BLOB", hash.to_string(), format!("path={display}")),
            )?;
        }
        match safe_open(discovered) {
            Ok(mut file) => {
                let metadata = match file.metadata() {
                    Ok(v) if v.is_file() => v,
                    _ => {
                        push_finding(
                            &mut findings,
                            AuditFinding::new(
                                "BLOB_IO_ERROR",
                                hash.to_string(),
                                format!("path={display} reason=not-regular-after-open"),
                            ),
                        )?;
                        continue;
                    }
                };
                match hash_open_file(&mut file, config.chunk_size) {
                    Ok(actual) => {
                        blobs_hashed = blobs_hashed
                            .checked_add(1)
                            .ok_or_else(|| eyre!("hashed blob counter overflow"))?;
                        let expected_size = catalog
                            .map(|blob| blob.size_bytes)
                            .unwrap_or(metadata.len());
                        if expected_size != actual.size_bytes || metadata.len() != actual.size_bytes
                        {
                            push_finding(
                                &mut findings,
                                AuditFinding::new(
                                    "SIZE_MISMATCH",
                                    hash.to_string(),
                                    format!(
                                        "expected={} metadata={} actual={}",
                                        expected_size,
                                        metadata.len(),
                                        actual.size_bytes
                                    ),
                                ),
                            )?;
                        }
                        if actual.hash != hash {
                            push_finding(
                                &mut findings,
                                AuditFinding::new(
                                    "HASH_MISMATCH",
                                    hash.to_string(),
                                    format!("expected={hash} actual={}", actual.hash),
                                ),
                            )?;
                        }
                    }
                    Err(_) => push_finding(
                        &mut findings,
                        AuditFinding::new(
                            "BLOB_IO_ERROR",
                            hash.to_string(),
                            format!("path={display} reason=read-failed"),
                        ),
                    )?,
                }
            }
            Err(_) => push_finding(
                &mut findings,
                AuditFinding::new(
                    "BLOB_IO_ERROR",
                    hash.to_string(),
                    format!("path={display} reason=open-failed"),
                ),
            )?,
        }
    }
    findings.sort();
    findings.dedup();
    Ok(AuditReport {
        catalog_blobs: snapshot.blob_rows_seen,
        cas_blob_files,
        blobs_hashed,
        gc_candidates: snapshot.gc_candidates,
        findings,
    })
}

fn walk(
    root: &Path,
    dir: &Path,
    cas: &mut HashMap<BlobHash, PathBuf>,
    findings: &mut Vec<AuditFinding>,
    ancestors_valid: bool,
) -> Result<()> {
    let entries =
        fs::read_dir(dir).wrap_err_with(|| format!("enumerate blobs directory {:?}", dir))?;
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
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(v) => v,
            Err(_) => {
                push_finding(
                    findings,
                    AuditFinding::for_path("CAS_IO_ERROR", rel, "reason=stat-failed"),
                )?;
                continue;
            }
        };
        let mut components = Vec::new();
        components
            .try_reserve(rel.components().count())
            .map_err(|error| eyre!("reserve CAS path components: {error}"))?;
        components.extend(rel.components().map(|component| component.as_os_str()));
        let kind = EntryKind::from_metadata(&metadata);
        match classify_cas_entry(&components, kind, ancestors_valid) {
            CasClassification::ValidDirectory => {
                if let Err(error) = walk(root, &path, cas, findings, true) {
                    push_finding(
                        findings,
                        AuditFinding::for_path(
                            "CAS_IO_ERROR",
                            rel,
                            format!("reason=enumeration-failed context={error}"),
                        ),
                    )?;
                }
            }
            CasClassification::Invalid(reason) => {
                push_finding(
                    findings,
                    AuditFinding::for_path("INVALID_CAS_ENTRY", rel, format!("reason={reason}")),
                )?;
                if metadata.is_dir()
                    && let Err(error) = walk(root, &path, cas, findings, false)
                {
                    push_finding(
                        findings,
                        AuditFinding::for_path(
                            "CAS_IO_ERROR",
                            rel,
                            format!("reason=enumeration-failed context={error}"),
                        ),
                    )?;
                }
            }
            CasClassification::ValidBlob(hash) => {
                cas.try_reserve(1)
                    .map_err(|error| eyre!("reserve CAS blob index: {error}"))?;
                cas.insert(hash, path);
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
    value.to_str().is_some_and(|s| {
        s.len() == 2
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}
fn relative(root: &Path, path: &Path) -> String {
    escape_path(path.strip_prefix(root).unwrap_or(path))
}
fn escape_path(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        escape_bytes(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    escape_bytes(path.to_string_lossy().as_bytes())
}

fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    path.to_string_lossy().into_owned().into_bytes()
}

fn escape_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut output = String::new();
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                push_valid_text(&mut output, valid);
                break;
            }
            Err(error) => {
                let valid = &remaining[..error.valid_up_to()];
                push_valid_text(
                    &mut output,
                    std::str::from_utf8(valid).expect("UTF-8 valid prefix"),
                );
                let invalid_len = error
                    .error_len()
                    .unwrap_or(remaining.len() - error.valid_up_to())
                    .max(1);
                for byte in &remaining[error.valid_up_to()..error.valid_up_to() + invalid_len] {
                    write!(output, "\\x{byte:02x}").expect("writing to String cannot fail");
                }
                remaining = &remaining[error.valid_up_to() + invalid_len..];
            }
        }
    }
    output
}

fn push_valid_text(output: &mut String, text: &str) {
    use std::fmt::Write;
    for character in text.chars() {
        if character.is_control() || character.is_whitespace() {
            let mut encoded = [0; 4];
            for byte in character.encode_utf8(&mut encoded).as_bytes() {
                write!(output, "\\x{byte:02x}").expect("writing to String cannot fail");
            }
        } else {
            output.push(character);
        }
    }
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
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x100)
        .open(path)
}
#[cfg(not(unix))]
fn safe_open(path: &Path) -> std::io::Result<File> {
    File::open(path)
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

    #[cfg(unix)]
    #[test]
    fn raw_path_identity_escapes_non_utf8_and_control_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(OsStr::from_bytes(b"bad\xff name\n"));
        assert_eq!(escape_path(path), "bad\\xff\\x20name\\x0a");
    }
}
