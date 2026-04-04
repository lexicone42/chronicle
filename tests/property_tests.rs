//! Property-based tests for Chronicle.
//!
//! These tests verify core invariants that must hold for *any* sequence of
//! operations, not just hand-picked examples. They complement the existing
//! integration tests by exploring the combinatorial space of state mutations,
//! branch interactions, and snapshot boundaries.

use chronicle::{
    apply_operation, Sequence, StateOperation, StateRegistration, StateStrategy, Store, StoreConfig,
    TreeEntry,
};
use proptest::prelude::*;
use serde_json::json;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_store(dir: &TempDir) -> Store {
    Store::create(StoreConfig {
        path: dir.path().join("store"),
        blob_cache_size: 100,
        create_if_missing: true,
    })
    .unwrap()
}

fn register_append_log(store: &Store, id: &str, delta_every: u64, full_every: u64) {
    store
        .register_state(StateRegistration {
            id: id.to_string(),
            strategy: StateStrategy::AppendLog {
                delta_snapshot_every: delta_every,
                full_snapshot_every: full_every,
            },
            initial_value: None,
        })
        .unwrap();
}

fn register_tree(store: &Store, id: &str, delta_every: u64, full_every: u64) {
    store
        .register_state(StateRegistration {
            id: id.to_string(),
            strategy: StateStrategy::Tree {
                delta_snapshot_every: delta_every,
                full_snapshot_every: full_every,
            },
            initial_value: None,
        })
        .unwrap();
}

// ---------------------------------------------------------------------------
// Strategies: generate arbitrary operations
// ---------------------------------------------------------------------------

/// Generate a JSON-safe string value suitable for AppendLog items.
fn arb_json_string() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,7}".prop_map(|s| format!("\"{}\"", s))
}

/// Wrapper enum for test-time operation selection (AppendLog).
#[derive(Clone, Debug)]
enum TestOp {
    Append(String),
    Edit(String),
    Redact,
}

fn arb_test_op() -> impl Strategy<Value = TestOp> {
    prop_oneof![
        8 => arb_json_string().prop_map(TestOp::Append),
        1 => arb_json_string().prop_map(TestOp::Edit),
        1 => Just(TestOp::Redact),
    ]
}

/// Convert a TestOp into a StateOperation given current array length.
/// Returns None if the operation can't be applied (e.g., edit on empty array).
fn realize_op(op: &TestOp, current_len: usize) -> Option<StateOperation> {
    match op {
        TestOp::Append(val) => Some(StateOperation::Append(val.clone().into_bytes())),
        TestOp::Edit(val) => {
            if current_len == 0 {
                None
            } else {
                Some(StateOperation::Edit {
                    index: current_len - 1,
                    new_value: val.clone().into_bytes(),
                })
            }
        }
        TestOp::Redact => {
            if current_len == 0 {
                None
            } else {
                let start = current_len.saturating_sub(1);
                Some(StateOperation::Redact {
                    start,
                    end: current_len,
                })
            }
        }
    }
}

/// Generate a valid tree path component.
fn arb_tree_path() -> impl Strategy<Value = String> {
    prop::collection::vec("[a-z]{1,4}", 1..=3)
        .prop_map(|parts| parts.join("/"))
}

fn arb_tree_entry() -> impl Strategy<Value = TreeEntry> {
    ("[a-f0-9]{64}", 0u64..100_000).prop_map(|(hash, size)| TreeEntry {
        blob_hash: hash,
        size,
        mode: 0o644,
    })
}

#[derive(Clone, Debug)]
enum TreeTestOp {
    Set(String, TreeEntry),
    Remove(String),
}

fn arb_tree_test_op() -> impl Strategy<Value = TreeTestOp> {
    prop_oneof![
        3 => (arb_tree_path(), arb_tree_entry()).prop_map(|(p, e)| TreeTestOp::Set(p, e)),
        1 => arb_tree_path().prop_map(TreeTestOp::Remove),
    ]
}

