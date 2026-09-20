//! Opt-in process handshakes used only by integration-test child commands.
//!
//! This is deliberately private and has no CLI surface. A command only pauses
//! when its environment contains the test-specific probe specification.

use std::fs;
use std::fs::OpenOptions;
use std::mem::size_of;
use std::path::PathBuf;
use std::ptr;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use crate::catalog::sqlite_ffi as ffi;
use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail};

const PROBE_ENV: &str = "MEDIA_IMPORTER_TEST_LIFECYCLE_PROBE";
const WAIT: Duration = Duration::from_secs(10);
const CHECKPOINT_VFS_NAME: &[u8] = b"media-importer-checkpoint-probe\0";

#[repr(C)]
struct CheckpointProbeFile {
    base: ffi::sqlite3_file,
    inner: *mut ffi::sqlite3_file,
}

static CHECKPOINT_PROBE_VFS: OnceLock<usize> = OnceLock::new();

/// Install a private VFS only for subprocesses that request the exact
/// in-checkpoint lifecycle seam. SQLite sends `SQLITE_FCNTL_CKPT_START` from
/// its WAL checkpoint implementation, after the VM's `OP_Checkpoint` has
/// entered that implementation. A progress handler is not sufficient: it can
/// run before that opcode.
pub(crate) fn install_checkpoint_start_vfs() -> Result<()> {
    if !enabled("catalog-writer-during-passive-checkpoint")? {
        return Ok(());
    }
    let pointer = *CHECKPOINT_PROBE_VFS.get_or_init(|| {
        // SAFETY: SQLite owns its registered default VFS for the process
        // lifetime. We copy its table into a deliberately leaked wrapper and
        // retain the original pointer for forwarding every VFS operation.
        unsafe {
            let original = ffi::sqlite3_vfs_find(ptr::null());
            assert!(!original.is_null(), "SQLite must provide a default VFS");
            let mut wrapped = *original;
            wrapped.zName = CHECKPOINT_VFS_NAME.as_ptr().cast();
            wrapped.pAppData = original.cast();
            wrapped.szOsFile += size_of::<CheckpointProbeFile>() as i32;
            wrapped.xOpen = Some(checkpoint_probe_vfs_open);
            let wrapped = Box::into_raw(Box::new(wrapped));
            let result = ffi::sqlite3_vfs_register(wrapped, 1);
            assert_eq!(result, ffi::SQLITE_OK, "register checkpoint probe VFS");
            wrapped as usize
        }
    });
    if pointer == 0 {
        bail!("checkpoint probe VFS registration returned a null pointer");
    }
    Ok(())
}

unsafe fn inner_file(file: *mut ffi::sqlite3_file) -> *mut ffi::sqlite3_file {
    // SAFETY: `checkpoint_probe_vfs_open` constructs this prefix before SQLite
    // can invoke an I/O method on the file.
    unsafe { (*(file.cast::<CheckpointProbeFile>())).inner }
}

macro_rules! forward_file_method {
    ($name:ident($($argument:ident: $argument_type:ty),*) -> $return_type:ty, $field:ident) => {
        unsafe extern "C" fn $name(
            file: *mut ffi::sqlite3_file,
            $($argument: $argument_type),*
        ) -> $return_type {
            // SAFETY: the wrapper forwards to the original file object and
            // method table created by the default SQLite VFS.
            unsafe {
                let inner = inner_file(file);
                ((*(*inner).pMethods).$field.expect("default SQLite VFS method"))(
                    inner,
                    $($argument),*
                )
            }
        }
    };
}

