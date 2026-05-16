use std::num::NonZeroUsize;
use std::path::PathBuf;

use color_eyre::Result;

use crate::paths::{SourceRoot, StoreRoot, validate_db_path, validate_no_overlaps};

pub const DEFAULT_CHUNK_SIZE: NonZeroUsize =
    NonZeroUsize::new(1024 * 1024).expect("default chunk size is non-zero");

#[derive(Clone, Debug)]
pub struct ImportConfig {
    pub store_root: StoreRoot,
    pub db_path: PathBuf,
    pub source_root: SourceRoot,
    pub dry_run: bool,
    pub chunk_size: NonZeroUsize,
}

#[derive(Clone, Debug)]
pub struct ImportOptions {
    pub store: PathBuf,
    pub source: PathBuf,
    pub db: Option<PathBuf>,
    pub dry_run: bool,
    pub chunk_size: NonZeroUsize,
}

impl ImportConfig {
    pub fn from_options(options: ImportOptions) -> Result<Self> {
        let source_root = SourceRoot::validate(&options.source)?;
        let store_root = StoreRoot::validate(&options.store)?;
        let db_was_explicit = options.db.is_some();
        let db_path = match options.db {
            Some(path) => validate_db_path(&path)?,
            None => store_root.default_db_path(),
        };

        validate_no_overlaps(&source_root, &store_root, &db_path, db_was_explicit)?;

        Ok(Self {
            store_root,
            db_path,
            source_root,
            dry_run: options.dry_run,
            chunk_size: options.chunk_size,
        })
    }
}
