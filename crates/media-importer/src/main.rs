use color_eyre::Result;
use media_importer::cli::{Cli, CliCommand, render_import_report};
use media_importer::ingest::import_source;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse_config()? {
        CliCommand::Import(config) => {
            let report = import_source(config)?;
            render_import_report(&report);
        }
    }

    Ok(())
}
