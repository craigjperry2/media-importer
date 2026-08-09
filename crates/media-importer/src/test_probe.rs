//! Opt-in process handshakes used only by integration-test child commands.
//!
//! This is deliberately private and has no CLI surface. A command only pauses
//! when its environment contains the test-specific probe specification.

use std::fs;
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
