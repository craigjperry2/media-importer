use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use color_eyre::Result;

use crate::audit::AuditReport;
use crate::config::{
    AuditConfig, AuditOptions, BuildTreeConfig, BuildTreeOptions, DEFAULT_CHUNK_SIZE,
    DEFAULT_HASH_DIGITS, GcConfig, GcOptions, ImportConfig, ImportOptions,
};
use crate::gc::{GcAction, GcActionKind, GcReport, SweepSourceState};
use crate::ingest::ImportReport;
use crate::materialize::BuildTreeReport;

#[derive(Debug, Parser)]
#[command(author, version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Import(ImportArgs),
    BuildTree(BuildTreeArgs),
    /// Verify catalog and CAS integrity without modifying either.
    Audit(AuditArgs),
    /// Mark unreachable blobs, then sweep blobs marked before this run.
    ///
    /// Reachability comes from catalog source records, not current source-file
    /// contents. Dry-run performs the complete read-only preflight. Present
    /// sweep candidates are fully hashed, so GC may be I/O intensive. The store
    /// waits indefinitely for conflicting cooperating commands automatically.
    Gc(GcArgs),
}

#[derive(Debug, Parser)]
struct AuditArgs {
    #[arg(long)]
    store: PathBuf,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE)]
    chunk_size: NonZeroUsize,
}

#[derive(Debug, Parser)]
struct GcArgs {
    #[arg(long)]
    store: PathBuf,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE.get())]
    chunk_size: usize,
}

#[derive(Debug, Parser)]
struct ImportArgs {
    #[arg(long)]
    store: PathBuf,
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
    /// Hash every source file instead of reusing unchanged source metadata.
    #[arg(long)]
    no_metadata_skip: bool,
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE)]
    chunk_size: NonZeroUsize,
}

#[derive(Debug, Parser)]
struct BuildTreeArgs {
    #[arg(long)]
    store: PathBuf,
    #[arg(long)]
    browse_tree: PathBuf,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value_t = DEFAULT_HASH_DIGITS)]
    hash_digits: NonZeroUsize,
}

pub enum CliCommand {
    Import(ImportConfig),
    BuildTree(BuildTreeConfig),
    Audit(AuditConfig),
    Gc(GcConfig),
}

impl Cli {
    pub fn parse_config() -> Result<CliCommand> {
        Self::parse().try_into()
    }
}

impl TryFrom<Cli> for CliCommand {
    type Error = color_eyre::Report;

    fn try_from(cli: Cli) -> Result<Self> {
        match cli.command {
            Command::Import(args) => {
                let config = ImportConfig::from_options(ImportOptions {
                    store: args.store,
                    source: args.source,
                    db: args.db,
                    dry_run: args.dry_run,
                    metadata_skip: !args.no_metadata_skip,
                    chunk_size: args.chunk_size,
                })?;
                Ok(Self::Import(config))
            }
            Command::BuildTree(args) => {
                let config = BuildTreeConfig::from_options(BuildTreeOptions {
                    store: args.store,
                    browse_tree: args.browse_tree,
                    db: args.db,
                    dry_run: args.dry_run,
                    hash_digits: args.hash_digits,
                })?;
                Ok(Self::BuildTree(config))
            }
            Command::Audit(args) => Ok(Self::Audit(AuditConfig::from_options(AuditOptions {
                store: args.store,
                db: args.db,
                chunk_size: args.chunk_size,
            })?)),
            Command::Gc(args) => Ok(Self::Gc(GcConfig::from_options(GcOptions {
                store: args.store,
                db: args.db,
                dry_run: args.dry_run,
                chunk_size: args.chunk_size,
            })?)),
        }
    }
}

pub fn render_gc_report(report: &GcReport, incomplete: bool) {
    if !report.findings.is_empty() {
        render_findings(&report.findings);
        println!();
    }
    if report.findings.is_empty() {
        let mut actions: Vec<&GcAction> = report.actions.iter().collect();
        actions.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.hash.cmp(&right.hash))
        });
        for action in actions {
            render_gc_action(action, report.dry_run);
        }
        if !report.actions.is_empty() {
            println!();
        }
    }

    if incomplete {
        println!("GC incomplete");
        println!(
            "Planned actions: marks={} resurrections={} sweeps={}",
            report.planned_marks, report.planned_resurrections, report.planned_sweeps
        );
        println!(
            "Completed catalog actions: marks={} resurrections={} sweeps={}",
            report.completed_marks, report.completed_resurrections, report.completed_sweeps
        );
        println!("CAS files removed: {}", report.cas_files_removed);
        println!("Logical bytes unlinked: {}", report.bytes_unlinked);
        println!("Bytes reclaimed: {}", report.bytes_reclaimed);
    } else if !report.findings.is_empty() {
        println!("GC blocked");
        println!("Blobs planned for marking: {}", report.planned_marks);
        println!(
            "Blobs planned for resurrection: {}",
            report.planned_resurrections
        );
        println!("Blobs planned for sweeping: {}", report.planned_sweeps);
    } else if report.dry_run {
        println!("GC dry run complete");
        println!("Blobs that would be marked: {}", report.planned_marks);
        println!(
            "Blobs that would be resurrected: {}",
            report.planned_resurrections
        );
        println!("Blobs that would be swept: {}", report.planned_sweeps);
        println!(
            "Bytes that would be reclaimed: {}",
            report.bytes_reclaimable
        );
    } else {
        println!("GC complete");
        println!("Blobs marked: {}", report.completed_marks);
        println!("Blobs resurrected: {}", report.completed_resurrections);
        println!("Blobs swept: {}", report.completed_sweeps);
        println!("CAS files removed: {}", report.cas_files_removed);
        println!("Bytes reclaimed: {}", report.bytes_reclaimed);
    }
    println!("Catalog blobs: {}", report.catalog_blobs);
    println!("Reachable blobs: {}", report.reachable_blobs);
    println!(
        "Sweep candidates hashed: {}",
        report.sweep_candidates_hashed
    );
    println!("Findings: {}", report.findings.len());
}

