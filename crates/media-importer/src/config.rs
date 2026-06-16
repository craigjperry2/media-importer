use std::num::NonZeroUsize;
use std::path::PathBuf;

use color_eyre::Result;
use color_eyre::eyre::bail;

use crate::paths::{
    BrowseTreeRoot, SourceRoot, StoreRoot, validate_build_tree_no_overlaps, validate_db_path,
    validate_no_overlaps,
};

pub const DEFAULT_CHUNK_SIZE: NonZeroUsize =
    NonZeroUsize::new(1024 * 1024).expect("default chunk size is non-zero");
pub const DEFAULT_HASH_DIGITS: NonZeroUsize = NonZeroUsize::new(6).expect("default is non-zero");

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

#[derive(Clone, Debug)]
pub struct BuildTreeConfig {
    pub store_root: StoreRoot,
    pub db_path: PathBuf,
    pub browse_tree_root: BrowseTreeRoot,
    pub dry_run: bool,
    pub hash_digits: NonZeroUsize,
}

#[derive(Clone, Debug)]
pub struct BuildTreeOptions {
    pub store: PathBuf,
    pub browse_tree: PathBuf,
    pub db: Option<PathBuf>,
    pub dry_run: bool,
    pub hash_digits: NonZeroUsize,
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

impl BuildTreeConfig {
    pub fn from_options(options: BuildTreeOptions) -> Result<Self> {
        if options.hash_digits.get() > 64 {
            bail!("--hash-digits must be in the range 1..=64");
        }
        let store_root = StoreRoot::validate_existing_for_build_tree(&options.store)?;
        let browse_tree_root = BrowseTreeRoot::validate(&options.browse_tree)?;
        let db_was_explicit = options.db.is_some();
        let db_path = match options.db {
            Some(path) => validate_db_path(&path)?,
            None => store_root.default_db_path(),
        };
        validate_build_tree_no_overlaps(&store_root, &browse_tree_root, &db_path, db_was_explicit)?;
        Ok(Self {
            store_root,
            db_path,
            browse_tree_root,
            dry_run: options.dry_run,
            hash_digits: options.hash_digits,
        })
    }
}
