//! Pure and stream-oriented BLAKE3 processing.
//!
//! This module deliberately does not open paths.  Filesystem policy (including
//! no-follow opens) belongs to the store, audit, and GC boundaries.

use std::io::{Read, Write};
use std::num::NonZeroUsize;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};

use crate::paths::BlobHash;

pub struct HashResult {
    pub hash: BlobHash,
    pub size_bytes: u64,
}

pub struct BlobHasher {
    hasher: blake3::Hasher,
    size_bytes: u64,
}

impl BlobHasher {
    pub fn new() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            size_bytes: 0,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) -> Result<()> {
        self.hasher.update(bytes);
        self.size_bytes = self
            .size_bytes
            .checked_add(u64::try_from(bytes.len()).wrap_err("convert hash chunk length")?)
            .ok_or_else(|| eyre!("hashed byte count overflow"))?;
        Ok(())
    }

    pub fn finish(self) -> Result<HashResult> {
        Ok(HashResult {
            hash: BlobHash::new(self.hasher.finalize().to_hex().to_string())?,
            size_bytes: self.size_bytes,
        })
    }
}

impl Default for BlobHasher {
    fn default() -> Self {
        Self::new()
    }
}

pub fn hash_reader_with_chunk_observer<R: Read>(
    reader: &mut R,
    chunk_size: NonZeroUsize,
    mut on_chunk_read: impl FnMut(u64) -> Result<()>,
) -> Result<HashResult> {
    let mut buffer = buffer(chunk_size, "hash")?;
    let mut hasher = BlobHasher::new();
    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .wrap_err("read stream while hashing")?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read])?;
        on_chunk_read(u64::try_from(bytes_read).wrap_err("convert read chunk length")?)?;
    }
    hasher.finish()
}

/// Hash and copy one already-open stream in a single pass.
pub fn hash_reader_to_writer_with_chunk_observer<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    chunk_size: NonZeroUsize,
    mut on_chunk_written: impl FnMut(u64) -> Result<()>,
) -> Result<HashResult> {
    let mut buffer = buffer(chunk_size, "copy")?;
    let mut hasher = BlobHasher::new();
    loop {
        let bytes_read = reader.read(&mut buffer).wrap_err("read source stream")?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read])?;
        writer
            .write_all(&buffer[..bytes_read])
            .wrap_err("write staging stream")?;
        on_chunk_written(u64::try_from(bytes_read).wrap_err("convert written chunk length")?)?;
    }
    hasher.finish()
}

fn buffer(chunk_size: NonZeroUsize, operation: &str) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(chunk_size.get())
        .map_err(|error| eyre!("allocate {operation} buffer: {error}"))?;
    buffer.resize(chunk_size.get(), 0);
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn byte_slice_has_expected_blake3_digest_and_length() {
        let mut hasher = BlobHasher::new();
        hasher.update(b"hello ").expect("first bytes");
        hasher.update(b"world").expect("second bytes");
        let result = hasher.finish().expect("digest");
        assert_eq!(
            result.hash.to_string(),
            blake3::hash(b"hello world").to_hex().to_string()
        );
        assert_eq!(result.size_bytes, 11);
    }

    #[test]
    fn synthetic_reader_is_hashed_without_filesystem_access() {
        let mut reader = Cursor::new(b"abcdefgh".to_vec());
        let result = hash_reader_with_chunk_observer(
            &mut reader,
            NonZeroUsize::new(3).expect("non-zero"),
            |_| Ok(()),
        )
        .expect("hash reader");
        assert_eq!(
            result.hash.to_string(),
            blake3::hash(b"abcdefgh").to_hex().to_string()
        );
        assert_eq!(result.size_bytes, 8);
    }
}
