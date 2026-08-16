use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail};
use walkdir::WalkDir;

use crate::catalog::ReadOnlyCatalog;
use crate::config::BuildTreeConfig;
use crate::paths::{BlobHash, output_relative_path, points_inside_blobs, relative_symlink_target};
use crate::run_lock::{LockMode, StoreRunLock};
use crate::telemetry::{NoopTelemetrySink, TelemetryEvent, TelemetrySink};
use crate::test_probe;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct BuildTreeReport {
    pub dry_run: bool,
    pub desired_links: u64,
    pub links_created: u64,
    pub links_unchanged: u64,
    pub links_replaced: u64,
    pub stale_links_removed: u64,
    pub directories_created: u64,
    pub directories_pruned: u64,
}

#[derive(Clone, Debug)]
struct DesiredLink {
    output_path: PathBuf,
    target_text: PathBuf,
    blob_hash: BlobHash,
}

#[derive(Clone, Debug)]
struct Plan {
    desired: Vec<DesiredLink>,
    stale_owned: Vec<PathBuf>,
    directories_to_create: Vec<PathBuf>,
    directories_to_prune: Vec<PathBuf>,
    report: BuildTreeReport,
}

pub fn build_tree(config: BuildTreeConfig) -> Result<BuildTreeReport> {
    build_tree_with_telemetry(config, Arc::new(NoopTelemetrySink))
}

/// Materialize through the command-neutral reporting boundary.  The plan and
/// apply milestones are emitted at the points they become true; no renderer
/// concerns leak into filesystem planning or mutation helpers.
pub fn build_tree_with_telemetry(
    config: BuildTreeConfig,
    telemetry: Arc<dyn TelemetrySink>,
) -> Result<BuildTreeReport> {
    let mode = if config.dry_run {
        LockMode::Shared
    } else {
        LockMode::Exclusive
    };
    let _lock = StoreRunLock::acquire(&config.store_root, "build-tree", mode)?;
    let plan = plan(&config, telemetry.as_ref())?;
    telemetry.emit(
        TelemetryEvent::new("build_tree", "tree_planned")
            .field("dry_run", config.dry_run)
            .field("entries", plan.report.desired_links)
            .field("directories", plan.report.directories_created),
    );
    if telemetry.failed() {
        bail!("telemetry renderer failed");
    }
    test_probe::pause("build-tree-planned")?;
    if config.dry_run {
        let report = plan.report;
        test_probe::pause("build-tree-report-constructed")?;
        return Ok(report);
    }
    if telemetry.failed() {
        bail!("telemetry renderer failed");
    }
    apply(&config, &plan, telemetry.as_ref())?;
    let report = plan.report;
    telemetry.emit(
        TelemetryEvent::new("build_tree", "tree_applied")
            .field("links_created", report.links_created)
            .field("links_replaced", report.links_replaced)
            .field("stale_links_removed", report.stale_links_removed)
            .field("directories_created", report.directories_created)
            .field("directories_pruned", report.directories_pruned),
    );
    test_probe::pause("build-tree-applied-report-constructed")?;
    Ok(report)
}

