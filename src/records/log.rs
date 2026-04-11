//! Append-only record log.
//!
//! The wire format is defined in `crate::format::record_log`. This file
//! implements the reader/writer against those constants.

use crate::error::{Result, StoreError};
use crate::format::record_log::{MAGIC as LOG_MAGIC, MAX_PAYLOAD_SIZE, VERSION as LOG_VERSION};
use crate::types::{BranchId, PayloadEncoding, Record, RecordId, RecordInput, Sequence, Timestamp};
use parking_lot::RwLock;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Persistent anchor cached in `records.meta` that lets `RecordLog::open`
/// avoid a full O(N) scan when the previous shutdown was clean.
#[derive(Clone, Copy, Debug)]
struct RecordMeta {
    /// Highest record ID that was written to the log.
    max_id: u64,
    /// Record log file size at the time the meta was written.
    log_size: u64,
}

/// Append-only record log.
pub struct RecordLog {
    /// Path to the log file.
    path: PathBuf,

    /// Log file handle.
    file: RwLock<File>,

    /// Next record ID to assign.
    next_id: RwLock<u64>,

    /// Current file size (for appending).
    file_size: RwLock<u64>,

    /// Number of writes since last sync.
    writes_since_sync: RwLock<u64>,

    /// Sync every N writes (0 = sync every write, critical for durability vs performance)
    sync_interval: u64,
}

