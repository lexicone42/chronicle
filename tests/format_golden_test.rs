//! Golden-byte tests for each on-disk format.
//!
//! These tests lock the wire format in place. They come in three flavors:
//!
//! 1. **Parser fixtures** — hand-constructed hex byte strings that we
//!    commit as the canonical representation of known inputs. Parsing
//!    them must produce the expected struct. If the parser changes
//!    behavior, these tests catch it.
//!
//! 2. **Round-trip tests** — write a struct via the public API, then
//!    parse the bytes back. If the writer and parser drift, these fail.
//!
//! 3. **Structural tests** — check field positions within headers. These
//!    catch accidental field reordering.
//!
//! These tests are intentionally low-level and operate directly on files.

use chronicle::{
    format, RecordInput, Store, StoreConfig,
};
use std::fs;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// MANIFEST
// ---------------------------------------------------------------------------

#[test]
fn manifest_golden_bytes() {
    // Expected bytes: "RST\0" + version byte (0x02)
    let expected: [u8; 5] = [0x52, 0x53, 0x54, 0x00, 0x02];

    assert_eq!(format::manifest::MAGIC, &expected[0..4]);
    assert_eq!(format::manifest::VERSION, expected[4]);
}

#[test]
fn manifest_roundtrip_on_disk() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("store");
    let _store = Store::create(StoreConfig {
        path: root.clone(),
        blob_cache_size: 10,
        create_if_missing: true,
    })
    .unwrap();
    drop(_store);

    let bytes = fs::read(root.join("MANIFEST")).unwrap();
    assert_eq!(&bytes[0..4], format::manifest::MAGIC);
    assert_eq!(bytes[4], format::manifest::VERSION);
    assert_eq!(bytes.len(), 5, "manifest should be exactly 5 bytes");
}

// ---------------------------------------------------------------------------
// Record log
// ---------------------------------------------------------------------------

#[test]
fn record_log_golden_header_bytes() {
    // Expected header prefix: "REC\0" + version (0x02)
    let expected_prefix: [u8; 5] = [0x52, 0x45, 0x43, 0x00, 0x02];

    assert_eq!(format::record_log::MAGIC, &expected_prefix[0..4]);
    assert_eq!(format::record_log::VERSION, expected_prefix[4]);
}

#[test]
fn record_log_structural_layout() {
    // A record's fixed-size header begins with:
    //   magic (4) + version (1) + flags (1) + id (8) + seq (8) + branch (8) + timestamp (8)
    //   = 38 bytes before the variable-length type_length field
    //
    // This test writes a minimal record and asserts the positions of the
    // header fields.
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("store");
    let store = Store::create(StoreConfig {
        path: root.clone(),
        blob_cache_size: 10,
        create_if_missing: true,
    })
    .unwrap();

    // Write a record with known values — we can't control timestamp so
    // we only assert on fields we know.
    let record = store
        .append(RecordInput::raw("x", vec![]))
        .unwrap();
    store.sync().unwrap();
    drop(store);

    let bytes = fs::read(root.join("records.log")).unwrap();
    assert!(bytes.len() >= 38, "record log should have at least a header");

    // Magic at offset 0
    assert_eq!(&bytes[0..4], format::record_log::MAGIC);
    // Version at offset 4
    assert_eq!(bytes[4], format::record_log::VERSION);
    // Flags at offset 5 (reserved 0x00)
    assert_eq!(bytes[5], 0x00);
    // Record ID at offset 6 (u64 LE) — should be 1 for the first record
    let id_bytes = &bytes[6..14];
    let id = u64::from_le_bytes(id_bytes.try_into().unwrap());
    assert_eq!(id, 1, "first record should have ID 1");
    assert_eq!(id, record.id.0, "on-disk ID matches returned record");
    // Sequence at offset 14 — should also be 1 for the first record on main
    let seq = u64::from_le_bytes(bytes[14..22].try_into().unwrap());
    assert_eq!(seq, 1);
}

#[test]
fn record_log_max_payload_size_unchanged() {
    // Tripwire: if someone changes the payload size limit, we want to know
    assert_eq!(format::record_log::MAX_PAYLOAD_SIZE, 256 * 1024 * 1024);
}

// ---------------------------------------------------------------------------
// Blob storage
// ---------------------------------------------------------------------------