fn plan(config: &BuildTreeConfig, telemetry: &dyn TelemetrySink) -> Result<Plan> {
    let catalog = ReadOnlyCatalog::open_for_materialization(&config.db_path)?;
    let entries = catalog.live_materialization_entries()?;
    let mut desired_by_output: BTreeMap<PathBuf, DesiredLink> = BTreeMap::new();
    let hash_digits = config.hash_digits.get();

    for entry in entries {
        if telemetry.failed() {
            bail!("telemetry renderer failed");
        }
        let relative_output =
            output_relative_path(&entry.relative_path, &entry.blob_hash, hash_digits);
        let output_path = config.browse_tree_root.path().join(relative_output);
        let target_path = config.store_root.blob_path(&entry.blob_hash);
        validate_cas_target(&target_path, entry.blob_size_bytes)?;
        let parent = output_path
            .parent()
            .expect("desired output under browse tree has a parent");
        let target_text = relative_symlink_target(parent, &target_path)?;
        if let Some(existing) = desired_by_output.get(&output_path) {
            bail!(
                "hash-suffix collision at {:?} for blobs {} and {}; rerun with more hash digits, for example --hash-digits 12",
                output_path,
                existing.blob_hash,
                entry.blob_hash
            );
        }
        desired_by_output.insert(
            output_path.clone(),
            DesiredLink {
                output_path: output_path.clone(),
                target_text,
                blob_hash: entry.blob_hash.clone(),
            },
        );
        // Emit when a catalog entry has actually been reconciled into the
        // desired tree, rather than after the full catalog scan. This keeps
        // the live dashboard truthful for large trees without an extra scan.
        telemetry.emit(
            TelemetryEvent::new("build_tree", "tree_entry_planned")
                .field("path", crate::integrity::escape_path(&output_path))
                .field("hash", entry.blob_hash.to_string())
                .field("dry_run", config.dry_run),
        );
        if telemetry.failed() {
            bail!("telemetry renderer failed");
        }
    }

    let desired_paths: BTreeSet<PathBuf> = desired_by_output.keys().cloned().collect();
    let owned_symlinks = collect_owned_symlinks(config)?;
    let mut report = BuildTreeReport {
        dry_run: config.dry_run,
        desired_links: desired_by_output.len() as u64,
        ..BuildTreeReport::default()
    };

    for desired in desired_by_output.values() {
        if telemetry.failed() {
            bail!("telemetry renderer failed");
        }
        match fs::symlink_metadata(&desired.output_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let parent = desired
                    .output_path
                    .parent()
                    .expect("desired output has parent");
                let target = fs::read_link(&desired.output_path)
                    .wrap_err_with(|| format!("read symlink {:?}", desired.output_path))?;
                if !points_inside_blobs(parent, &target, &config.store_root)? {
                    bail!(
                        "desired output path is a user-managed symlink blocker: {:?}",
                        desired.output_path
                    );
                }
                if target == desired.target_text {
                    report.links_unchanged += 1;
                } else {
                    report.links_replaced += 1;
                }
            }
            Ok(_) => bail!(
                "desired output path is blocked by a non-symlink entry: {:?}",
                desired.output_path
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => report.links_created += 1,
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("stat desired output {:?}", desired.output_path));
            }
        }
    }

    let stale_owned: Vec<PathBuf> = owned_symlinks
        .into_iter()
        .filter(|path| !desired_paths.contains(path))
        .collect();
    report.stale_links_removed = stale_owned.len() as u64;

    let directories_to_create = directories_to_create(config, desired_by_output.values())?;
    report.directories_created = directories_to_create.len() as u64;
    let directories_to_prune = directories_to_prune(config, &stale_owned, &desired_paths)?;
    report.directories_pruned = directories_to_prune.len() as u64;

    Ok(Plan {
        desired: desired_by_output.into_values().collect(),
        stale_owned,
        directories_to_create,
        directories_to_prune,
        report,
    })
}

fn validate_cas_target(path: &Path, expected_size: u64) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).wrap_err_with(|| format!("stat CAS blob {:?}", path))?;
    if !metadata.is_file() {
        bail!("CAS blob is not a regular file: {:?}", path);
    }
    if metadata.len() != expected_size {
        bail!(
            "CAS blob size mismatch for {:?}: expected {}, found {}",
            path,
            expected_size,
            metadata.len()
        );
    }
    Ok(())
}

fn collect_owned_symlinks(config: &BuildTreeConfig) -> Result<Vec<PathBuf>> {
    if !config.browse_tree_root.path().exists() {
        return Ok(Vec::new());
    }
    let mut owned = Vec::new();
    for entry in WalkDir::new(config.browse_tree_root.path()).follow_links(false) {
        let entry = entry.wrap_err("walk browse tree")?;
        let path = entry.path();
        if path == config.browse_tree_root.path() {
            continue;
        }
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            let target =
                fs::read_link(path).wrap_err_with(|| format!("read symlink {:?}", path))?;
            let parent = path.parent().expect("walked path has parent");
            if points_inside_blobs(parent, &target, &config.store_root)? {
                owned.push(path.to_path_buf());
            }
        }
    }
    Ok(owned)
}

fn directories_to_create<'a>(
    config: &BuildTreeConfig,
    desired: impl Iterator<Item = &'a DesiredLink>,
) -> Result<Vec<PathBuf>> {
    let mut result = BTreeSet::new();
    if !config.browse_tree_root.path().exists() {
        result.insert(config.browse_tree_root.path().to_path_buf());
    }
    for link in desired {
        let mut ancestors = Vec::new();
        let mut cursor = link
            .output_path
            .parent()
            .expect("desired output path has parent");
        while cursor != config.browse_tree_root.path() {
            ancestors.push(cursor.to_path_buf());
            cursor = cursor
                .parent()
                .expect("desired output remains under browse tree");
        }
        for dir in ancestors.into_iter().rev() {
            match fs::symlink_metadata(&dir) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("desired parent directory is a symlink blocker: {:?}", dir);
                }
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => bail!("desired parent path is not a directory: {:?}", dir),
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    result.insert(dir);
                }
                Err(error) => return Err(error).wrap_err_with(|| format!("stat {:?}", dir)),
            }
        }
    }
    Ok(result.into_iter().collect())
}

fn directories_to_prune(
    config: &BuildTreeConfig,
    stale_owned: &[PathBuf],
    desired_paths: &BTreeSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut candidates = BTreeSet::new();
    for path in stale_owned {
        let mut cursor = path.parent();
        while let Some(dir) = cursor {
            if dir == config.browse_tree_root.path() {
                break;
            }
            candidates.insert(dir.to_path_buf());
            cursor = dir.parent();
        }
    }

    let stale: BTreeSet<&Path> = stale_owned.iter().map(PathBuf::as_path).collect();
    let mut prunable = Vec::new();
    let mut prunable_set = BTreeSet::new();
    for dir in candidates.iter().rev() {
        if would_be_empty_after_cleanup(dir, &stale, desired_paths, &prunable_set)? {
            prunable.push(dir.clone());
            prunable_set.insert(dir.clone());
        }
    }
    Ok(prunable)
}

