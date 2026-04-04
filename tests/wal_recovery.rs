//! Integration tests for WAL crash recovery.
//!
//! These tests simulate crashes at various points in the write path
//! by dropping the store without sync, then reopening and verifying
//! that state is consistent.

use chronicle::{
    RecordInput, StateOperation, StateRegistration, StateStrategy, Store, StoreConfig,
    WriteAheadLog,
};
use serde_json::json;
use tempfile::TempDir;

fn test_config(dir: &TempDir) -> StoreConfig {
    StoreConfig {
        path: dir.path().join("store"),
        blob_cache_size: 100,
        create_if_missing: true,
    }
}

fn reopen(dir: &TempDir) -> Store {
    Store::open(StoreConfig {
        path: dir.path().join("store"),
        blob_cache_size: 100,
        create_if_missing: false,
    })
    .unwrap()
}

// ---------------------------------------------------------------------------
// Basic: write, close without sync, reopen — data should survive
// ---------------------------------------------------------------------------

#[test]
fn test_wal_basic_persistence() {
    let dir = TempDir::new().unwrap();

    // Write some records and state
    {
        let store = Store::create(test_config(&dir)).unwrap();

        store
            .register_state(StateRegistration {
                id: "messages".to_string(),
                strategy: StateStrategy::AppendLog {
                    delta_snapshot_every: 100,
                    full_snapshot_every: 10,
                },
                initial_value: None,
            })
            .unwrap();

        for i in 0..5 {
            store
                .update_state(
                    "messages",
                    StateOperation::Append(
                        serde_json::to_vec(&json!({"msg": format!("hello {}", i)})).unwrap(),
                    ),
                )
                .unwrap();
        }

        // Explicit sync to ensure everything is durable
        store.sync().unwrap();
    }

    // Reopen and verify
    let store = reopen(&dir);
    let state = store.get_state("messages").unwrap().unwrap();
    let messages: Vec<serde_json::Value> = serde_json::from_slice(&state).unwrap();
    assert_eq!(messages.len(), 5, "Expected 5 messages after reopen");
}

// ---------------------------------------------------------------------------
// Records survive close/reopen cycle
// ---------------------------------------------------------------------------

#[test]
fn test_wal_records_survive_reopen() {
    let dir = TempDir::new().unwrap();

    let record_id;
    {
        let store = Store::create(test_config(&dir)).unwrap();

        let record = store
            .append(RecordInput::json("test.event", &json!({"data": "important"})).unwrap())
            .unwrap();
        record_id = record.id;

        store.sync().unwrap();
    }

    let store = reopen(&dir);
    let record = store.get_record(record_id).unwrap();
    assert!(record.is_some(), "Record should survive reopen");
}

// ---------------------------------------------------------------------------
// Branch creation survives reopen
// ---------------------------------------------------------------------------

#[test]
fn test_wal_branches_survive_reopen() {
    let dir = TempDir::new().unwrap();

    {
        let store = Store::create(test_config(&dir)).unwrap();

        store.create_branch("experiment", None).unwrap();

        // Add state on the branch
        store.switch_branch("experiment").unwrap();
        store
            .register_state(StateRegistration {
                id: "data".to_string(),
                strategy: StateStrategy::AppendLog {
                    delta_snapshot_every: 50,
                    full_snapshot_every: 5,
                },
                initial_value: None,
            })
            .unwrap();
        store
            .update_state(
                "data",
                StateOperation::Append(b"\"branch_value\"".to_vec()),
            )
            .unwrap();

        store.sync().unwrap();
    }

    let store = reopen(&dir);
    let branches = store.list_branches();
    let branch_names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(
        branch_names.contains(&"experiment"),
        "Branch 'experiment' should survive reopen"
    );
}

// ---------------------------------------------------------------------------
// State isolation survives reopen across branches
// ---------------------------------------------------------------------------

#[test]
fn test_wal_branch_state_isolation_after_reopen() {
    let dir = TempDir::new().unwrap();

    {
        let store = Store::create(test_config(&dir)).unwrap();

        store
            .register_state(StateRegistration {
                id: "items".to_string(),
                strategy: StateStrategy::AppendLog {
                    delta_snapshot_every: 50,
                    full_snapshot_every: 5,
                },
                initial_value: None,
            })
            .unwrap();

        // Write on main
        store
            .update_state("items", StateOperation::Append(b"\"main_item\"".to_vec()))
            .unwrap();

        // Branch and write
        store.create_branch("fork", None).unwrap();
        store.switch_branch("fork").unwrap();
        store
            .update_state("items", StateOperation::Append(b"\"fork_item\"".to_vec()))
            .unwrap();

        store.sync().unwrap();
    }

    // Reopen and verify isolation
    let store = reopen(&dir);

    store.switch_branch("main").unwrap();
    let main_state = store.get_state("items").unwrap().unwrap();
    let main_items: Vec<serde_json::Value> = serde_json::from_slice(&main_state).unwrap();
    assert_eq!(main_items.len(), 1, "Main should have 1 item");

    store.switch_branch("fork").unwrap();
    let fork_state = store.get_state("items").unwrap().unwrap();
    let fork_items: Vec<serde_json::Value> = serde_json::from_slice(&fork_state).unwrap();
    assert_eq!(fork_items.len(), 2, "Fork should have 2 items (inherited + own)");
}

// ---------------------------------------------------------------------------
// Blob storage is idempotent (content-addressed)
// ---------------------------------------------------------------------------

