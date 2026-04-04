//! Blob storage implementation.

use crate::error::{Result, StoreError};
use crate::types::{Blob, Hash};
use lru::LruCache;
use parking_lot::Mutex;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

/// Magic bytes for blob files.
const BLOB_MAGIC: &[u8; 4] = b"BLB\0";

/// Current blob format version.
const BLOB_VERSION: u8 = 1;

/// Maximum blob content size: 1 GiB. Prevents unbounded allocations
/// when reading potentially malformed blob files from disk.
const MAX_BLOB_SIZE: u64 = 1024 * 1024 * 1024;

/// Cached blob data (content + content_type).
#[derive(Clone)]
struct CachedBlob {
    content: Vec<u8>,
    content_type: String,
}

/// Content-addressed blob storage.
pub struct BlobStorage {
    /// Base directory for blobs.
    path: PathBuf,

    /// LRU cache for recently accessed blobs.
    cache: Mutex<LruCache<Hash, CachedBlob>>,
}

impl BlobStorage {
    /// Create a new blob storage at the given path.
    pub fn new(path: impl AsRef<Path>, cache_size: usize) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(&path)?;

        let cache_size = NonZeroUsize::new(cache_size.max(1)).unwrap();

        Ok(Self {
            path,
            cache: Mutex::new(LruCache::new(cache_size)),
        })
    }

    /// Store a blob, returning its hash.
    ///
    /// If the blob already exists, this is a no-op and returns the existing hash.
    pub fn store(&self, content: &[u8], content_type: &str) -> Result<Hash> {
        let hash = Hash::from_bytes(content);

        // Check if already exists
        if self.exists(&hash) {
            return Ok(hash);
        }

        // Create shard directory
        let shard_dir = self.shard_path(&hash);
        fs::create_dir_all(&shard_dir)?;

        // Write blob file
        let blob_path = self.blob_path(&hash);
        let mut file = File::create(&blob_path)?;

        // Write header
        file.write_all(BLOB_MAGIC)?;
        file.write_all(&[BLOB_VERSION])?;

        // Write content type
        let content_type_bytes = content_type.as_bytes();
        let content_type_len = content_type_bytes.len() as u16;
        file.write_all(&content_type_len.to_le_bytes())?;
        file.write_all(content_type_bytes)?;

        // Write content
        let content_len = content.len() as u64;
        file.write_all(&content_len.to_le_bytes())?;
        file.write_all(content)?;

        // Write checksum
        let checksum = crc32fast::hash(content);
        file.write_all(&checksum.to_le_bytes())?;

        file.sync_all()?;

        // Add to cache
        self.cache.lock().put(hash, CachedBlob {
            content: content.to_vec(),
            content_type: content_type.to_string(),
        });

        Ok(hash)
    }

    /// Get a blob by its hash.
    pub fn get(&self, hash: &Hash) -> Result<Option<Blob>> {
        // Check cache first
        if let Some(cached) = self.cache.lock().get(hash).cloned() {
            return Ok(Some(Blob {
                hash: *hash,
                content: cached.content,
                content_type: cached.content_type,
            }));
        }

        let blob_path = self.blob_path(hash);
        if !blob_path.exists() {
            return Ok(None);
        }

        let mut file = File::open(&blob_path)?;

        // Read and verify magic
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != BLOB_MAGIC {
            return Err(StoreError::InvalidFormat("Invalid blob magic".into()));
        }

        // Read version
        let mut version = [0u8; 1];
        file.read_exact(&mut version)?;
        if version[0] != BLOB_VERSION {
            return Err(StoreError::InvalidFormat(format!(
                "Unsupported blob version: {}",
                version[0]
            )));
        }

        // Read content type
        let mut content_type_len_bytes = [0u8; 2];
        file.read_exact(&mut content_type_len_bytes)?;
        let content_type_len = u16::from_le_bytes(content_type_len_bytes) as usize;

        let mut content_type_bytes = vec![0u8; content_type_len];
        file.read_exact(&mut content_type_bytes)?;
        let content_type = String::from_utf8_lossy(&content_type_bytes).into_owned();

        // Read content
        let mut content_len_bytes = [0u8; 8];
        file.read_exact(&mut content_len_bytes)?;
        let content_len_raw = u64::from_le_bytes(content_len_bytes);
        if content_len_raw > MAX_BLOB_SIZE {
            return Err(StoreError::PayloadTooLarge {
                size: content_len_raw,
                limit: MAX_BLOB_SIZE,
            });
        }
        let content_len = content_len_raw as usize;

        let mut content = vec![0u8; content_len];
        file.read_exact(&mut content)?;

        // Read and verify checksum
        let mut checksum_bytes = [0u8; 4];
        file.read_exact(&mut checksum_bytes)?;
        let stored_checksum = u32::from_le_bytes(checksum_bytes);
        let computed_checksum = crc32fast::hash(&content);

        if stored_checksum != computed_checksum {
            return Err(StoreError::ChecksumMismatch {
                expected: stored_checksum,
                got: computed_checksum,
            });
        }

        // Verify hash
        let computed_hash = Hash::from_bytes(&content);
        if &computed_hash != hash {
            return Err(StoreError::HashMismatch {
                expected: *hash,
                got: computed_hash,
            });
        }

        // Add to cache
        self.cache.lock().put(*hash, CachedBlob {
            content: content.clone(),
            content_type: content_type.clone(),
        });

        Ok(Some(Blob {
            hash: *hash,
            content,
            content_type,
        }))
    }

    /// Check if a blob exists.
    pub fn exists(&self, hash: &Hash) -> bool {
        if self.cache.lock().contains(hash) {
            return true;
        }
        self.blob_path(hash).exists()
    }

    /// Delete a blob (for garbage collection).
    pub fn delete(&self, hash: &Hash) -> Result<bool> {
        self.cache.lock().pop(hash);

        let blob_path = self.blob_path(hash);
        if blob_path.exists() {
            fs::remove_file(&blob_path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all blob hashes.
    pub fn list(&self) -> Result<Vec<Hash>> {
        let mut hashes = Vec::new();

        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                for blob_entry in fs::read_dir(entry.path())? {
                    let blob_entry = blob_entry?;
                    let filename = blob_entry.file_name();
                    let filename_str = filename.to_string_lossy();
                    if let Ok(hash) = Hash::from_hex(&filename_str) {
                        hashes.push(hash);
                    }
                }
            }
        }

        Ok(hashes)
    }

    /// Get total size of all blobs.
    pub fn total_size(&self) -> Result<u64> {
        let mut total = 0u64;

        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                for blob_entry in fs::read_dir(entry.path())? {
                    let blob_entry = blob_entry?;
                    total += blob_entry.metadata()?.len();
                }
            }
        }

        Ok(total)
    }

    /// Get the shard directory for a hash.
    fn shard_path(&self, hash: &Hash) -> PathBuf {
        self.path.join(hash.shard_prefix())
    }

    /// Get the full path for a blob.
    fn blob_path(&self, hash: &Hash) -> PathBuf {
        self.shard_path(hash).join(hash.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_store_and_retrieve() {
        let dir = TempDir::new().unwrap();
        let storage = BlobStorage::new(dir.path().join("blobs"), 100).unwrap();

        let content = b"Hello, world!";
        let hash = storage.store(content, "text/plain").unwrap();

        let blob = storage.get(&hash).unwrap().unwrap();
        assert_eq!(blob.content, content);
        assert_eq!(blob.content_type, "text/plain");
    }

    #[test]
    fn test_deduplication() {
        let dir = TempDir::new().unwrap();
        let storage = BlobStorage::new(dir.path().join("blobs"), 100).unwrap();

        let content = b"Same content";
        let hash1 = storage.store(content, "text/plain").unwrap();
        let hash2 = storage.store(content, "text/plain").unwrap();

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_exists() {
        let dir = TempDir::new().unwrap();
        let storage = BlobStorage::new(dir.path().join("blobs"), 100).unwrap();

        let content = b"Test content";
        let hash = storage.store(content, "text/plain").unwrap();

        assert!(storage.exists(&hash));

        let other_hash = Hash::from_bytes(b"nonexistent");
        assert!(!storage.exists(&other_hash));
    }

    #[test]
    fn test_delete() {
        let dir = TempDir::new().unwrap();
        let storage = BlobStorage::new(dir.path().join("blobs"), 100).unwrap();

        let content = b"To be deleted";
        let hash = storage.store(content, "text/plain").unwrap();

        assert!(storage.exists(&hash));
        assert!(storage.delete(&hash).unwrap());
        assert!(!storage.exists(&hash));
    }

    #[test]
    fn test_list() {
        let dir = TempDir::new().unwrap();
        let storage = BlobStorage::new(dir.path().join("blobs"), 100).unwrap();

        let hash1 = storage.store(b"content1", "text/plain").unwrap();
        let hash2 = storage.store(b"content2", "text/plain").unwrap();
        let hash3 = storage.store(b"content3", "text/plain").unwrap();

        let hashes = storage.list().unwrap();
        assert_eq!(hashes.len(), 3);
        assert!(hashes.contains(&hash1));
        assert!(hashes.contains(&hash2));
        assert!(hashes.contains(&hash3));
    }
}