// ===========================================================================
// 1. State Reconstruction: store chain must equal direct application
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// The core invariant: reconstructing state through the store (which uses
    /// chain walks, delta snapshots, full snapshots) must produce the same
    /// result as directly applying the same operations in sequence.
    #[test]
    fn prop_appendlog_reconstruction_matches_direct_apply(
        ops in prop::collection::vec(arb_test_op(), 1..60),
        delta_every in 2u64..10,
        full_every in 2u64..6,
    ) {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);
        register_append_log(&store, "test", delta_every, full_every);

        let mut direct_state: Vec<u8> = Vec::new();
        let mut current_len: usize = 0;

        for op in &ops {
            if let Some(state_op) = realize_op(op, current_len) {
                // Apply via store
                let store_result = store.update_state("test", state_op.clone());

                // Apply directly
                let direct_result = apply_operation(direct_state.clone(), state_op);

                match (store_result, direct_result) {
                    (Ok(_), Ok(new_direct)) => {
                        direct_state = new_direct;
                        // Count items for next op realization
                        if let Ok(arr) = serde_json::from_slice::<Vec<serde_json::Value>>(&direct_state) {
                            current_len = arr.len();
                        }
                    }
                    (Err(_), Err(_)) => {
                        // Both failed — fine, skip
                    }
                    (Ok(_), Err(e)) => {
                        prop_assert!(false, "Store succeeded but direct apply failed: {}", e);
                    }
                    (Err(e), Ok(_)) => {
                        prop_assert!(false, "Store failed but direct apply succeeded: {}", e);
                    }
                }
            }
        }

        // Final comparison
        let store_state = store.get_state("test").unwrap().unwrap_or_default();
        prop_assert_eq!(
            store_state, direct_state,
            "State diverged after {} ops (delta_every={}, full_every={})",
            ops.len(), delta_every, full_every
        );
    }
}

// ===========================================================================
// 2. Compaction preserves state
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// Compacting a state must not change its value. The state before and after
    /// compaction must be byte-identical.
    #[test]
    fn prop_compaction_preserves_state(
        ops in prop::collection::vec(arb_test_op(), 5..80),
        delta_every in 3u64..8,
        full_every in 2u64..5,
    ) {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);
        register_append_log(&store, "test", delta_every, full_every);

        let mut current_len: usize = 0;
        for op in &ops {
            if let Some(state_op) = realize_op(op, current_len) {
                if store.update_state("test", state_op).is_ok() {
                    if let Ok(Some(s)) = store.get_state("test") {
                        if let Ok(arr) = serde_json::from_slice::<Vec<serde_json::Value>>(&s) {
                            current_len = arr.len();
                        }
                    }
                }
            }
        }

        let state_before = store.get_state("test").unwrap();

        // Force compaction
        let _ = store.compact_state("test");

        let state_after = store.get_state("test").unwrap();
        prop_assert_eq!(
            state_before, state_after,
            "Compaction altered state!"
        );
    }
}

