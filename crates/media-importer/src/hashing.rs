use std::fs::File;
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::path::Path;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};

use crate::paths::BlobHash;

pub struct HashResult {
    pub hash: BlobHash,
    pub size_bytes: u64,
}

pub fn hash_file(path: &Path, chunk_size: NonZeroUsize) -> Result<HashResult> {
    let mut file = File::open(path).wrap_err_with(|| format!("open source file {:?}", path))?;
    hash_open_file(&mut file, chunk_size)
}

pub fn hash_open_file(file: &mut File, chunk_size: NonZeroUsize) -> Result<HashResult> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(chunk_size.get())
        .map_err(|error| eyre!("allocate hash buffer: {error}"))?;
    buffer.resize(chunk_size.get(), 0);
    let mut size_bytes = 0_u64;

    loop {
        let bytes_read = file.read(&mut buffer).wrap_err("read file while hashing")?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
        size_bytes = size_bytes
            .checked_add(bytes_read as u64)
            .ok_or_else(|| eyre!("hashed byte count overflow"))?;
    }

    Ok(HashResult {
        hash: BlobHash::new(hasher.finalize().to_hex().to_string())?,
        size_bytes,
    })
}

pub fn hash_reader_to_writer(
    reader: &mut File,
    writer: &mut File,
    chunk_size: NonZeroUsize,
) -> Result<HashResult> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(chunk_size.get())
        .map_err(|error| eyre!("allocate copy buffer: {error}"))?;
    buffer.resize(chunk_size.get(), 0);
    let mut size_bytes = 0_u64;

    loop {
        let bytes_read = reader.read(&mut buffer).wrap_err("read source file")?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
        writer
            .write_all(&buffer[..bytes_read])
            .wrap_err("write staging file")?;
        size_bytes = size_bytes
            .checked_add(bytes_read as u64)
            .ok_or_else(|| eyre!("copied byte count overflow"))?;
    }

    Ok(HashResult {
        hash: BlobHash::new(hasher.finalize().to_hex().to_string())?,
        size_bytes,
    })
}