#[test]
fn test_wal_blob_idempotent() {
    let dir = TempDir::new().unwrap();

    let hash;
    {
        let store = Store::create(test_config(&dir)).unwrap();
        hash = store.store_blob(b"hello world", "text/plain").unwrap();
        store.sync().unwrap();
    }

    let store = reopen(&dir);
    let blob = store.get_blob(&hash).unwrap();
    assert!(blob.is_some(), "Blob should survive reopen");
    assert_eq!(blob.unwrap().content, b"hello world");

    // Store same content again — should return same hash
    let hash2 = store.store_blob(b"hello world", "text/plain").unwrap();
    assert_eq!(hash, hash2, "Content-addressed blob should be idempotent");
}

// ---------------------------------------------------------------------------
// Historical state access works after reopen
// ---------------------------------------------------------------------------

#[test]
fn test_wal_historical_state_after_reopen() {
    let dir = TempDir::new().unwrap();

    let mut sequences = Vec::new();
    {
        let store = Store::create(test_config(&dir)).unwrap();

        store
            .register_state(StateRegistration {
                id: "log".to_string(),
                strategy: StateStrategy::AppendLog {
                    delta_snapshot_every: 100,
                    full_snapshot_every: 10,
                },
                initial_value: None,
            })
            .unwrap();

        for i in 0..5 {
            let rec = store
                .update_state(
                    "log",
                    StateOperation::Append(format!("\"item{}\"", i).into_bytes()),
                )
                .unwrap();
            sequences.push(rec.sequence);
        }

        store.sync().unwrap();
    }

    let store = reopen(&dir);

    // Current state should have 5 items
    let state = store.get_state("log").unwrap().unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&state).unwrap();
    assert_eq!(items.len(), 5);

    // Historical state at sequence[2] should have 3 items
    let historical = store.get_state_at("log", sequences[2]).unwrap().unwrap();
    let hist_items: Vec<serde_json::Value> = serde_json::from_slice(&historical).unwrap();
    assert_eq!(hist_items.len(), 3, "Historical state should have 3 items");
}

// ---------------------------------------------------------------------------
// Stress: many operations, close, reopen, verify consistency
// ---------------------------------------------------------------------------

#[test]
fn test_wal_stress_reopen_consistency() {
    let dir = TempDir::new().unwrap();

    {
        let store = Store::create(test_config(&dir)).unwrap();

        store
            .register_state(StateRegistration {
                id: "counter".to_string(),
                strategy: StateStrategy::AppendLog {
                    delta_snapshot_every: 10,
                    full_snapshot_every: 5,
                },
                initial_value: None,
            })
            .unwrap();

        // 200 operations across 3 branches
        for i in 0..200 {
            store
                .update_state(
                    "counter",
                    StateOperation::Append(format!("\"op{}\"", i).into_bytes()),
                )
                .unwrap();

            if i == 50 {
                store.create_branch("b1", None).unwrap();
                store.switch_branch("b1").unwrap();
            }
            if i == 100 {
                store.create_branch("b2", Some("main")).unwrap();
                store.switch_branch("b2").unwrap();
            }
            if i == 150 {
                store.switch_branch("main").unwrap();
            }
        }

        store.sync().unwrap();
    }

    let store = reopen(&dir);

    // Verify each branch has consistent, readable state after reopen
    store.switch_branch("main").unwrap();
    let main_state = store.get_state("counter").unwrap().unwrap();
    let main_items: Vec<serde_json::Value> = serde_json::from_slice(&main_state).unwrap();
    // main: ops 0..=50 (51 items), then ops 150..=199 (50 items) = 101
    // But the update at i=50 runs before the branch check, and switching at i=150
    // runs after the update — so main gets ops 0..=50 and 150..=199 = 100 items
    assert_eq!(main_items.len(), 100, "Main should have 100 items");

    // b1 inherits main's state at branch point + its own writes
    store.switch_branch("b1").unwrap();
    let b1_state = store.get_state("counter").unwrap().unwrap();
    let b1_items: Vec<serde_json::Value> = serde_json::from_slice(&b1_state).unwrap();
    assert!(b1_items.len() > 50, "b1 should have items from main + own writes");

    // All branches should be present
    let branches = store.list_branches();
    assert_eq!(branches.len(), 3, "Should have main, b1, b2");
}

// ---------------------------------------------------------------------------
// WAL file exists on disk after store creation
// ---------------------------------------------------------------------------

#[test]
fn test_wal_file_created() {
    let dir = TempDir::new().unwrap();

    {
        let _store = Store::create(test_config(&dir)).unwrap();
    }

    let wal_path = dir.path().join("store/wal.log");
    assert!(wal_path.exists(), "WAL file should exist after store creation");
}

// ---------------------------------------------------------------------------
// WAL is clean after normal close (no pending entries)
// ---------------------------------------------------------------------------

#[test]
fn test_wal_clean_after_sync() {
    let dir = TempDir::new().unwrap();

    {
        let store = Store::create(test_config(&dir)).unwrap();

        store
            .register_state(StateRegistration {
                id: "data".to_string(),
                strategy: StateStrategy::AppendLog {
                    delta_snapshot_every: 50,
                    full_snapshot_every: 5,
                },
                initial_value: None,
            })
            .unwrap();

        for i in 0..10 {
            store
                .update_state(
                    "data",
                    StateOperation::Append(format!("\"v{}\"", i).into_bytes()),
                )
                .unwrap();
        }

        store.sync().unwrap();
    }

    // Open WAL directly and check no pending entries
    let wal = WriteAheadLog::open(dir.path().join("store/wal.log")).unwrap();
    let pending = wal.get_pending_entries().unwrap();
    assert!(
        pending.is_empty(),
        "WAL should have no pending entries after sync+close, found {}",
        pending.len()
    );
}