// ===========================================================================
// 3. Branch isolation: writes on one branch never leak to another
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Writes on a child branch must not be visible on the parent, and
    /// writes on the parent after branching must not be visible on the child.
    #[test]
    fn prop_branch_isolation(
        parent_ops in prop::collection::vec(arb_json_string(), 1..15),
        child_ops in prop::collection::vec(arb_json_string(), 1..15),
        parent_ops_after in prop::collection::vec(arb_json_string(), 0..10),
    ) {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);
        register_append_log(&store, "data", 50, 50);

        // Write on main
        for val in &parent_ops {
            store.update_state("data", StateOperation::Append(val.clone().into_bytes())).unwrap();
        }

        let _state_at_branch_point = store.get_state("data").unwrap();

        // Create child branch
        store.create_branch("child", None).unwrap();
        store.switch_branch("child").unwrap();

        // Write on child
        for val in &child_ops {
            store.update_state("data", StateOperation::Append(val.clone().into_bytes())).unwrap();
        }
        let child_state = store.get_state("data").unwrap();

        // Switch back to parent and write more
        store.switch_branch("main").unwrap();
        for val in &parent_ops_after {
            store.update_state("data", StateOperation::Append(val.clone().into_bytes())).unwrap();
        }
        let parent_final = store.get_state("data").unwrap();

        // Verify child state hasn't changed
        store.switch_branch("child").unwrap();
        let child_state_after = store.get_state("data").unwrap();
        prop_assert_eq!(
            &child_state, &child_state_after,
            "Child state changed after parent writes!"
        );

        // Verify parent doesn't contain child ops
        store.switch_branch("main").unwrap();
        let parent_state_now = store.get_state("data").unwrap();
        prop_assert_eq!(
            &parent_final, &parent_state_now,
            "Parent state changed!"
        );

        // Verify parent array length = parent_ops + parent_ops_after
        if let Some(ref state) = parent_state_now {
            let arr: Vec<serde_json::Value> = serde_json::from_slice(state).unwrap();
            prop_assert_eq!(
                arr.len(),
                parent_ops.len() + parent_ops_after.len(),
                "Parent has wrong number of items"
            );
        }

        // Verify child array length = parent_ops + child_ops
        store.switch_branch("child").unwrap();
        if let Some(ref state) = child_state_after {
            let arr: Vec<serde_json::Value> = serde_json::from_slice(state).unwrap();
            prop_assert_eq!(
                arr.len(),
                parent_ops.len() + child_ops.len(),
                "Child has wrong number of items"
            );
        }
    }
}

// ===========================================================================
// 4. Historical state access (get_state_at)
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// get_state_at(seq) must return the state that existed immediately after
    /// the operation at that sequence was applied.
    #[test]
    fn prop_historical_state_matches_snapshot(
        items in prop::collection::vec(arb_json_string(), 3..30),
        delta_every in 3u64..8,
        full_every in 2u64..5,
    ) {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);
        register_append_log(&store, "test", delta_every, full_every);

        // Track state at each sequence
        let mut snapshots: Vec<(Sequence, Vec<u8>)> = Vec::new();

        for item in &items {
            let rec = store.update_state(
                "test",
                StateOperation::Append(item.clone().into_bytes()),
            ).unwrap();
            let state = store.get_state("test").unwrap().unwrap_or_default();
            snapshots.push((rec.sequence, state));
        }

        // Verify every historical state matches
        for (seq, expected) in &snapshots {
            let historical = store.get_state_at("test", *seq).unwrap().unwrap_or_default();
            prop_assert_eq!(
                &historical, expected,
                "State at sequence {:?} doesn't match snapshot", seq
            );
        }
    }
}

// ===========================================================================
// 5. Tree state: set/remove/batch consistency
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// After any sequence of tree set/remove operations, tree_list and
    /// tree_get must agree: every listed path is gettable, every gettable
    /// path is listed.
    #[test]
    fn prop_tree_list_get_consistency(
        ops in prop::collection::vec(arb_tree_test_op(), 1..40),
        delta_every in 3u64..10,
        full_every in 2u64..5,
    ) {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);
        register_tree(&store, "files", delta_every, full_every);

        // Also track expected state directly via BTreeMap
        let mut expected = std::collections::BTreeMap::<String, TreeEntry>::new();

        for op in &ops {
            match op {
                TreeTestOp::Set(path, entry) => {
                    store.tree_set("files", path, entry).unwrap();
                    expected.insert(path.clone(), entry.clone());
                }
                TreeTestOp::Remove(path) => {
                    store.tree_remove("files", path).unwrap();
                    expected.remove(path);
                }
            }
        }

        // Verify list matches expected
        let listed = store.tree_list("files", None).unwrap();
        let listed_map: std::collections::BTreeMap<String, TreeEntry> = listed.into_iter().collect();

        prop_assert_eq!(
            listed_map.len(), expected.len(),
            "List length mismatch: got {} expected {}",
            listed_map.len(), expected.len()
        );

        for (path, expected_entry) in &expected {
            // tree_get must return the entry
            let got = store.tree_get("files", path).unwrap();
            prop_assert!(
                got.is_some(),
                "tree_get returned None for path '{}' that should exist", path
            );
            let got = got.unwrap();
            prop_assert_eq!(
                &got, expected_entry,
                "Entry mismatch for path '{}'", path
            );

            // Must also be in list
            prop_assert!(
                listed_map.contains_key(path),
                "Path '{}' not in tree_list but exists in tree_get", path
            );
        }
    }
}

