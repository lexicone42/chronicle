//! Tests for corrupted store file handling.
//!
//! These tests verify that the store handles corrupted on-disk files
//! gracefully (clean errors, no panics, no OOM) — the same class of
//! bugs that fuzzing finds, but as deterministic regression tests.

use chronicle::{RecordLog, Store, StoreConfig};
use std::io::Write;
use tempfile::TempDir;

fn store_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("store")
}

/// Helper to create a valid store, close it, then corrupt a specific file.
fn create_then_corrupt(dir: &TempDir, filename: &str, content: &[u8]) {
    // Create a valid store first (so the directory structure exists)
    {
        let _store = Store::create(StoreConfig {
            path: store_path(dir),
            blob_cache_size: 10,
            create_if_missing: true,
        })
        .unwrap();
    }

    // Overwrite the target file with corrupt data
    let path = store_path(dir).join(filename);
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(content).unwrap();
    file.sync_all().unwrap();
}

// ===========================================================================
// Record log: corrupted files should error, not panic/OOM
// ===========================================================================

#[test]
fn test_corrupted_record_log_invalid_magic() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bad.log");

    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(b"BADMAGIC").unwrap();
    file.sync_all().unwrap();

    // Should open (find_max_id sees bad magic, returns 0, starts fresh)
    let log = RecordLog::open(&path).unwrap();
    // But reading should fail
    let result = log.read_at(0);
    assert!(result.is_err());
}

#[test]
fn test_corrupted_record_log_max_id() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("maxid.log");

    // Valid magic + version + flags, then ID = u64::MAX, rest zeros
    let mut data = vec![0u8; 95];
    data[0..4].copy_from_slice(b"REC\0"); // magic
    data[4] = 2; // version (LOG_VERSION)
    data[5] = 0; // flags
    // ID at offset 6 = u64::MAX
    data[6..14].copy_from_slice(&u64::MAX.to_le_bytes());
    // rest is zeros (invalid but shouldn't crash)

    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(&data).unwrap();
    file.sync_all().unwrap();

    // Should not panic (saturating_add prevents overflow)
    let result = RecordLog::open(&path);
    assert!(result.is_ok(), "Opening log with max ID should not panic");
}

#[test]
fn test_corrupted_record_log_huge_payload_length() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("huge.log");

    // Build a record with valid header but payload_len = 0xFFFFFFFF
    let mut data = Vec::new();
    data.extend_from_slice(b"REC\0"); // magic
    data.push(2); // version
    data.push(0); // flags
    data.extend_from_slice(&1u64.to_le_bytes()); // ID
    data.extend_from_slice(&1u64.to_le_bytes()); // sequence
    data.extend_from_slice(&1u64.to_le_bytes()); // branch
    data.extend_from_slice(&0i64.to_le_bytes()); // timestamp
    data.extend_from_slice(&0u16.to_le_bytes()); // type_len = 0
    data.push(0); // encoding
    data.extend_from_slice(&u32::MAX.to_le_bytes()); // payload_len = 4GB
    // No actual payload data follows

    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(&data).unwrap();
    file.sync_all().unwrap();

    // Should open without OOM (find_max_id breaks on oversized payload)
    let result = RecordLog::open(&path);
    assert!(result.is_ok(), "Opening log with huge payload length should not OOM");

    // Reading the record should fail with PayloadTooLarge
    let log = result.unwrap();
    let read_result = log.read_at(0);
    assert!(read_result.is_err(), "Reading oversized record should error");
}

#[test]
fn test_corrupted_record_log_wrong_version() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("badver.log");

    let mut data = Vec::new();
    data.extend_from_slice(b"REC\0"); // magic
    data.push(99); // invalid version
    data.push(0); // flags
    data.extend_from_slice(&1u64.to_le_bytes()); // ID

    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(&data).unwrap();
    file.sync_all().unwrap();

    // find_max_id should break on wrong version, return 0
    let result = RecordLog::open(&path);
    assert!(result.is_ok());

    // Reading should fail with invalid format
    let log = result.unwrap();
    let read_result = log.read_at(0);
    assert!(read_result.is_err());
}

// ===========================================================================
// Store open with corrupted metadata files
// ===========================================================================

#[test]
fn test_corrupted_branch_index_huge_length() {
    let dir = TempDir::new().unwrap();

    // Write magic + version + current_branch_id + length = u64::MAX
    let mut data = Vec::new();
    data.extend_from_slice(b"BRI\0"); // magic
    data.push(1); // version
    data.extend_from_slice(&1u64.to_le_bytes()); // current branch ID
    data.extend_from_slice(&u64::MAX.to_le_bytes()); // length = massive

    create_then_corrupt(&dir, "branches.bin", &data);

    // Should error, not OOM
    let result = Store::open(StoreConfig {
        path: store_path(&dir),
        blob_cache_size: 10,
        create_if_missing: false,
    });
    assert!(result.is_err(), "Opening store with corrupted branches.bin should error, not OOM");
}

#[test]
fn test_corrupted_state_index_huge_length() {
    let dir = TempDir::new().unwrap();

    // Write magic + version + length = u64::MAX
    let mut data = Vec::new();
    data.extend_from_slice(b"STI\0"); // magic
    data.push(2); // version
    data.extend_from_slice(&u64::MAX.to_le_bytes()); // length = massive

    create_then_corrupt(&dir, "state.bin", &data);

    // Should error, not OOM
    let result = Store::open(StoreConfig {
        path: store_path(&dir),
        blob_cache_size: 10,
        create_if_missing: false,
    });
    assert!(result.is_err(), "Opening store with corrupted state.bin should error, not OOM");
}

#[test]
fn test_corrupted_manifest_wrong_version() {
    let dir = TempDir::new().unwrap();

    create_then_corrupt(&dir, "MANIFEST", b"RST\0\xFF");

    let result = Store::open(StoreConfig {
        path: store_path(&dir),
        blob_cache_size: 10,
        create_if_missing: false,
    });
    assert!(result.is_err(), "Wrong manifest version should error");
}

#[test]
fn test_corrupted_manifest_bad_magic() {
    let dir = TempDir::new().unwrap();

    create_then_corrupt(&dir, "MANIFEST", b"BAD!");

    let result = Store::open(StoreConfig {
        path: store_path(&dir),
        blob_cache_size: 10,
        create_if_missing: false,
    });
    assert!(result.is_err(), "Bad manifest magic should error");
}

// ===========================================================================
// Truncated files
// ===========================================================================

#[test]
fn test_truncated_record_log() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("trunc.log");

    // Just magic + version, then truncated (no flags byte, no ID)
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(b"REC\0\x02").unwrap();
    file.sync_all().unwrap();

    // find_max_id sees valid magic+version, tries to read flags/ID, gets EOF.
    // This is OK — open() either succeeds with max_id=0, or propagates the IO error.
    match RecordLog::open(&path) {
        Ok(log) => {
            // Opened fine (scan broke early). Reading should still fail.
            let result = log.read_at(0);
            assert!(result.is_err(), "Reading truncated record should error");
        }
        Err(_) => {
            // Also acceptable — truncated file caused IO error during scan
        }
    }
}

#[test]
fn test_empty_record_log() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty.log");

    std::fs::File::create(&path).unwrap();

    let log = RecordLog::open(&path).unwrap();
    let items: Vec<_> = log.iter().collect();
    assert_eq!(items.len(), 0, "Empty log should iterate with no items");
}
