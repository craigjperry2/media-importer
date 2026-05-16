use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use color_eyre::Result;

use crate::config::{DEFAULT_CHUNK_SIZE, ImportConfig, ImportOptions};
use crate::ingest::ImportReport;

#[derive(Debug, Parser)]
#[command(author, version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Import(ImportArgs),
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
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE)]
    chunk_size: NonZeroUsize,
}

pub enum CliCommand {
    Import(ImportConfig),
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
                    chunk_size: args.chunk_size,
                })?;
                Ok(Self::Import(config))
            }
        }
    }
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
    }
}