// ===========================================================================
// 6. Serialization roundtrips for StateOperation
// ===========================================================================

fn arb_state_operation() -> impl Strategy<Value = StateOperation> {
    prop_oneof![
        // Set
        prop::collection::vec(any::<u8>(), 0..64)
            .prop_map(StateOperation::Set),
        // Append
        arb_json_string()
            .prop_map(|s| StateOperation::Append(s.into_bytes())),
        // Redact
        (0usize..50, 0usize..50)
            .prop_map(|(a, b)| StateOperation::Redact {
                start: a.min(b),
                end: a.max(b),
            }),
        // Edit
        (0usize..50, arb_json_string())
            .prop_map(|(idx, val)| StateOperation::Edit {
                index: idx,
                new_value: val.into_bytes(),
            }),
        // Snapshot
        prop::collection::vec(any::<u8>(), 0..64)
            .prop_map(StateOperation::Snapshot),
        // DeltaSnapshot
        Just(StateOperation::DeltaSnapshot(
            serde_json::to_vec(&json!(["a","b"])).unwrap()
        )),
        // TreeSet
        (arb_tree_path(), arb_tree_entry())
            .prop_map(|(p, e)| StateOperation::TreeSet { path: p, entry: e }),
        // TreeRemove
        arb_tree_path()
            .prop_map(|p| StateOperation::TreeRemove { path: p }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Every StateOperation must survive a JSON roundtrip.
    #[test]
    fn prop_state_operation_json_roundtrip(op in arb_state_operation()) {
        let serialized = serde_json::to_vec(&op).unwrap();
        let deserialized: StateOperation = serde_json::from_slice(&serialized).unwrap();

        // Re-serialize and compare bytes (since StateOperation doesn't derive PartialEq)
        let reserialized = serde_json::to_vec(&deserialized).unwrap();
        prop_assert_eq!(serialized, reserialized, "JSON roundtrip produced different bytes");
    }

    /// Every StateOperation must survive a MessagePack roundtrip.
    #[test]
    fn prop_state_operation_msgpack_roundtrip(op in arb_state_operation()) {
        let encoded = rmp_serde::to_vec(&op).unwrap();
        let decoded: StateOperation = rmp_serde::from_slice(&encoded).unwrap();

        // Roundtrip via JSON for comparison
        let original_json = serde_json::to_vec(&op).unwrap();
        let decoded_json = serde_json::to_vec(&decoded).unwrap();
        prop_assert_eq!(original_json, decoded_json, "MessagePack roundtrip changed value");
    }
}

// ===========================================================================
// 7. Reopen persistence: state survives close + reopen
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// State must be identical after closing and reopening the store.
    #[test]
    fn prop_state_survives_reopen(
        items in prop::collection::vec(arb_json_string(), 3..25),
        delta_every in 3u64..8,
        full_every in 2u64..5,
    ) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store");

        // Write
        {
            let store = Store::create(StoreConfig {
                path: path.clone(),
                blob_cache_size: 100,
                create_if_missing: true,
            }).unwrap();
            register_append_log(&store, "test", delta_every, full_every);

            for item in &items {
                store.update_state(
                    "test",
                    StateOperation::Append(item.clone().into_bytes()),
                ).unwrap();
            }
            store.sync().unwrap();
        }

        // Reopen and verify
        let store = Store::open(StoreConfig {
            path,
            blob_cache_size: 100,
            create_if_missing: false,
        }).unwrap();

        let state = store.get_state("test").unwrap().unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_slice(&state).unwrap();
        prop_assert_eq!(
            arr.len(), items.len(),
            "Lost items after reopen: got {} expected {}", arr.len(), items.len()
        );
    }
}