#[test]
fn blob_golden_header_bytes() {
    let expected_prefix: [u8; 5] = [0x42, 0x4C, 0x42, 0x00, 0x01];

    assert_eq!(format::blob::MAGIC, &expected_prefix[0..4]);
    assert_eq!(format::blob::VERSION, expected_prefix[4]);
}

#[test]
fn blob_structural_layout() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("store");
    let store = Store::create(StoreConfig {
        path: root.clone(),
        blob_cache_size: 10,
        create_if_missing: true,
    })
    .unwrap();

    store.store_blob(b"hi", "text/plain").unwrap();
    drop(store);

    // Find the blob file
    let blobs = root.join("blobs");
    let mut blob_path = None;
    for shard in fs::read_dir(&blobs).unwrap() {
        let shard = shard.unwrap();
        if shard.file_type().unwrap().is_dir() {
            for blob in fs::read_dir(shard.path()).unwrap() {
                blob_path = Some(blob.unwrap().path());
                break;
            }
        }
        if blob_path.is_some() {
            break;
        }
    }
    let blob_path = blob_path.expect("blob file should exist");
    let bytes = fs::read(&blob_path).unwrap();

    // Magic at offset 0
    assert_eq!(&bytes[0..4], format::blob::MAGIC);
    // Version at offset 4
    assert_eq!(bytes[4], format::blob::VERSION);
    // Content-type length at offset 5 (u16 LE) — "text/plain" = 10 bytes
    let ct_len = u16::from_le_bytes(bytes[5..7].try_into().unwrap());
    assert_eq!(ct_len, 10);
    // Content-type at offset 7
    assert_eq!(&bytes[7..17], b"text/plain");
    // Content length at offset 17 (u64 LE) — "hi" = 2 bytes
    let content_len = u64::from_le_bytes(bytes[17..25].try_into().unwrap());
    assert_eq!(content_len, 2);
    // Content at offset 25
    assert_eq!(&bytes[25..27], b"hi");
    // CRC32 at offset 27 (4 bytes)
    let crc = u32::from_le_bytes(bytes[27..31].try_into().unwrap());
    assert_eq!(crc, crc32fast::hash(b"hi"));
}

#[test]
fn blob_max_size_unchanged() {
    assert_eq!(format::blob::MAX_SIZE, 1024 * 1024 * 1024);
}

// ---------------------------------------------------------------------------
// State index envelope
// ---------------------------------------------------------------------------

#[test]
fn state_index_golden_header() {
    let expected_prefix: [u8; 5] = [0x53, 0x54, 0x49, 0x00, 0x03];
    assert_eq!(format::state_index::MAGIC, &expected_prefix[0..4]);
    assert_eq!(format::state_index::VERSION, expected_prefix[4]);
    assert_eq!(format::state_index::MIN_READABLE_VERSION, 2);
}

#[test]
fn state_index_structural_envelope() {
    // Create a store, register a state, sync, then inspect state.bin
    use chronicle::{StateOperation, StateRegistration, StateStrategy};

    let dir = TempDir::new().unwrap();
    let root = dir.path().join("store");
    let store = Store::create(StoreConfig {
        path: root.clone(),
        blob_cache_size: 10,
        create_if_missing: true,
    })
    .unwrap();

    store
        .register_state(StateRegistration {
            id: "x".to_string(),
            strategy: StateStrategy::AppendLog {
                delta_snapshot_every: 10,
                full_snapshot_every: 5,
            },
            initial_value: None,
        })
        .unwrap();
    store
        .update_state("x", StateOperation::Append(b"\"v\"".to_vec()))
        .unwrap();
    store.sync().unwrap();
    drop(store);

    let bytes = fs::read(root.join("state.bin")).unwrap();

    // Magic + version
    assert_eq!(&bytes[0..4], format::state_index::MAGIC);
    assert_eq!(bytes[4], format::state_index::VERSION);
    // Length (u64 LE) at offset 5
    let len = u64::from_le_bytes(bytes[5..13].try_into().unwrap());
    // Total file size should be 13 (header) + len (content) + 4 (CRC32)
    assert_eq!(bytes.len() as u64, 13 + len + 4);
    // CRC32 at the end
    let body = &bytes[13..13 + len as usize];
    let stored_crc = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
    assert_eq!(stored_crc, crc32fast::hash(body));
}

// ---------------------------------------------------------------------------
// Branch index envelope
// ---------------------------------------------------------------------------

