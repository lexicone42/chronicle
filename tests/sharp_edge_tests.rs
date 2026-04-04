//! Tests for sharp edge fixes — verifying that footguns are properly guarded.
//!
//! Each test targets a specific sharp edge identified during the API review.
//! These serve as regression tests: if any guard is accidentally removed,
//! the corresponding test will fail.

use chronicle::{
    StateOperation, StateRegistration, StateStrategy, Store, StoreConfig, StoreError, TreeEntry,
};
use tempfile::TempDir;

fn test_store(dir: &TempDir) -> Store {
    Store::create(StoreConfig {
        path: dir.path().join("store"),
        blob_cache_size: 100,
        create_if_missing: true,
    })
    .unwrap()
}

// ===========================================================================
// Sharp Edge #1: Zero snapshot frequency
// ===========================================================================

#[test]
fn test_reject_zero_delta_snapshot_frequency() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let result = store.register_state(StateRegistration {
        id: "bad".to_string(),
        strategy: StateStrategy::AppendLog {
            delta_snapshot_every: 0,
            full_snapshot_every: 10,
        },
        initial_value: None,
    });

    assert!(result.is_err(), "Should reject delta_snapshot_every=0");
    assert!(
        matches!(result.unwrap_err(), StoreError::InvalidOperation(_)),
        "Should be InvalidOperation error"
    );
}

#[test]
fn test_reject_zero_full_snapshot_frequency() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let result = store.register_state(StateRegistration {
        id: "bad".to_string(),
        strategy: StateStrategy::AppendLog {
            delta_snapshot_every: 10,
            full_snapshot_every: 0,
        },
        initial_value: None,
    });

    assert!(result.is_err(), "Should reject full_snapshot_every=0");
}

#[test]
fn test_reject_zero_tree_snapshot_frequency() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let result = store.register_state(StateRegistration {
        id: "bad".to_string(),
        strategy: StateStrategy::Tree {
            delta_snapshot_every: 0,
            full_snapshot_every: 5,
        },
        initial_value: None,
    });

    assert!(result.is_err(), "Should reject zero tree snapshot frequency");
}

#[test]
fn test_accept_valid_snapshot_frequencies() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    // These should all succeed
    store
        .register_state(StateRegistration {
            id: "al".to_string(),
            strategy: StateStrategy::AppendLog {
                delta_snapshot_every: 1,
                full_snapshot_every: 1,
            },
            initial_value: None,
        })
        .unwrap();

    store
        .register_state(StateRegistration {
            id: "tree".to_string(),
            strategy: StateStrategy::Tree {
                delta_snapshot_every: 100,
                full_snapshot_every: 50,
            },
            initial_value: None,
        })
        .unwrap();

    // Snapshot strategy has no frequency — should always work
    store
        .register_state(StateRegistration {
            id: "snap".to_string(),
            strategy: StateStrategy::Snapshot,
            initial_value: None,
        })
        .unwrap();
}

// ===========================================================================
// Sharp Edge #2: Empty state IDs
// ===========================================================================

#[test]
fn test_reject_empty_state_id() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let result = store.register_state(StateRegistration {
        id: "".to_string(),
        strategy: StateStrategy::Snapshot,
        initial_value: None,
    });

    assert!(result.is_err(), "Should reject empty state ID");
    assert!(
        matches!(result.unwrap_err(), StoreError::InvalidOperation(_)),
        "Should be InvalidOperation error"
    );
}

#[test]
fn test_accept_nonempty_state_ids() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    // Various valid state IDs
    for id in &["a", "messages", "state-with-dashes", "state.with.dots", "123"] {
        store
            .register_state(StateRegistration {
                id: id.to_string(),
                strategy: StateStrategy::Snapshot,
                initial_value: None,
            })
            .unwrap();
    }
}

// ===========================================================================
// Sharp Edge #3: Empty branch names
// ===========================================================================

#[test]
fn test_reject_empty_branch_name() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let result = store.create_branch("", None);

    assert!(result.is_err(), "Should reject empty branch name");
    assert!(
        matches!(result.unwrap_err(), StoreError::InvalidOperation(_)),
        "Should be InvalidOperation error"
    );
}

#[test]
fn test_accept_valid_branch_names() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    for name in &["feature", "experiment-1", "user/branch", "a"] {
        store.create_branch(name, None).unwrap();
    }
}

// ===========================================================================
// Sharp Edge #4: Strategy mismatch — Append on non-AppendLog
// ===========================================================================

#[test]
fn test_reject_append_on_snapshot_strategy() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store
        .register_state(StateRegistration {
            id: "snap".to_string(),
            strategy: StateStrategy::Snapshot,
            initial_value: None,
        })
        .unwrap();

    let result = store.update_state("snap", StateOperation::Append(b"\"value\"".to_vec()));
    assert!(result.is_err(), "Append should be rejected on Snapshot strategy");
    assert!(matches!(result.unwrap_err(), StoreError::InvalidOperation(_)));
}