impl RecordLog {
    /// Open or create a record log with default sync interval.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_sync_interval(path, crate::format::record_log::DEFAULT_SYNC_INTERVAL)
    }

    /// Open or create a record log with custom sync interval.
    /// - sync_interval = 0: sync every write (safest, slowest)
    /// - sync_interval = 1: sync every write
    /// - sync_interval = 100: sync every 100 writes (good balance)
    /// - sync_interval = 1000: sync every 1000 writes (fastest, least durable)
    pub fn open_with_sync_interval(path: impl AsRef<Path>, sync_interval: u64) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let metadata = file.metadata()?;
        let file_size = metadata.len();

        // Try to read records.meta anchor first. If it's present and the
        // recorded file size matches the actual log size, we can skip the
        // O(N) find_max_id scan.
        let meta_path = Self::meta_path(&path);
        let next_id = if file_size == 0 {
            1
        } else {
            match Self::read_meta(&meta_path) {
                Ok(Some(meta)) if meta.log_size == file_size => {
                    // Clean shutdown: trust the cached max_id
                    meta.max_id.saturating_add(1)
                }
                _ => {
                    // No meta, or stale meta, or corrupt meta —
                    // fall back to full scan
                    Self::find_max_id(&file)?.saturating_add(1)
                }
            }
        };

        Ok(Self {
            path,
            file: RwLock::new(file),
            next_id: RwLock::new(next_id),
            file_size: RwLock::new(file_size),
            writes_since_sync: RwLock::new(0),
            sync_interval: if sync_interval == 0 { 1 } else { sync_interval },
        })
    }

    /// Path to the records.meta file next to the record log.
    fn meta_path(log_path: &Path) -> PathBuf {
        let mut meta = log_path.to_path_buf();
        meta.set_extension("meta");
        meta
    }

    /// Read and validate the records.meta anchor file.
    ///
    /// Returns `Ok(Some(meta))` if the file exists and is valid, `Ok(None)`
    /// if it doesn't exist, and `Err` if it exists but is corrupted.
    fn read_meta(meta_path: &Path) -> Result<Option<RecordMeta>> {
        use crate::format::records_meta::{MAGIC, VERSION};

        if !meta_path.exists() {
            return Ok(None);
        }

        let mut file = File::open(meta_path)?;
        let mut buf = [0u8; 4 + 1 + 8 + 8 + 4]; // magic + version + max_id + log_size + crc32
        if file.read_exact(&mut buf).is_err() {
            // Truncated file — treat as missing, fall back to scan
            return Ok(None);
        }

        if &buf[0..4] != MAGIC {
            return Ok(None); // Not ours, ignore
        }
        if buf[4] != VERSION {
            return Ok(None); // Future version we don't understand
        }

        let max_id = u64::from_le_bytes(buf[5..13].try_into().unwrap());
        let log_size = u64::from_le_bytes(buf[13..21].try_into().unwrap());
        let stored_crc = u32::from_le_bytes(buf[21..25].try_into().unwrap());

        // CRC32 over magic + version + max_id + log_size
        let computed_crc = crc32fast::hash(&buf[0..21]);
        if stored_crc != computed_crc {
            // Corrupt meta — fall back to scan
            return Ok(None);
        }

        Ok(Some(RecordMeta { max_id, log_size }))
    }

    /// Write the records.meta anchor file. Called during `sync()` so that
    /// a clean shutdown leaves a valid anchor that the next open can use.
    fn write_meta(&self) -> Result<()> {
        use crate::format::records_meta::{MAGIC, VERSION};

        let meta_path = Self::meta_path(&self.path);
        let max_id = self.next_id.read().saturating_sub(1);
        let log_size = *self.file_size.read();

        let mut buf = Vec::with_capacity(25);
        buf.extend_from_slice(MAGIC);
        buf.push(VERSION);
        buf.extend_from_slice(&max_id.to_le_bytes());
        buf.extend_from_slice(&log_size.to_le_bytes());
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        // Write atomically via tmp + rename to avoid partial writes
        let tmp_path = meta_path.with_extension("meta.tmp");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;
            file.write_all(&buf)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp_path, &meta_path)?;
        Ok(())
    }

    /// Append a record to the log.
    ///
    /// Returns the record and the offset where it was written.
    pub fn append(
        &self,
        input: RecordInput,
        branch: BranchId,
        sequence: Sequence,
    ) -> Result<(Record, u64)> {
        let mut file = self.file.write();

        // Assign ID
        let id = RecordId(*self.next_id.read());
        *self.next_id.write() += 1;

        let timestamp = Timestamp::now();

        // Build record
        let record = Record {
            id,
            sequence,
            branch,
            timestamp,
            record_type: input.record_type,
            payload: input.payload,
            encoding: input.encoding,
            caused_by: input.caused_by,
            linked_to: input.linked_to,
        };

        // Serialize and write
        let offset = *self.file_size.read();
        file.seek(SeekFrom::Start(offset))?;

        self.write_record(&mut *file, &record)?;

        let new_size = file.stream_position()?;
        *self.file_size.write() = new_size;

        // Sync periodically based on sync_interval
        let mut writes = self.writes_since_sync.write();
        *writes += 1;
        if *writes >= self.sync_interval {
            file.sync_all()?;
            *writes = 0;
        }

        Ok((record, offset))
    }

    /// Force sync all pending writes to disk.
    pub fn sync(&self) -> Result<()> {
        {
            let file = self.file.write();
            file.sync_all()?;
            *self.writes_since_sync.write() = 0;
        }
        // Persist the next_id anchor so the next open can skip find_max_id
        self.write_meta()?;
        Ok(())
    }

    /// Read a record at a given offset.
    pub fn read_at(&self, offset: u64) -> Result<Record> {
        let mut file = self.file.write();
        file.seek(SeekFrom::Start(offset))?;
        self.read_record(&mut *file)
    }

    /// Read a record at a given offset, returning both the record and the
    /// file position immediately after it (i.e. the offset of the next record).
    ///
    /// This lets iterators avoid a second read to locate the next record.
    pub fn read_at_with_end(&self, offset: u64) -> Result<(Record, u64)> {
        let mut file = self.file.write();
        file.seek(SeekFrom::Start(offset))?;
        let record = self.read_record(&mut *file)?;
        let end = file.stream_position()?;
        Ok((record, end))
    }

    /// Iterate all records from the beginning.
    pub fn iter(&self) -> RecordIterator {
        self.iter_from(0)
    }

    /// Iterate all records from a given offset.
    pub fn iter_from(&self, offset: u64) -> RecordIterator {
        RecordIterator {
            log: self,
            offset,
            end: *self.file_size.read(),
        }
    }

    /// Get current file size.
    pub fn size(&self) -> u64 {
        *self.file_size.read()
    }

    /// Write a record to the file.
    fn write_record(&self, file: &mut File, record: &Record) -> Result<()> {
        // Magic
        file.write_all(LOG_MAGIC)?;

        // Version
        file.write_all(&[LOG_VERSION])?;

        // Flags (reserved)
        file.write_all(&[0u8])?;

        // Record ID
        file.write_all(&record.id.0.to_le_bytes())?;

        // Sequence
        file.write_all(&record.sequence.0.to_le_bytes())?;

        // Branch
        file.write_all(&record.branch.0.to_le_bytes())?;

        // Timestamp
        file.write_all(&record.timestamp.0.to_le_bytes())?;

        // Type
        let type_bytes = record.record_type.as_bytes();
        file.write_all(&(type_bytes.len() as u16).to_le_bytes())?;
        file.write_all(type_bytes)?;

        // Encoding
        let encoding_byte = match record.encoding {
            PayloadEncoding::Json => 0u8,
            PayloadEncoding::MessagePack => 1u8,
            PayloadEncoding::Raw => 2u8,
        };
        file.write_all(&[encoding_byte])?;

        // Payload
        file.write_all(&(record.payload.len() as u32).to_le_bytes())?;
        file.write_all(&record.payload)?;

        // Caused by
        file.write_all(&(record.caused_by.len() as u16).to_le_bytes())?;
        for id in &record.caused_by {
            file.write_all(&id.0.to_le_bytes())?;
        }

        // Linked to
        file.write_all(&(record.linked_to.len() as u16).to_le_bytes())?;
        for id in &record.linked_to {
            file.write_all(&id.0.to_le_bytes())?;
        }

        // Checksum covers all record fields (not just payload) to detect
        // metadata corruption (sequence, branch, timestamps, causal links).
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&record.id.0.to_le_bytes());
        hasher.update(&record.sequence.0.to_le_bytes());
        hasher.update(&record.branch.0.to_le_bytes());
        hasher.update(&record.timestamp.0.to_le_bytes());
        hasher.update(record.record_type.as_bytes());
        hasher.update(&[encoding_byte]);
        hasher.update(&record.payload);
        for id in &record.caused_by {
            hasher.update(&id.0.to_le_bytes());
        }
        for id in &record.linked_to {
            hasher.update(&id.0.to_le_bytes());
        }
        let checksum = hasher.finalize();
        file.write_all(&checksum.to_le_bytes())?;

        Ok(())
    }

    /// Read a record from the file at current position.
    fn read_record(&self, file: &mut File) -> Result<Record> {
        // Magic
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != LOG_MAGIC {
            return Err(StoreError::InvalidFormat("Invalid record magic".into()));
        }

        // Version
        let mut version = [0u8; 1];
        file.read_exact(&mut version)?;
        if version[0] != LOG_VERSION {
            return Err(StoreError::InvalidFormat(format!(
                "Unsupported log version: {}",
                version[0]
            )));
        }

        // Flags
        let mut _flags = [0u8; 1];
        file.read_exact(&mut _flags)?;

        // Record ID
        let mut id_bytes = [0u8; 8];
        file.read_exact(&mut id_bytes)?;
        let id = RecordId(u64::from_le_bytes(id_bytes));

        // Sequence
        let mut seq_bytes = [0u8; 8];
        file.read_exact(&mut seq_bytes)?;
        let sequence = Sequence(u64::from_le_bytes(seq_bytes));

        // Branch
        let mut branch_bytes = [0u8; 8];
        file.read_exact(&mut branch_bytes)?;
        let branch = BranchId(u64::from_le_bytes(branch_bytes));

        // Timestamp
        let mut ts_bytes = [0u8; 8];
        file.read_exact(&mut ts_bytes)?;
        let timestamp = Timestamp(i64::from_le_bytes(ts_bytes));

        // Type
        let mut type_len_bytes = [0u8; 2];
        file.read_exact(&mut type_len_bytes)?;
        let type_len = u16::from_le_bytes(type_len_bytes) as usize;
        let mut type_bytes = vec![0u8; type_len];
        file.read_exact(&mut type_bytes)?;
        let record_type = String::from_utf8_lossy(&type_bytes).into_owned();

        // Encoding
        let mut encoding_byte = [0u8; 1];
        file.read_exact(&mut encoding_byte)?;
        let encoding = match encoding_byte[0] {
            0 => PayloadEncoding::Json,
            1 => PayloadEncoding::MessagePack,
            2 => PayloadEncoding::Raw,
            _ => PayloadEncoding::Raw,
        };

        // Payload
        let mut payload_len_bytes = [0u8; 4];
        file.read_exact(&mut payload_len_bytes)?;
        let payload_len_raw = u32::from_le_bytes(payload_len_bytes);
        if payload_len_raw > MAX_PAYLOAD_SIZE {
            return Err(StoreError::PayloadTooLarge {
                size: payload_len_raw as u64,
                limit: MAX_PAYLOAD_SIZE as u64,
            });
        }
        let payload_len = payload_len_raw as usize;
        let mut payload = vec![0u8; payload_len];
        file.read_exact(&mut payload)?;

        // Caused by
        let mut caused_by_count_bytes = [0u8; 2];
        file.read_exact(&mut caused_by_count_bytes)?;
        let caused_by_count = u16::from_le_bytes(caused_by_count_bytes) as usize;
        let mut caused_by = Vec::with_capacity(caused_by_count);
        for _ in 0..caused_by_count {
            let mut id_bytes = [0u8; 8];
            file.read_exact(&mut id_bytes)?;
            caused_by.push(RecordId(u64::from_le_bytes(id_bytes)));
        }

        // Linked to
        let mut linked_to_count_bytes = [0u8; 2];
        file.read_exact(&mut linked_to_count_bytes)?;
        let linked_to_count = u16::from_le_bytes(linked_to_count_bytes) as usize;
        let mut linked_to = Vec::with_capacity(linked_to_count);
        for _ in 0..linked_to_count {
            let mut id_bytes = [0u8; 8];
            file.read_exact(&mut id_bytes)?;
            linked_to.push(RecordId(u64::from_le_bytes(id_bytes)));
        }

        // Checksum — covers all record fields, not just payload
        let mut checksum_bytes = [0u8; 4];
        file.read_exact(&mut checksum_bytes)?;
        let stored_checksum = u32::from_le_bytes(checksum_bytes);

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&id_bytes);
        hasher.update(&seq_bytes);
        hasher.update(&branch_bytes);
        hasher.update(&ts_bytes);
        hasher.update(&type_bytes);
        hasher.update(&encoding_byte);
        hasher.update(&payload);
        for cb in &caused_by {
            hasher.update(&cb.0.to_le_bytes());
        }
        for lt in &linked_to {
            hasher.update(&lt.0.to_le_bytes());
        }
        let computed_checksum = hasher.finalize();

        if stored_checksum != computed_checksum {
            return Err(StoreError::ChecksumMismatch {
                expected: stored_checksum,
                got: computed_checksum,
            });
        }

        Ok(Record {
            id,
            sequence,
            branch,
            timestamp,
            record_type,
            payload,
            encoding,
            caused_by,
            linked_to,
        })
    }

    /// Find the maximum record ID in the log.
    fn find_max_id(file: &File) -> Result<u64> {
        let mut file = file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;

        let mut max_id = 0u64;
        let file_size = file.metadata()?.len();

        while file.stream_position()? < file_size {
            // Read magic
            let mut magic = [0u8; 4];
            if file.read_exact(&mut magic).is_err() {
                break;
            }

            if &magic != LOG_MAGIC {
                break;
            }

            // Verify version
            let mut version = [0u8; 1];
            file.read_exact(&mut version)?;
            if version[0] != LOG_VERSION {
                break; // Unknown version — stop scanning
            }

            // Skip flags
            file.seek(SeekFrom::Current(1))?;

            // Read ID
            let mut id_bytes = [0u8; 8];
            file.read_exact(&mut id_bytes)?;
            let id = u64::from_le_bytes(id_bytes);
            max_id = max_id.max(id);

            // Skip to next record - we need to read lengths to know how far to skip
            // Skip: sequence(8) + branch(8) + timestamp(8)
            file.seek(SeekFrom::Current(24))?;

            // Read type length and skip type
            let mut type_len_bytes = [0u8; 2];
            file.read_exact(&mut type_len_bytes)?;
            let type_len = u16::from_le_bytes(type_len_bytes) as i64;
            file.seek(SeekFrom::Current(type_len))?;

            // Skip encoding
            file.seek(SeekFrom::Current(1))?;

            // Read payload length and skip payload (with size guard)
            let mut payload_len_bytes = [0u8; 4];
            file.read_exact(&mut payload_len_bytes)?;
            let payload_len = u32::from_le_bytes(payload_len_bytes);
            if payload_len > MAX_PAYLOAD_SIZE {
                break; // Corrupted record — stop scanning
            }
            file.seek(SeekFrom::Current(payload_len as i64))?;

            // Read caused_by count and skip
            let mut caused_by_count_bytes = [0u8; 2];
            file.read_exact(&mut caused_by_count_bytes)?;
            let caused_by_count = u16::from_le_bytes(caused_by_count_bytes) as i64;
            file.seek(SeekFrom::Current(caused_by_count * 8))?;

            // Read linked_to count and skip
            let mut linked_to_count_bytes = [0u8; 2];
            file.read_exact(&mut linked_to_count_bytes)?;
            let linked_to_count = u16::from_le_bytes(linked_to_count_bytes) as i64;
            file.seek(SeekFrom::Current(linked_to_count * 8))?;

            // Skip checksum
            file.seek(SeekFrom::Current(4))?;
        }

        Ok(max_id)
    }
}

