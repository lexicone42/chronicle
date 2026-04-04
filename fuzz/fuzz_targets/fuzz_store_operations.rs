//! Fuzz target: arbitrary store operation sequences.
//!
//! Exercises the full Store API with sequences of state registrations,
//! updates, branch operations, and queries. Finds panics, deadlocks,
//! and state corruption under arbitrary operation orderings.

#![no_main]

use arbitrary::Arbitrary;
use chronicle::{
    Sequence, StateOperation, StateRegistration, StateStrategy, Store, StoreConfig, TreeEntry,
};
use libfuzzer_sys::fuzz_target;
use tempfile::TempDir;

#[derive(Arbitrary, Debug)]
enum StoreOp {
    /// Register an AppendLog state
    RegisterAppendLog { id: u8, delta_every: u8, full_every: u8 },
    /// Register a Tree state
    RegisterTree { id: u8 },
    /// Append a JSON string to a state
    Append { id: u8, value: u8 },
    /// Edit an item in a state
    Edit { id: u8, index: u8, value: u8 },
    /// Redact a range from a state
    Redact { id: u8, start: u8, end: u8 },
    /// Set a tree entry
    TreeSet { id: u8, path_seed: u8 },
    /// Remove a tree entry
    TreeRemove { id: u8, path_seed: u8 },
    /// Get current state
    GetState { id: u8 },
    /// Get historical state
    GetStateAt { id: u8, seq: u16 },
    /// Create a branch
    CreateBranch { name_seed: u8 },
    /// Switch branch
    SwitchBranch { name_seed: u8 },
    /// Delete branch
    DeleteBranch { name_seed: u8 },
    /// Compact a state
    Compact { id: u8 },
    /// Compact all states
    CompactAll,
}

fn state_id(id: u8) -> String {
    format!("s{}", id % 4) // Limit to 4 state IDs
}

fn branch_name(seed: u8) -> String {
    match seed % 4 {
        0 => "main".to_string(),
        n => format!("b{}", n),
    }
}

fn tree_path(seed: u8) -> String {
    match seed % 6 {
        0 => "a/b".to_string(),
        1 => "c".to_string(),
        2 => "a/d/e".to_string(),
        3 => "f".to_string(),
        4 => "a/g".to_string(),
        _ => "h/i/j".to_string(),
    }
}

fuzz_target!(|ops: Vec<StoreOp>| {
    // Limit to avoid timeouts
    let ops = if ops.len() > 50 { &ops[..50] } else { &ops };

    let dir = TempDir::new().unwrap();
    let store = match Store::create(StoreConfig {
        path: dir.path().join("store"),
        blob_cache_size: 10,
        create_if_missing: true,
    }) {
        Ok(s) => s,
        Err(_) => return,
    };

    for op in ops {
        match op {
            StoreOp::RegisterAppendLog { id, delta_every, full_every } => {
                let de = (*delta_every as u64).max(1);
                let fe = (*full_every as u64).max(1);
                let _ = store.register_state(StateRegistration {
                    id: state_id(*id),
                    strategy: StateStrategy::AppendLog {
                        delta_snapshot_every: de,
                        full_snapshot_every: fe,
                    },
                    initial_value: None,
                });
            }
            StoreOp::RegisterTree { id } => {
                let _ = store.register_state(StateRegistration {
                    id: state_id(*id),
                    strategy: StateStrategy::Tree {
                        delta_snapshot_every: 5,
                        full_snapshot_every: 3,
                    },
                    initial_value: None,
                });
            }
            StoreOp::Append { id, value } => {
                let json = format!("\"v{}\"", value);
                let _ = store.update_state(
                    &state_id(*id),
                    StateOperation::Append(json.into_bytes()),
                );
            }
            StoreOp::Edit { id, index, value } => {
                let json = format!("\"e{}\"", value);
                let _ = store.update_state(
                    &state_id(*id),
                    StateOperation::Edit {
                        index: *index as usize,
                        new_value: json.into_bytes(),
                    },
                );
            }
            StoreOp::Redact { id, start, end } => {
                let _ = store.update_state(
                    &state_id(*id),
                    StateOperation::Redact {
                        start: *start as usize,
                        end: *end as usize,
                    },
                );
            }
            StoreOp::TreeSet { id, path_seed } => {
                let _ = store.tree_set(
                    &state_id(*id),
                    &tree_path(*path_seed),
                    &TreeEntry {
                        blob_hash: "deadbeef".repeat(8),
                        size: 42,
                        mode: 0o644,
                    },
                );
            }
            StoreOp::TreeRemove { id, path_seed } => {
                let _ = store.tree_remove(&state_id(*id), &tree_path(*path_seed));
            }
            StoreOp::GetState { id } => {
                let _ = store.get_state(&state_id(*id));
            }
            StoreOp::GetStateAt { id, seq } => {
                let _ = store.get_state_at(&state_id(*id), Sequence(*seq as u64));
            }
            StoreOp::CreateBranch { name_seed } => {
                let name = branch_name(*name_seed);
                if name != "main" {
                    let _ = store.create_branch(&name, None);
                }
            }
            StoreOp::SwitchBranch { name_seed } => {
                let _ = store.switch_branch(&branch_name(*name_seed));
            }
            StoreOp::DeleteBranch { name_seed } => {
                let name = branch_name(*name_seed);
                if name != "main" {
                    let _ = store.delete_branch(&name);
                }
            }
            StoreOp::Compact { id } => {
                let _ = store.compact_state(&state_id(*id));
            }
            StoreOp::CompactAll => {
                let _ = store.compact_all_states();
            }
        }
    }

    // Final consistency check: all states should be readable without panic
    for i in 0..4 {
        let _ = store.get_state(&state_id(i));
    }
});