#[test]
fn test_reject_edit_on_snapshot_strategy() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store
        .register_state(StateRegistration {
            id: "snap".to_string(),
            strategy: StateStrategy::Snapshot,
            initial_value: None,
        })
        .unwrap();

    let result = store.update_state(
        "snap",
        StateOperation::Edit {
            index: 0,
            new_value: b"\"x\"".to_vec(),
        },
    );
    assert!(result.is_err(), "Edit should be rejected on Snapshot strategy");
}

#[test]
fn test_reject_redact_on_snapshot_strategy() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store
        .register_state(StateRegistration {
            id: "snap".to_string(),
            strategy: StateStrategy::Snapshot,
            initial_value: None,
        })
        .unwrap();

    let result = store.update_state(
        "snap",
        StateOperation::Redact { start: 0, end: 1 },
    );
    assert!(result.is_err(), "Redact should be rejected on Snapshot strategy");
}

#[test]
fn test_reject_append_on_tree_strategy() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store
        .register_state(StateRegistration {
            id: "tree".to_string(),
            strategy: StateStrategy::Tree {
                delta_snapshot_every: 50,
                full_snapshot_every: 10,
            },
            initial_value: None,
        })
        .unwrap();

    let result = store.update_state("tree", StateOperation::Append(b"\"value\"".to_vec()));
    assert!(result.is_err(), "Append should be rejected on Tree strategy");
}

#[test]
fn test_reject_tree_ops_on_appendlog_strategy() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store
        .register_state(StateRegistration {
            id: "al".to_string(),
            strategy: StateStrategy::AppendLog {
                delta_snapshot_every: 50,
                full_snapshot_every: 10,
            },
            initial_value: None,
        })
        .unwrap();

    let result = store.tree_set(
        "al",
        "some/path",
        &TreeEntry {
            blob_hash: "a".repeat(64),
            size: 1,
            mode: 0o644,
        },
    );
    assert!(result.is_err(), "TreeSet should be rejected on AppendLog strategy");
}

#[test]
fn test_accept_append_on_appendlog_strategy() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store
        .register_state(StateRegistration {
            id: "al".to_string(),
            strategy: StateStrategy::AppendLog {
                delta_snapshot_every: 50,
                full_snapshot_every: 10,
            },
            initial_value: None,
        })
        .unwrap();

    // Should succeed
    store
        .update_state("al", StateOperation::Append(b"\"hello\"".to_vec()))
        .unwrap();

    let state = store.get_state("al").unwrap().unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&state).unwrap();
    assert_eq!(items.len(), 1);
}

#[test]
fn test_set_works_on_any_strategy() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    // Set should work on Snapshot
    store
        .register_state(StateRegistration {
            id: "snap".to_string(),
            strategy: StateStrategy::Snapshot,
            initial_value: None,
        })
        .unwrap();
    store
        .update_state("snap", StateOperation::Set(b"{\"key\":\"val\"}".to_vec()))
        .unwrap();

    // Set should also work on AppendLog (for initialization)
    store
        .register_state(StateRegistration {
            id: "al".to_string(),
            strategy: StateStrategy::AppendLog {
                delta_snapshot_every: 50,
                full_snapshot_every: 10,
            },
            initial_value: None,
        })
        .unwrap();
    store
        .update_state("al", StateOperation::Set(b"[]".to_vec()))
        .unwrap();
}

// ===========================================================================
// Sharp Edge #5: Operations on unregistered state
// ===========================================================================

#[test]
fn test_reject_update_on_unregistered_state() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let result = store.update_state(
        "nonexistent",
        StateOperation::Append(b"\"value\"".to_vec()),
    );

    assert!(result.is_err(), "Should reject operations on unregistered state");
    assert!(
        matches!(result.unwrap_err(), StoreError::StateNotRegistered(_)),
        "Should be StateNotRegistered error"
    );
}

#[test]
fn test_reject_set_on_unregistered_state() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let result = store.update_state("ghost", StateOperation::Set(b"data".to_vec()));
    assert!(result.is_err(), "Set should fail on unregistered state");
    assert!(matches!(result.unwrap_err(), StoreError::StateNotRegistered(_)));
}

#[test]
fn test_reject_tree_ops_on_unregistered_state() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let result = store.tree_set(
        "ghost",
        "path",
        &TreeEntry {
            blob_hash: "a".repeat(64),
            size: 1,
            mode: 0o644,
        },
    );
    assert!(result.is_err(), "TreeSet should fail on unregistered state");
}