/// Iterator over records in the log.
pub struct RecordIterator<'a> {
    log: &'a RecordLog,
    offset: u64,
    end: u64,
}

impl<'a> Iterator for RecordIterator<'a> {
    type Item = Result<(u64, Record)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.end {
            return None;
        }

        let current_offset = self.offset;
        match self.log.read_at_with_end(current_offset) {
            Ok((record, next_offset)) => {
                self.offset = next_offset;
                Some(Ok((current_offset, record)))
            }
            Err(e) => {
                self.offset = self.end; // Stop iteration on error
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_append_and_read() {
        let dir = TempDir::new().unwrap();
        let log = RecordLog::open(dir.path().join("log.bin")).unwrap();

        let input = RecordInput::raw("test", b"hello".to_vec());
        let (record, offset) = log.append(input, BranchId(1), Sequence(1)).unwrap();

        assert_eq!(record.id.0, 1);
        assert_eq!(record.sequence.0, 1);
        assert_eq!(record.payload, b"hello");
        assert_eq!(offset, 0);
    }

    #[test]
    fn test_multiple_records() {
        let dir = TempDir::new().unwrap();
        let log = RecordLog::open(dir.path().join("log.bin")).unwrap();

        for i in 1..=10 {
            let input = RecordInput::raw("test", format!("record {}", i).into_bytes());
            let (record, _offset) = log.append(input, BranchId(1), Sequence(i)).unwrap();
            assert_eq!(record.id.0, i);
        }

        // Iterate and verify
        let records: Vec<_> = log.iter_from(0).collect();
        assert_eq!(records.len(), 10);
    }

    #[test]
    fn test_persistence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.bin");

        // Write some records
        {
            let log = RecordLog::open(&path).unwrap();
            for i in 1..=5 {
                let input = RecordInput::raw("test", format!("record {}", i).into_bytes());
                log.append(input, BranchId(1), Sequence(i)).unwrap();
            }
        }

        // Reopen and verify
        {
            let log = RecordLog::open(&path).unwrap();
            let records: Vec<_> = log.iter_from(0).collect();
            assert_eq!(records.len(), 5);

            // Append more
            let input = RecordInput::raw("test", b"record 6".to_vec());
            let (record, _offset) = log.append(input, BranchId(1), Sequence(6)).unwrap();
            assert_eq!(record.id.0, 6); // Should continue from max ID
        }
    }

    #[test]
    fn test_records_meta_written_on_sync() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.bin");
        let meta_path = dir.path().join("log.meta");

        {
            let log = RecordLog::open(&path).unwrap();
            for i in 1..=3 {
                log.append(
                    RecordInput::raw("test", format!("r{}", i).into_bytes()),
                    BranchId(1),
                    Sequence(i),
                )
                .unwrap();
            }
            log.sync().unwrap();
        }

        // After sync, the meta file should exist
        assert!(meta_path.exists(), "records.meta should exist after sync");

        // Re-opening should use the cached next_id without scanning
        // (we verify by checking that the next append gets ID 4)
        let log = RecordLog::open(&path).unwrap();
        let (record, _) = log
            .append(RecordInput::raw("test", b"r4".to_vec()), BranchId(1), Sequence(4))
            .unwrap();
        assert_eq!(record.id.0, 4);
    }

