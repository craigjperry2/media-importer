//! Cooperative, process-scoped coordination for one canonical store root.

use std::fs::{File, OpenOptions};

use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use tracing::trace;

use crate::paths::StoreRoot;
use crate::test_probe;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

impl LockMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Exclusive => "exclusive",
        }
    }
}

/// An advisory lock retained by owning an open handle to the store directory.
#[must_use = "the lock is released when its guard is dropped"]
pub struct StoreRunLock {
    _directory: File,
}

impl StoreRunLock {
    pub fn acquire(store_root: &StoreRoot, command: &'static str, mode: LockMode) -> Result<Self> {
        let path = store_root.path();
        test_probe::pause("lock-before-open")?;
        let directory = open_store_directory(path).wrap_err_with(|| {
            format!(
                "open store directory for {command} {} coordination at {path:?}",
                mode.as_str()
            )
        })?;
        let metadata = directory.metadata().wrap_err_with(|| {
            format!(
                "inspect opened store directory for {command} {} coordination at {path:?}",
                mode.as_str()
            )
        })?;
        if !metadata.is_dir() {
            return Err(color_eyre::eyre::eyre!(
                "opened store coordination target for {command} {} mode is not a directory: {path:?}",
                mode.as_str()
            ));
        }
        trace!(store = ?path, command, mode = mode.as_str(), "requesting store run lock");
        match mode {
            LockMode::Shared => directory.lock_shared(),
            LockMode::Exclusive => directory.lock(),
        }
        .wrap_err_with(|| {
            format!(
                "acquire {} store run lock for {command} at {path:?}",
                mode.as_str()
            )
        })?;
        trace!(store = ?path, command, mode = mode.as_str(), "acquired store run lock");
        Ok(Self {
            _directory: directory,
        })
    }
}

#[cfg(unix)]
fn open_store_directory(path: &std::path::Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn open_store_directory(path: &std::path::Path) -> std::io::Result<File> {
    File::open(path)
}
