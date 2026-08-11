//! Opt-in process handshakes used only by integration-test child commands.
//!
//! This is deliberately private and has no CLI surface. A command only pauses
//! when its environment contains the test-specific probe specification.

use std::fs;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail};

const PROBE_ENV: &str = "MEDIA_IMPORTER_TEST_LIFECYCLE_PROBE";
const WAIT: Duration = Duration::from_secs(10);

pub(crate) fn pause(stage: &str) -> Result<()> {
    let Some(specification) = std::env::var_os(PROBE_ENV) else {
        return Ok(());
    };
    let specification = specification
        .into_string()
        .map_err(|_| color_eyre::eyre::eyre!("{PROBE_ENV} must be valid Unicode"))?;
    let Some((directory, requested_stage)) = specification.rsplit_once('|') else {
        bail!("{PROBE_ENV} must be PATH|STAGE");
    };
    if !requested_stage
        .split(',')
        .any(|candidate| candidate == stage)
    {
        return Ok(());
    }

    let directory = PathBuf::from(directory);
    let ready = directory.join(format!("{stage}.ready"));
    let release = directory.join(format!("{stage}.release"));
    // A test may request a real multi-worker barrier by writing the number of
    // participants before launching the command. This keeps the normal
    // one-participant lifecycle handshakes unchanged while proving CAS races
    // at the actual pre-install boundary.
    let participants = directory.join(format!("{stage}.participants"));
    let required = fs::read_to_string(&participants)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1);
    if required > 1 {
        let current_thread = thread::current();
        let name = current_thread.name().unwrap_or("unnamed");
        let arrived = directory.join(format!("{stage}.arrived-{}-{name}", std::process::id()));
        let _ = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(arrived);
        let deadline = Instant::now() + WAIT;
        loop {
            let count = fs::read_dir(&directory)
                .wrap_err_with(|| format!("read lifecycle probe directory {directory:?}"))?
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{stage}.arrived-"))
                })
                .count();
            if count >= required {
                break;
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for {required} lifecycle probe participants at {stage}");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    fs::write(&ready, "ready").wrap_err_with(|| format!("write lifecycle probe {ready:?}"))?;

    let deadline = Instant::now() + WAIT;
    while !release.exists() {
        if Instant::now() >= deadline {
            bail!("timed out waiting for lifecycle probe release {release:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

pub(crate) fn pause_or_fail(stage: &str) -> Result<()> {
    pause(stage)?;
    let Some(specification) = std::env::var_os(PROBE_ENV) else {
        return Ok(());
    };
    let specification = specification
        .into_string()
        .map_err(|_| color_eyre::eyre::eyre!("{PROBE_ENV} must be valid Unicode"))?;
    let Some((directory, requested_stage)) = specification.rsplit_once('|') else {
        bail!("{PROBE_ENV} must be PATH|STAGE");
    };
    if requested_stage
        .split(',')
        .any(|candidate| candidate == stage)
        && PathBuf::from(directory)
            .join(format!("{stage}.fail"))
            .exists()
    {
        bail!("injected lifecycle probe failure at {stage}");
    }
    Ok(())
}

/// Whether a private integration-test probe has enabled `stage`.
pub(crate) fn enabled(stage: &str) -> Result<bool> {
    let Some(specification) = std::env::var_os(PROBE_ENV) else {
        return Ok(false);
    };
    let specification = specification
        .into_string()
        .map_err(|_| color_eyre::eyre::eyre!("{PROBE_ENV} must be valid Unicode"))?;
    let Some((_, requested_stage)) = specification.rsplit_once('|') else {
        bail!("{PROBE_ENV} must be PATH|STAGE");
    };
    Ok(requested_stage
        .split(',')
        .any(|candidate| candidate == stage))
}

/// Test-only lifecycle seam for verifying that callers join a worker which
/// panics before it can report readiness. This has no effect unless the same
/// private integration-test probe environment is configured.
pub(crate) fn pause_or_panic(stage: &str) -> Result<()> {
    pause(stage)?;
    let Some(specification) = std::env::var_os(PROBE_ENV) else {
        return Ok(());
    };
    let specification = specification
        .into_string()
        .map_err(|_| color_eyre::eyre::eyre!("{PROBE_ENV} must be valid Unicode"))?;
    let Some((directory, requested_stage)) = specification.rsplit_once('|') else {
        bail!("{PROBE_ENV} must be PATH|STAGE");
    };
    if requested_stage
        .split(',')
        .any(|candidate| candidate == stage)
        && PathBuf::from(directory)
            .join(format!("{stage}.panic"))
            .exists()
    {
        panic!("injected lifecycle probe panic at {stage}");
    }
    Ok(())
}