forward_file_method!(checkpoint_probe_close() -> i32, xClose);
forward_file_method!(checkpoint_probe_read(buffer: *mut std::ffi::c_void, amount: i32, offset: i64) -> i32, xRead);
forward_file_method!(checkpoint_probe_write(buffer: *const std::ffi::c_void, amount: i32, offset: i64) -> i32, xWrite);
forward_file_method!(checkpoint_probe_truncate(size: i64) -> i32, xTruncate);
forward_file_method!(checkpoint_probe_sync(flags: i32) -> i32, xSync);
forward_file_method!(checkpoint_probe_file_size(size: *mut i64) -> i32, xFileSize);
forward_file_method!(checkpoint_probe_lock(level: i32) -> i32, xLock);
forward_file_method!(checkpoint_probe_unlock(level: i32) -> i32, xUnlock);
forward_file_method!(checkpoint_probe_check_reserved_lock(result: *mut i32) -> i32, xCheckReservedLock);
forward_file_method!(checkpoint_probe_sector_size() -> i32, xSectorSize);
forward_file_method!(checkpoint_probe_device_characteristics() -> i32, xDeviceCharacteristics);
forward_file_method!(checkpoint_probe_shm_map(page: i32, page_size: i32, extend: i32, result: *mut *mut std::ffi::c_void) -> i32, xShmMap);
forward_file_method!(checkpoint_probe_shm_lock(offset: i32, count: i32, flags: i32) -> i32, xShmLock);
forward_file_method!(checkpoint_probe_shm_unmap(delete: i32) -> i32, xShmUnmap);
forward_file_method!(checkpoint_probe_fetch(offset: i64, amount: i32, result: *mut *mut std::ffi::c_void) -> i32, xFetch);
forward_file_method!(checkpoint_probe_unfetch(offset: i64, value: *mut std::ffi::c_void) -> i32, xUnfetch);

unsafe extern "C" fn checkpoint_probe_shm_barrier(file: *mut ffi::sqlite3_file) {
    // SAFETY: see `forward_file_method`; this is the void-returning variant.
    unsafe {
        let inner = inner_file(file);
        ((*(*inner).pMethods)
            .xShmBarrier
            .expect("default SQLite VFS xShmBarrier"))(inner);
    }
}

unsafe extern "C" fn checkpoint_probe_file_control(
    file: *mut ffi::sqlite3_file,
    operation: i32,
    argument: *mut std::ffi::c_void,
) -> i32 {
    if operation == ffi::SQLITE_FCNTL_CKPT_START {
        // This callback is made by SQLite's WAL checkpoint code, not by the
        // statement VM before OP_Checkpoint. Do not move this to Rust-side
        // code: doing so would reintroduce the pre-checkpoint false seam.
        if pause("catalog-writer-during-passive-checkpoint").is_err() {
            return ffi::SQLITE_IOERR;
        }
    }
    // SAFETY: the original VFS owns the underlying file and its method table.
    unsafe {
        let inner = inner_file(file);
        ((*(*inner).pMethods)
            .xFileControl
            .expect("default SQLite VFS xFileControl"))(inner, operation, argument)
    }
}

static CHECKPOINT_PROBE_IO_METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 3,
    xClose: Some(checkpoint_probe_close),
    xRead: Some(checkpoint_probe_read),
    xWrite: Some(checkpoint_probe_write),
    xTruncate: Some(checkpoint_probe_truncate),
    xSync: Some(checkpoint_probe_sync),
    xFileSize: Some(checkpoint_probe_file_size),
    xLock: Some(checkpoint_probe_lock),
    xUnlock: Some(checkpoint_probe_unlock),
    xCheckReservedLock: Some(checkpoint_probe_check_reserved_lock),
    xFileControl: Some(checkpoint_probe_file_control),
    xSectorSize: Some(checkpoint_probe_sector_size),
    xDeviceCharacteristics: Some(checkpoint_probe_device_characteristics),
    xShmMap: Some(checkpoint_probe_shm_map),
    xShmLock: Some(checkpoint_probe_shm_lock),
    xShmBarrier: Some(checkpoint_probe_shm_barrier),
    xShmUnmap: Some(checkpoint_probe_shm_unmap),
    xFetch: Some(checkpoint_probe_fetch),
    xUnfetch: Some(checkpoint_probe_unfetch),
};

unsafe extern "C" fn checkpoint_probe_vfs_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: i32,
    output_flags: *mut i32,
) -> i32 {
    // SAFETY: `CHECKPOINT_PROBE_VFS` is initialized from SQLite's registered
    // default VFS before this wrapper is registered. The allocation SQLite
    // supplies is enlarged by `CheckpointProbeFile` in the copied VFS table.
    unsafe {
        let original = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        assert!(
            !original.is_null(),
            "checkpoint VFS must wrap a default VFS"
        );
        let wrapped = file.cast::<CheckpointProbeFile>();
        let inner = wrapped.add(1).cast::<ffi::sqlite3_file>();
        let result = ((*original).xOpen.expect("default SQLite VFS xOpen"))(
            original,
            name,
            inner,
            flags,
            output_flags,
        );
        if result == ffi::SQLITE_OK {
            (*wrapped).inner = inner;
            (*wrapped).base.pMethods = &CHECKPOINT_PROBE_IO_METHODS;
        }
        result
    }
}

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
