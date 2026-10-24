pub mod audit;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod gc;
pub mod hashing;
pub mod ingest;
pub mod integrity;
pub mod materialize;
pub mod paths;
pub mod run_lock;
pub mod scanner;
pub mod store;
pub mod telemetry;

mod test_probe;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("media-importer supports Linux and macOS only");