fn render_gc_action(action: &GcAction, dry_run: bool) {
    let prefix = if dry_run { "WOULD_" } else { "" };
    match action.kind {
        GcActionKind::Mark => {
            println!("{prefix}MARK {} bytes={}", action.hash, action.size_bytes);
        }
        GcActionKind::Resurrect => println!("{prefix}RESURRECT {}", action.hash),
        GcActionKind::Sweep => {
            if action.sweep_source_state == Some(SweepSourceState::AlreadyAbsent) {
                println!(
                    "{prefix}SWEEP {} bytes={} state=already-absent",
                    action.hash, action.size_bytes
                );
            } else {
                println!("{prefix}SWEEP {} bytes={}", action.hash, action.size_bytes);
            }
        }
    }
}

fn render_findings(findings: &[crate::integrity::IntegrityFinding]) {
    for finding in findings {
        if finding.details.is_empty() {
            println!("{} {}", finding.category, finding.identity);
        } else {
            println!(
                "{} {} {}",
                finding.category, finding.identity, finding.details
            );
        }
    }
}

pub fn render_audit_report(report: &AuditReport) {
    if report.is_clean() {
        println!("Audit clean");
    } else {
        for finding in &report.findings {
            if finding.details.is_empty() {
                println!("{} {}", finding.category, finding.identity);
            } else {
                println!(
                    "{} {} {}",
                    finding.category, finding.identity, finding.details
                );
            }
        }
        println!();
        println!("Audit complete");
    }
    println!("Catalog blobs: {}", report.catalog_blobs);
    println!("CAS blob files: {}", report.cas_blob_files);
    println!("Blobs hashed: {}", report.blobs_hashed);
    println!("GC candidates: {}", report.gc_candidates);
    println!("Findings: {}", report.findings.len());
}

pub fn render_import_report(report: &ImportReport) {
    if report.dry_run {
        println!("Dry run complete");
        println!("Files seen: {}", report.files_seen);
        println!("Blobs that would be created: {}", report.blobs_created);
        println!("Blobs that would be reused: {}", report.blobs_reused);
        println!(
            "Source records that would be inserted: {}",
            report.source_records_inserted
        );
        println!(
            "Source records that would be updated: {}",
            report.source_records_updated
        );
        println!("Bytes seen: {}", report.bytes_seen);
        println!("Bytes that would be written: {}", report.bytes_written);
        println!(
            "Files that would skip content reads: {}",
            report.files_skipped
        );
        println!(
            "Bytes that would skip content reads: {}",
            report.bytes_skipped
        );
        println!("Files that would be hashed: {}", report.files_hashed);
        println!("Bytes that would be hashed: {}", report.bytes_hashed);
    } else {
        println!("Import complete");
        println!("Files seen: {}", report.files_seen);
        println!("Blobs created: {}", report.blobs_created);
        println!("Blobs reused: {}", report.blobs_reused);
        println!(
            "Source records inserted: {}",
            report.source_records_inserted
        );
        println!("Source records updated: {}", report.source_records_updated);
        println!("Bytes seen: {}", report.bytes_seen);
        println!("Bytes written: {}", report.bytes_written);
        println!("Files skipped: {}", report.files_skipped);
        println!("Bytes skipped: {}", report.bytes_skipped);
        println!("Files hashed: {}", report.files_hashed);
        println!("Bytes hashed: {}", report.bytes_hashed);
    }
}

pub fn render_build_tree_report(report: &BuildTreeReport) {
    if report.dry_run {
        println!("Dry run complete");
        println!("Desired links: {}", report.desired_links);
        println!("Links that would be created: {}", report.links_created);
        println!(
            "Links that would be left unchanged: {}",
            report.links_unchanged
        );
        println!("Links that would be replaced: {}", report.links_replaced);
        println!(
            "Stale links that would be removed: {}",
            report.stale_links_removed
        );
        println!(
            "Directories that would be created: {}",
            report.directories_created
        );
        println!(
            "Directories that would be pruned: {}",
            report.directories_pruned
        );
    } else {
        println!("Build tree complete");
        println!("Desired links: {}", report.desired_links);
        println!("Links created: {}", report.links_created);
        println!("Links unchanged: {}", report.links_unchanged);
        println!("Links replaced: {}", report.links_replaced);
        println!("Stale links removed: {}", report.stale_links_removed);
        println!("Directories created: {}", report.directories_created);
        println!("Directories pruned: {}", report.directories_pruned);
    }
}
