use color_eyre::Result;
use media_importer::audit::audit_store_with_telemetry;
use media_importer::cli::{
    Cli, CliCommand, render_audit_report, render_build_tree_report, render_gc_report,
    render_import_report,
};
use media_importer::gc::{GcOutcome, collect_garbage_with_telemetry};
use media_importer::ingest::import_source_with_telemetry;
use media_importer::materialize::build_tree_with_telemetry;
use media_importer::telemetry::{
    HumanTelemetrySink, JsonlTelemetrySink, RendererTransport, TelemetryEvent, TelemetrySink,
    stdout_is_terminal,
};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

fn main() -> Result<std::process::ExitCode> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let parsed = Cli::parse_config()?;
    let terminal = stdout_is_terminal();
    let jsonl = parsed.output.uses_jsonl(terminal);
    let command_name = match &parsed.command {
        CliCommand::Import(_) => "import",
        CliCommand::BuildTree(_) => "build_tree",
        CliCommand::Audit(_) => "audit",
        CliCommand::Gc(_) => "gc",
    };
    // The selected renderer owns stdout.  In particular, human mode must not
    // construct a JSON writer merely because import happens to emit telemetry.
    let renderer: Arc<dyn TelemetrySink> = if jsonl {
        Arc::new(JsonlTelemetrySink::new())
    } else {
        Arc::new(HumanTelemetrySink::new(terminal, command_name))
    };
    let transport = Arc::new(RendererTransport::new(renderer));
    let telemetry: Arc<dyn TelemetrySink> = transport.clone();

    match parsed.command {
        CliCommand::Import(config) => {
            emit_started(&telemetry, "import", config.dry_run);
            match import_source_with_telemetry(config, Arc::clone(&telemetry)) {
                Ok(report) => {
                    emit_finished(&telemetry, "import", report.dry_run);
                    if jsonl {
                        emit_import_summary(&telemetry, &report);
                    } else {
                        telemetry.finish();
                        render_import_report(&report)?;
                    }
                    if jsonl {
                        telemetry.finish();
                    }
                    ensure_renderer_healthy(&telemetry)?;
                }
                Err(error) => {
                    emit_failed(&telemetry, jsonl, "import");
                    telemetry.finish();
                    return Err(error);
                }
            }
        }
        CliCommand::BuildTree(config) => {
            let dry_run = config.dry_run;
            emit_started(&telemetry, "build_tree", dry_run);
            match build_tree_with_telemetry(config, Arc::clone(&telemetry)) {
                Ok(report) => {
                    emit_finished(&telemetry, "build_tree", report.dry_run);
                    if jsonl {
                        telemetry.emit(
                            TelemetryEvent::new("build_tree", "command_summary")
                                .field("dry_run", report.dry_run)
                                .field("desired_links", report.desired_links)
                                .field("links_created", report.links_created)
                                .field("links_unchanged", report.links_unchanged)
                                .field("links_replaced", report.links_replaced)
                                .field("stale_links_removed", report.stale_links_removed)
                                .field("directories_created", report.directories_created)
                                .field("directories_pruned", report.directories_pruned),
                        );
                    } else {
                        telemetry.finish();
                        render_build_tree_report(&report)?;
                    }
                    if jsonl {
                        telemetry.finish();
                    }
                    ensure_renderer_healthy(&telemetry)?;
                }
                Err(error) => {
                    emit_failed(&telemetry, jsonl, "build_tree");
                    telemetry.finish();
                    return Err(error);
                }
            }
        }
        CliCommand::Audit(config) => {
            emit_started(&telemetry, "audit", false);
            let report = match audit_store_with_telemetry(config, Arc::clone(&telemetry)) {
                Ok(report) => report,
                Err(error) => {
                    emit_failed(&telemetry, jsonl, "audit");
                    telemetry.finish();
                    return Err(error);
                }
            };
            let clean = report.is_clean();
            emit_finished(&telemetry, "audit", false);
            if jsonl {
                telemetry.emit(
                    TelemetryEvent::new("audit", "command_summary")
                        .field("catalog_blobs", report.catalog_blobs)
                        .field("cas_blob_files", report.cas_blob_files)
                        .field("blobs_hashed", report.blobs_hashed)
                        .field("gc_candidates", report.gc_candidates)
                        .field("findings", report.findings.len() as u64)
                        .field("clean", clean),
                );
            } else {
                telemetry.finish();
                render_audit_report(&report)?;
            }
            if jsonl {
                telemetry.finish();
            }
            ensure_renderer_healthy(&telemetry)?;
            return Ok(if clean {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::from(2)
            });
        }
        CliCommand::Gc(config) => {
            let dry_run = config.dry_run;
            emit_started(&telemetry, "gc", dry_run);
            let outcome = match collect_garbage_with_telemetry(config, Arc::clone(&telemetry)) {
                Ok(outcome) => outcome,
                Err(error) => {
                    emit_failed(&telemetry, jsonl, "gc");
                    telemetry.finish();
                    return Err(error);
                }
            };
            match outcome {
                GcOutcome::Complete(report) => {
                    emit_finished(&telemetry, "gc", report.dry_run);
                    if jsonl {
                        emit_gc_summary(&telemetry, &report, false, "complete")
                    } else {
                        telemetry.finish();
                        render_gc_report(&report, false)?
                    }
                    if jsonl {
                        telemetry.finish();
                    }
                    ensure_renderer_healthy(&telemetry)?;
                }
                GcOutcome::Blocked(report) => {
                    emit_finished(&telemetry, "gc", report.dry_run);
                    if jsonl {
                        emit_gc_summary(&telemetry, &report, false, "blocked")
                    } else {
                        telemetry.finish();
                        render_gc_report(&report, false)?
                    }
                    if jsonl {
                        telemetry.finish();
                    }
                    ensure_renderer_healthy(&telemetry)?;
                    return Ok(std::process::ExitCode::from(2));
                }
                GcOutcome::Incomplete { report, error } => {
                    emit_finished(&telemetry, "gc", report.dry_run);
                    if jsonl {
                        emit_gc_summary(&telemetry, &report, true, "incomplete")
                    } else {
                        telemetry.finish();
                        render_gc_report(&report, true)?
                    }
                    if jsonl {
                        telemetry.finish();
                    }
                    return Err(error);
                }
            }
        }
    }

    Ok(std::process::ExitCode::SUCCESS)
}

