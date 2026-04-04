//! Fuzz target: tree path validation bypass attempts.
//!
//! Feeds arbitrary strings as tree paths to find path traversal bypasses,
//! null byte injection, or other validation gaps in validate_tree_path().

#![no_main]

use chronicle::{StateRegistration, StateStrategy, Store, StoreConfig, TreeEntry};
use libfuzzer_sys::fuzz_target;
use tempfile::TempDir;

fuzz_target!(|paths: Vec<String>| {
    if paths.is_empty() || paths.len() > 20 {
        return;
    }

    let dir = TempDir::new().unwrap();
    let store = match Store::create(StoreConfig {
        path: dir.path().join("store"),
        blob_cache_size: 10,
        create_if_missing: true,
    }) {
        Ok(s) => s,
        Err(_) => return,
    };

    let _ = store.register_state(StateRegistration {
        id: "tree".to_string(),
        strategy: StateStrategy::Tree {
            delta_snapshot_every: 50,
            full_snapshot_every: 10,
        },
        initial_value: None,
    });

    let entry = TreeEntry {
        blob_hash: "a".repeat(64),
        size: 1,
        mode: 0o644,
    };

    for path in &paths {
        // Attempt to set — should either succeed cleanly or return an error.
        // Must never panic.
        match store.tree_set("tree", path, &entry) {
            Ok(_) => {
                // If set succeeded, get and list must also work
                let got = store.tree_get("tree", path);
                assert!(got.is_ok(), "tree_get panicked after successful tree_set");

                let list = store.tree_list("tree", None);
                assert!(list.is_ok(), "tree_list panicked after successful tree_set");

                // Path should not contain traversal components if it was accepted
                assert!(
                    !path.split('/').any(|c| c == ".." || c == "."),
                    "Path with traversal component was accepted: {:?}",
                    path
                );
                assert!(
                    !path.contains('\0'),
                    "Path with null byte was accepted: {:?}",
                    path
                );
            }
            Err(_) => {} // Rejection is fine
        }
    }
});