    #[test]
    fn test_records_meta_stale_falls_back_to_scan() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.bin");
        let meta_path = dir.path().join("log.meta");

        // Write records and sync, so meta is created
        {
            let log = RecordLog::open(&path).unwrap();
            for i in 1..=3 {
                log.append(
                    RecordInput::raw("test", format!("r{}", i).into_bytes()),
                    BranchId(1),
                    Sequence(i),
                )
                .unwrap();
            }
            log.sync().unwrap();
        }

        // Write one more record WITHOUT syncing — this makes the
        // meta's log_size stale
        {
            let log = RecordLog::open(&path).unwrap();
            log.append(
                RecordInput::raw("test", b"r4".to_vec()),
                BranchId(1),
                Sequence(4),
            )
            .unwrap();
            // Intentionally no sync — meta is now stale
            std::mem::forget(log); // keep file open; drop-triggered sync happens nowhere
        }

        // Wait — the meta file still exists and doesn't match
        assert!(meta_path.exists());

        // Reopening should detect the mismatch and fall back to scan,
        // correctly finding that max_id is now 4
        let log = RecordLog::open(&path).unwrap();
        let (record, _) = log
            .append(RecordInput::raw("test", b"r5".to_vec()), BranchId(1), Sequence(5))
            .unwrap();
        assert_eq!(record.id.0, 5); // Would be 4 if we'd trusted stale meta
    }

    #[test]
    fn test_records_meta_corrupt_falls_back_to_scan() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.bin");
        let meta_path = dir.path().join("log.meta");

        // Write records and sync
        {
            let log = RecordLog::open(&path).unwrap();
            for i in 1..=3 {
                log.append(
                    RecordInput::raw("test", format!("r{}", i).into_bytes()),
                    BranchId(1),
                    Sequence(i),
                )
                .unwrap();
            }
            log.sync().unwrap();
        }

        // Corrupt the meta file by overwriting its CRC bytes
        let mut meta_content = std::fs::read(&meta_path).unwrap();
        let len = meta_content.len();
        meta_content[len - 4] ^= 0xFF;
        meta_content[len - 1] ^= 0xFF;
        std::fs::write(&meta_path, &meta_content).unwrap();

        // Should still open correctly (fall back to scan)
        let log = RecordLog::open(&path).unwrap();
        let (record, _) = log
            .append(RecordInput::raw("test", b"r4".to_vec()), BranchId(1), Sequence(4))
            .unwrap();
        assert_eq!(record.id.0, 4);
    }
}