fn would_be_empty_after_cleanup(
    dir: &Path,
    stale: &BTreeSet<&Path>,
    desired_paths: &BTreeSet<PathBuf>,
    prunable_dirs: &BTreeSet<PathBuf>,
) -> Result<bool> {
    for entry in fs::read_dir(dir).wrap_err_with(|| format!("read directory {:?}", dir))? {
        let entry = entry.wrap_err_with(|| format!("read directory entry {:?}", dir))?;
        let path = entry.path();
        if stale.contains(path.as_path()) || prunable_dirs.contains(&path) {
            continue;
        }
        if desired_paths.contains(&path) {
            return Ok(false);
        }
        return Ok(false);
    }
    Ok(true)
}

fn apply(config: &BuildTreeConfig, plan: &Plan, telemetry: &dyn TelemetrySink) -> Result<()> {
    for dir in &plan.directories_to_create {
        if telemetry.failed() {
            bail!("telemetry renderer failed");
        }
        fs::create_dir(dir).wrap_err_with(|| format!("create directory {:?}", dir))?;
        telemetry.emit(
            TelemetryEvent::new("build_tree", "tree_directory_applied")
                .field("path", crate::integrity::escape_path(dir))
                .field("action", "created"),
        );
    }
    for link in &plan.desired {
        if telemetry.failed() {
            bail!("telemetry renderer failed");
        }
        apply_desired_link(link)?;
        telemetry.emit(
            TelemetryEvent::new("build_tree", "tree_link_applied")
                .field("path", crate::integrity::escape_path(&link.output_path))
                .field("hash", link.blob_hash.to_string()),
        );
    }
    for path in &plan.stale_owned {
        if telemetry.failed() {
            bail!("telemetry renderer failed");
        }
        fs::remove_file(path).wrap_err_with(|| format!("remove stale symlink {:?}", path))?;
    }
    for dir in &plan.directories_to_prune {
        if telemetry.failed() {
            bail!("telemetry renderer failed");
        }
        match fs::remove_dir(dir) {
            Ok(()) => telemetry.emit(
                TelemetryEvent::new("build_tree", "tree_directory_applied")
                    .field("path", crate::integrity::escape_path(dir))
                    .field("action", "pruned"),
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) if error.kind() == ErrorKind::DirectoryNotEmpty => {}
            Err(error) => return Err(error).wrap_err_with(|| format!("prune directory {:?}", dir)),
        }
    }
    let _ = config;
    Ok(())
}

#[cfg(unix)]
fn apply_desired_link(link: &DesiredLink) -> Result<()> {
    use std::os::unix::fs::symlink;

    match fs::symlink_metadata(&link.output_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let existing = fs::read_link(&link.output_path)
                .wrap_err_with(|| format!("read symlink {:?}", link.output_path))?;
            if existing == link.target_text {
                return Ok(());
            }
        }
        Ok(_) => bail!(
            "desired output path is blocked by a non-symlink entry: {:?}",
            link.output_path
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            symlink(&link.target_text, &link.output_path).wrap_err_with(|| {
                format!(
                    "create symlink {:?} -> {:?}",
                    link.output_path, link.target_text
                )
            })?;
            return Ok(());
        }
        Err(error) => {
            return Err(error).wrap_err_with(|| format!("stat {:?}", link.output_path));
        }
    }

    let temp_path = link.output_path.with_file_name(format!(
        ".{}.media-importer.tmp-{}",
        link.output_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("link"),
        uuid::Uuid::new_v4()
    ));
    symlink(&link.target_text, &temp_path)
        .wrap_err_with(|| format!("create temporary symlink {:?}", temp_path))?;
    fs::rename(&temp_path, &link.output_path).wrap_err_with(|| {
        let _ = fs::remove_file(&temp_path);
        format!(
            "replace symlink {:?} -> {:?}",
            link.output_path, link.target_text
        )
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_desired_link(_link: &DesiredLink) -> Result<()> {
    bail!("build-tree symlink materialization is supported only on Unix platforms")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    use assert_fs::TempDir;
    use assert_fs::prelude::*;

    #[test]
    fn apply_desired_link_refuses_existing_non_symlink() {
        let temp = TempDir::new().expect("tempdir");
        let output = temp.child("output");
        output.write_str("user data").expect("user file");
        let link = DesiredLink {
            output_path: output.path().to_path_buf(),
            target_text: PathBuf::from("../blob"),
            blob_hash: BlobHash::new("a".repeat(64)).expect("hash"),
        };

        let error = apply_desired_link(&link).expect_err("non-symlink should block apply");

        assert!(
            error.to_string().contains("blocked by a non-symlink"),
            "{error:?}"
        );
        assert_eq!(
            fs::read_to_string(output.path()).expect("read user file"),
            "user data"
        );
    }
}