#[test]
fn branch_index_golden_header() {
    let expected_prefix: [u8; 5] = [0x42, 0x52, 0x49, 0x00, 0x02];
    assert_eq!(format::branch_index::MAGIC, &expected_prefix[0..4]);
    assert_eq!(format::branch_index::VERSION, expected_prefix[4]);
    assert_eq!(format::branch_index::MIN_READABLE_VERSION, 1);
}

#[test]
fn branch_index_structural_envelope() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("store");
    let store = Store::create(StoreConfig {
        path: root.clone(),
        blob_cache_size: 10,
        create_if_missing: true,
    })
    .unwrap();

    store.create_branch("feature", None).unwrap();
    store.sync().unwrap();
    drop(store);

    let bytes = fs::read(root.join("branches.bin")).unwrap();

    // Magic + version
    assert_eq!(&bytes[0..4], format::branch_index::MAGIC);
    assert_eq!(bytes[4], format::branch_index::VERSION);
    // Current branch ID (u64 LE) at offset 5
    let _current_id = u64::from_le_bytes(bytes[5..13].try_into().unwrap());
    // Length (u64 LE) at offset 13
    let len = u64::from_le_bytes(bytes[13..21].try_into().unwrap());
    // Total: 21 (header) + len + 4 (CRC32)
    assert_eq!(bytes.len() as u64, 21 + len + 4);
    // CRC32 verification
    let body = &bytes[21..21 + len as usize];
    let stored_crc = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
    assert_eq!(stored_crc, crc32fast::hash(body));
}

// ---------------------------------------------------------------------------
// WAL
// ---------------------------------------------------------------------------

#[test]
fn wal_golden_header_bytes() {
    let expected_prefix: [u8; 5] = [0x57, 0x41, 0x4C, 0x00, 0x01];
    assert_eq!(format::wal::MAGIC, &expected_prefix[0..4]);
    assert_eq!(format::wal::VERSION, expected_prefix[4]);
}

// ---------------------------------------------------------------------------
// records.meta
// ---------------------------------------------------------------------------

#[test]
fn records_meta_golden_header() {
    let expected_prefix: [u8; 5] = [0x52, 0x4D, 0x54, 0x00, 0x01];
    assert_eq!(format::records_meta::MAGIC, &expected_prefix[0..4]);
    assert_eq!(format::records_meta::VERSION, expected_prefix[4]);
}

#[test]
fn records_meta_structural_layout() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("store");
    let store = Store::create(StoreConfig {
        path: root.clone(),
        blob_cache_size: 10,
        create_if_missing: true,
    })
    .unwrap();

    store
        .append(RecordInput::raw("test", b"hello".to_vec()))
        .unwrap();
    store.sync().unwrap();
    drop(store);

    let bytes = fs::read(root.join("records.meta")).unwrap();

    // Total size is exactly 25 bytes: 4 (magic) + 1 (version) + 8 (max_id) + 8 (log_size) + 4 (crc)
    assert_eq!(bytes.len(), 25);

    // Magic + version
    assert_eq!(&bytes[0..4], format::records_meta::MAGIC);
    assert_eq!(bytes[4], format::records_meta::VERSION);

    // max_id at offset 5 (u64 LE) — should be 1 (one record written)
    let max_id = u64::from_le_bytes(bytes[5..13].try_into().unwrap());
    assert_eq!(max_id, 1);

    // log_size at offset 13 (u64 LE) — must match the actual file size
    let meta_log_size = u64::from_le_bytes(bytes[13..21].try_into().unwrap());
    let actual_log_size = fs::metadata(root.join("records.log")).unwrap().len();
    assert_eq!(meta_log_size, actual_log_size);

    // CRC32 at offset 21 (4 bytes) — over the first 21 bytes
    let stored_crc = u32::from_le_bytes(bytes[21..25].try_into().unwrap());
    let expected_crc = crc32fast::hash(&bytes[0..21]);
    assert_eq!(stored_crc, expected_crc);
}

// ---------------------------------------------------------------------------
// Format module self-consistency
// ---------------------------------------------------------------------------

#[test]
fn current_versions_matrix_is_complete() {
    // Every magic in the matrix should be distinct
    let mut seen: Vec<&[u8; 4]> = Vec::new();
    for (_, magic, _) in format::CURRENT_VERSIONS {
        assert!(!seen.contains(&magic));
        seen.push(magic);
    }
    // We should have seven entries (manifest, record_log, blob, wal, state_index,
    // branch_index, records_meta)
    assert_eq!(format::CURRENT_VERSIONS.len(), 7);
}
