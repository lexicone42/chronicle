//! Format matrix smoke test.
//!
//! Creates a fresh store, performs one of each operation to materialize
//! every file type, then reads the first bytes of each file and asserts
//! they match `format::CURRENT_VERSIONS`.
//!
//! This is a tripwire: bumping a version constant in `src/format/mod.rs`
//! without updating the writer (or vice versa) breaks the test loudly.

use chronicle::{
    format::CURRENT_VERSIONS, RecordInput, StateOperation, StateRegistration, StateStrategy,
    Store, StoreConfig,
};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Create a store and perform one of each operation so that all files
/// on disk are materialized. Returns the store root path so tests can
/// inspect individual files.
fn populate_fresh_store(dir: &TempDir) -> PathBuf {
    let root = dir.path().join("store");

    let store = Store::create(StoreConfig {
        path: root.clone(),
        blob_cache_size: 10,
        create_if_missing: true,
    })
    .unwrap();

    // A record → records.log gets a record
    store
        .append(RecordInput::json("test", &json!({"data": 1})).unwrap())
        .unwrap();

    // A branch → branches.bin gets updated
    store.create_branch("feature", None).unwrap();

    // A state → state.bin gets updated
    store
        .register_state(StateRegistration {
            id: "items".to_string(),
            strategy: StateStrategy::AppendLog {
                delta_snapshot_every: 10,
                full_snapshot_every: 5,
            },
            initial_value: None,
        })
        .unwrap();
    store
        .update_state("items", StateOperation::Append(b"\"first\"".to_vec()))
        .unwrap();

    // A blob → blobs/<shard>/<hash> gets written
    store.store_blob(b"blob content", "text/plain").unwrap();

    // Explicit sync → WAL, records.meta, state.bin, branches.bin all flushed
    store.sync().unwrap();

    // Drop the store to release the lock so the test can read files freely
    drop(store);

    root
}

/// Read the first `n` bytes of a file.
fn read_head(path: &std::path::Path, n: usize) -> Vec<u8> {
    let data = fs::read(path).unwrap_or_else(|e| panic!("reading {:?}: {}", path, e));
    assert!(
        data.len() >= n,
        "file {:?} is shorter than {} bytes (got {})",
        path,
        n,
        data.len()
    );
    data[..n].to_vec()
}

#[test]
fn format_matrix_matches_current_versions() {
    let dir = TempDir::new().unwrap();
    let root = populate_fresh_store(&dir);

    // Expected file paths for each logical component
    let path_for = |name: &str| -> PathBuf {
        match name {
            "manifest" => root.join("MANIFEST"),
            "record_log" => root.join("records.log"),
            "wal" => root.join("wal.log"),
            "state_index" => root.join("state.bin"),
            "branch_index" => root.join("branches.bin"),
            "records_meta" => root.join("records.meta"),
            "blob" => {
                // Find any blob file under blobs/
                let mut found = None;
                for shard in fs::read_dir(root.join("blobs")).unwrap() {
                    let shard = shard.unwrap();
                    if shard.file_type().unwrap().is_dir() {
                        for blob in fs::read_dir(shard.path()).unwrap() {
                            found = Some(blob.unwrap().path());
                            break;
                        }
                    }
                    if found.is_some() {
                        break;
                    }
                }
                found.expect("no blob file found — did populate_fresh_store create one?")
            }
            other => panic!("unexpected component name: {}", other),
        }
    };

    // Check every component in the current versions matrix
    for (name, expected_magic, expected_version) in CURRENT_VERSIONS {
        let path = path_for(name);
        let head = read_head(&path, 5);

        assert_eq!(
            &head[0..4],
            *expected_magic,
            "{}: wrong magic bytes (file: {:?}, got {:?}, expected {:?})",
            name,
            path,
            &head[0..4],
            expected_magic
        );

        assert_eq!(
            head[4],
            *expected_version,
            "{}: wrong version byte (file: {:?}, got {}, expected {})",
            name,
            path,
            head[4],
            expected_version
        );
    }
}

#[test]
fn all_expected_files_exist_after_sync() {
    let dir = TempDir::new().unwrap();
    let root = populate_fresh_store(&dir);

    // Files that must exist after a full operation cycle + sync
    let expected_files = [
        "MANIFEST",
        "LOCK",
        "records.log",
        "records.meta",
        "state.bin",
        "branches.bin",
        "wal.log",
    ];

    for f in expected_files {
        let path = root.join(f);
        assert!(
            path.exists(),
            "expected file {:?} does not exist after sync",
            path
        );
    }

    // At least one blob should exist
    let blobs_dir = root.join("blobs");
    assert!(blobs_dir.exists());
    let mut found_blob = false;
    for shard in fs::read_dir(&blobs_dir).unwrap() {
        let shard = shard.unwrap();
        if shard.file_type().unwrap().is_dir() {
            for _blob in fs::read_dir(shard.path()).unwrap() {
                found_blob = true;
                break;
            }
        }
    }
    assert!(found_blob, "no blob files found under {:?}", blobs_dir);
}