fn ensure_renderer_healthy(telemetry: &Arc<dyn TelemetrySink>) -> Result<()> {
    if telemetry.failed() {
        color_eyre::eyre::bail!("telemetry renderer failed")
    }
    Ok(())
}

fn emit_started(sink: &Arc<dyn TelemetrySink>, command: &str, dry_run: bool) {
    sink.emit(TelemetryEvent::new(command, "command_started").field("dry_run", dry_run));
}
fn emit_failed(sink: &Arc<dyn TelemetrySink>, enabled: bool, command: &str) {
    if enabled {
        sink.emit(TelemetryEvent::new(command, "command_failed"));
    }
}
fn emit_finished(sink: &Arc<dyn TelemetrySink>, command: &str, dry_run: bool) {
    sink.emit(TelemetryEvent::new(command, "command_finished").field("dry_run", dry_run));
}
fn emit_import_summary(
    sink: &Arc<dyn TelemetrySink>,
    report: &media_importer::ingest::ImportReport,
) {
    sink.emit(
        TelemetryEvent::new("import", "command_summary")
            .field("dry_run", report.dry_run)
            .field("files_seen", report.files_seen)
            .field("files_skipped", report.files_skipped)
            .field("files_hashed", report.files_hashed)
            .field("bytes_seen", report.bytes_seen)
            .field("bytes_skipped", report.bytes_skipped)
            .field("bytes_hashed", report.bytes_hashed)
            .field("bytes_written", report.bytes_written)
            .field("blobs_created", report.blobs_created)
            .field("blobs_reused", report.blobs_reused)
            .field("source_records_inserted", report.source_records_inserted)
            .field("source_records_updated", report.source_records_updated),
    );
}
fn emit_gc_summary(
    sink: &Arc<dyn TelemetrySink>,
    report: &media_importer::gc::GcReport,
    incomplete: bool,
    outcome: &str,
) {
    sink.emit(
        TelemetryEvent::new("gc", "command_summary")
            .field("dry_run", report.dry_run)
            .field("incomplete", incomplete)
            .field("outcome", outcome)
            .field("findings", report.findings.len() as u64)
            .field("planned_marks", report.planned_marks)
            .field("planned_resurrections", report.planned_resurrections)
            .field("planned_sweeps", report.planned_sweeps)
            .field("completed_marks", report.completed_marks)
            .field("completed_resurrections", report.completed_resurrections)
            .field("completed_sweeps", report.completed_sweeps)
            .field("sweep_candidates_hashed", report.sweep_candidates_hashed)
            .field("catalog_blobs", report.catalog_blobs)
            .field("reachable_blobs", report.reachable_blobs)
            .field("cas_files_removed", report.cas_files_removed)
            .field("bytes_reclaimable", report.bytes_reclaimable)
            .field("bytes_unlinked", report.bytes_unlinked)
            .field("bytes_reclaimed", report.bytes_reclaimed),
    );
}
