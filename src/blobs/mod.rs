//! Content-addressed blob storage.
//!
//! Blobs are stored by their BLAKE3 hash, sharded into directories
//! by the first byte of the hash (like Git objects).

mod storage;

pub use storage::BlobStorage;