// ===========================================================================
// Sharp Edge #6: Duplicate state registration
// ===========================================================================

#[test]
fn test_reject_duplicate_state_registration() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store
        .register_state(StateRegistration {
            id: "data".to_string(),
            strategy: StateStrategy::Snapshot,
            initial_value: None,
        })
        .unwrap();

    let result = store.register_state(StateRegistration {
        id: "data".to_string(),
        strategy: StateStrategy::Snapshot,
        initial_value: None,
    });

    assert!(result.is_err(), "Should reject duplicate state registration");
    assert!(matches!(result.unwrap_err(), StoreError::StateExists(_)));
}

// ===========================================================================
// Sharp Edge #7: Delete current branch
// ===========================================================================

#[test]
fn test_cannot_delete_current_branch() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store.create_branch("feature", None).unwrap();
    store.switch_branch("feature").unwrap();

    let result = store.delete_branch("feature");
    assert!(result.is_err(), "Should not be able to delete current branch");
}

#[test]
fn test_cannot_delete_main_branch() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let result = store.delete_branch("main");
    assert!(result.is_err(), "Should not be able to delete main branch");
}

// ===========================================================================
// Sharp Edge #8: Branch operations after deletion
// ===========================================================================

#[test]
fn test_switch_to_deleted_branch_fails() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store.create_branch("temp", None).unwrap();
    store.delete_branch("temp").unwrap();

    let result = store.switch_branch("temp");
    assert!(result.is_err(), "Should not switch to deleted branch");
}

// ===========================================================================
// Sharp Edge #9: Consistency under strategy-correct operations
// ===========================================================================

#[test]
fn test_appendlog_full_lifecycle() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store
        .register_state(StateRegistration {
            id: "log".to_string(),
            strategy: StateStrategy::AppendLog {
                delta_snapshot_every: 3,
                full_snapshot_every: 2,
            },
            initial_value: None,
        })
        .unwrap();

    // Append several items (triggers delta + full snapshots)
    for i in 0..15 {
        store
            .update_state(
                "log",
                StateOperation::Append(format!("\"item{}\"", i).into_bytes()),
            )
            .unwrap();
    }

    // Verify state
    let state = store.get_state("log").unwrap().unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&state).unwrap();
    assert_eq!(items.len(), 15);

    // Edit last item
    store
        .update_state(
            "log",
            StateOperation::Edit {
                index: 14,
                new_value: b"\"edited\"".to_vec(),
            },
        )
        .unwrap();

    let state = store.get_state("log").unwrap().unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&state).unwrap();
    assert_eq!(items[14], "edited");

    // Redact 3 items from the middle
    store
        .update_state(
            "log",
            StateOperation::Redact { start: 5, end: 8 },
        )
        .unwrap();

    let state = store.get_state("log").unwrap().unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&state).unwrap();
    assert_eq!(items.len(), 12, "Should have 15 - 3 = 12 items after redact");

    // Compact and verify nothing changed
    let state_before = store.get_state("log").unwrap();
    store.compact_state("log").ok();
    let state_after = store.get_state("log").unwrap();
    assert_eq!(state_before, state_after, "Compaction should not change state");
}

#[test]
fn test_tree_full_lifecycle() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store
        .register_state(StateRegistration {
            id: "fs".to_string(),
            strategy: StateStrategy::Tree {
                delta_snapshot_every: 3,
                full_snapshot_every: 2,
            },
            initial_value: None,
        })
        .unwrap();

    let entry = |hash: &str, size: u64| TreeEntry {
        blob_hash: hash.to_string(),
        size,
        mode: 0o644,
    };

    // Set several files
    store.tree_set("fs", "src/main.rs", &entry("aaa", 100)).unwrap();
    store.tree_set("fs", "src/lib.rs", &entry("bbb", 200)).unwrap();
    store.tree_set("fs", "README.md", &entry("ccc", 50)).unwrap();
    store.tree_set("fs", "tests/test.rs", &entry("ddd", 150)).unwrap();

    // Verify list
    let files = store.tree_list("fs", None).unwrap();
    assert_eq!(files.len(), 4);

    // Verify prefix listing
    let src_files = store.tree_list("fs", Some("src/")).unwrap();
    assert_eq!(src_files.len(), 2);

    // Overwrite a file
    store.tree_set("fs", "src/main.rs", &entry("eee", 300)).unwrap();
    let got = store.tree_get("fs", "src/main.rs").unwrap().unwrap();
    assert_eq!(got.blob_hash, "eee");
    assert_eq!(got.size, 300);

    // Remove a file
    store.tree_remove("fs", "README.md").unwrap();
    let files = store.tree_list("fs", None).unwrap();
    assert_eq!(files.len(), 3);
    assert!(store.tree_get("fs", "README.md").unwrap().is_none());
}
