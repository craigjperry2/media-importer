use color_eyre::Result;
use media_importer::audit::audit_store;
use media_importer::cli::{
    Cli, CliCommand, render_audit_report, render_build_tree_report, render_import_report,
};
use media_importer::ingest::import_source;
use media_importer::materialize::build_tree;
use tracing_subscriber::EnvFilter;

fn main() -> Result<std::process::ExitCode> {
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
        CliCommand::BuildTree(config) => {
            let report = build_tree(config)?;
            render_build_tree_report(&report);
        }
        CliCommand::Audit(config) => {
            let report = audit_store(config)?;
            let clean = report.is_clean();
            render_audit_report(&report);
            return Ok(if clean {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::from(2)
            });
        }
    }

    Ok(std::process::ExitCode::SUCCESS)
}
