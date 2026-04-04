//! Tests for subscription functionality — targeting gaps identified by
//! mutation testing (cargo-mutants missed mutants in catch_up, stats,
//! subscription_count, and mark_subscription_caught_up).

use chronicle::{
    RecordInput, Sequence, StateOperation, StateRegistration, StateStrategy, Store, StoreConfig,
    StoreEvent, SubscriptionConfig, SubscriptionFilter,
};
use serde_json::json;
use std::time::Duration;
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
// Mutant: Store::subscription_count -> 0
// ===========================================================================

#[test]
fn test_subscription_count_tracks_active_subs() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    assert_eq!(store.subscription_count(), 0);

    let h1 = store.subscribe(SubscriptionConfig::default());
    assert_eq!(store.subscription_count(), 1);

    let h2 = store.subscribe(SubscriptionConfig::default());
    assert_eq!(store.subscription_count(), 2);

    store.unsubscribe(h1.id);
    assert_eq!(store.subscription_count(), 1);

    store.unsubscribe(h2.id);
    assert_eq!(store.subscription_count(), 0);
}

// ===========================================================================
// Mutant: Store::stats total_size_bytes (+ with *)
// ===========================================================================

#[test]
fn test_stats_total_size_includes_log_and_blobs() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let stats_empty = store.stats().unwrap();
    assert_eq!(stats_empty.record_count, 0);
    assert_eq!(stats_empty.blob_count, 0);

    // Add some records
    store
        .append(RecordInput::json("test", &json!({"data": "hello"})).unwrap())
        .unwrap();

    let stats_with_record = store.stats().unwrap();
    assert_eq!(stats_with_record.record_count, 1);
    assert!(
        stats_with_record.total_size_bytes > 0,
        "total_size_bytes should be > 0 after adding a record"
    );

    // Add a blob
    store.store_blob(b"blob content here", "text/plain").unwrap();

    let stats_with_blob = store.stats().unwrap();
    assert_eq!(stats_with_blob.blob_count, 1);
    assert!(
        stats_with_blob.total_size_bytes > stats_with_record.total_size_bytes,
        "total_size_bytes should increase after adding a blob (was {}, now {})",
        stats_with_record.total_size_bytes,
        stats_with_blob.total_size_bytes
    );
    assert!(
        stats_with_blob.blob_size_bytes > 0,
        "blob_size_bytes should be > 0"
    );

    // Verify total = log + blobs (the mutant changes + to *)
    // If * were used, total would be log_size * blob_size which is way bigger
    let log_size = stats_with_blob.total_size_bytes - stats_with_blob.blob_size_bytes;
    assert!(log_size > 0, "log component of total_size should be > 0");
    assert!(
        stats_with_blob.total_size_bytes < stats_with_blob.blob_size_bytes * 100,
        "total_size should be reasonable (not a product of two sizes)"
    );
}

// ===========================================================================
// Mutant: Store::mark_subscription_caught_up -> Ok(())
// ===========================================================================

#[test]
fn test_mark_subscription_caught_up() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    // Add some records first
    for i in 0..5 {
        store
            .append(RecordInput::json("event", &json!({"i": i})).unwrap())
            .unwrap();
    }

    // Subscribe with catch-up from sequence 1
    let handle = store.subscribe(SubscriptionConfig {
        from_sequence: Some(Sequence(1)),
        filter: SubscriptionFilter::records(),
        ..Default::default()
    });

    // Manually mark as caught up (skips replay, goes straight to live)
    store.mark_subscription_caught_up(handle.id).unwrap();

    // Drain the CaughtUp event
    let _ = handle.recv_timeout(Duration::from_millis(100));

    // Now add a new record — subscriber should get it as a live event
    store
        .append(RecordInput::json("live_event", &json!({"live": true})).unwrap())
        .unwrap();

    // Should receive the live event
    let event = handle.recv_timeout(Duration::from_secs(1)).unwrap();
    match event {
        StoreEvent::Record { record } => {
            assert_eq!(record.record_type, "live_event");
        }
        other => panic!("Expected Record event, got {:?}", other),
    }

    store.unsubscribe(handle.id);
}

// ===========================================================================
// Mutant: catch_up_subscription boundary conditions (> vs == vs >=)
// ===========================================================================

#[test]
fn test_catch_up_subscription_replays_historical_records() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    // Add records
    for i in 0..5 {
        store
            .append(
                RecordInput::json("msg", &json!({"i": i})).unwrap(),
            )
            .unwrap();
    }

    // Subscribe with catch-up from sequence 1
    let handle = store.subscribe(SubscriptionConfig {
        from_sequence: Some(Sequence(1)),
        filter: SubscriptionFilter::records(),
        ..Default::default()
    });

    store.catch_up_subscription(handle.id).unwrap();

    // Should receive historical records + CaughtUp marker
    let mut record_count = 0;
    let mut caught_up = false;
    loop {
        match handle.recv_timeout(Duration::from_secs(2)) {
            Ok(StoreEvent::Record { .. }) => record_count += 1,
            Ok(StoreEvent::CaughtUp) => {
                caught_up = true;
                break;
            }
            Ok(other) => {
                // Skip other event types (branch head, etc)
                continue;
            }
            Err(_) => break,
        }
    }

    assert!(caught_up, "Should receive CaughtUp event");
    assert!(
        record_count >= 5,
        "Should receive at least 5 historical records, got {}",
        record_count
    );

    store.unsubscribe(handle.id);
}

#[test]
fn test_catch_up_subscription_no_from_sequence_marks_caught_up() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    // Subscribe without from_sequence (live only)
    let handle = store.subscribe(SubscriptionConfig {
        from_sequence: None,
        filter: SubscriptionFilter::records(),
        ..Default::default()
    });

    // Catch up should immediately mark as caught up
    store.catch_up_subscription(handle.id).unwrap();

    let event = handle.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        matches!(event, StoreEvent::CaughtUp),
        "Should receive CaughtUp immediately when no from_sequence"
    );

    store.unsubscribe(handle.id);
}

#[test]
fn test_catch_up_with_state_snapshots() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    // Register state and add data
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

    for i in 0..3 {
        store
            .update_state(
                "messages",
                StateOperation::Append(format!("\"msg{}\"", i).into_bytes()),
            )
            .unwrap();
    }

    // Subscribe to everything with catch-up
    let handle = store.subscribe(SubscriptionConfig {
        from_sequence: Some(Sequence(1)),
        filter: SubscriptionFilter::states(vec!["messages".to_string()]),
        max_snapshot_bytes: 10 * 1024 * 1024,
        ..Default::default()
    });

    store.catch_up_subscription(handle.id).unwrap();

    // Should receive state snapshot + records + CaughtUp
    let mut got_snapshot = false;
    let mut got_caught_up = false;
    loop {
        match handle.recv_timeout(Duration::from_secs(2)) {
            Ok(StoreEvent::StateSnapshot { state_id, data, .. }) => {
                assert_eq!(state_id, "messages");
                // Snapshot at from_sequence may have partial state
                if let serde_json::Value::Array(arr) = data {
                    assert!(arr.len() >= 1, "Snapshot should have at least 1 item");
                }
                got_snapshot = true;
            }
            Ok(StoreEvent::CaughtUp) => {
                got_caught_up = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    assert!(got_snapshot, "Should receive state snapshot during catch-up");
    assert!(got_caught_up, "Should receive CaughtUp event");

    store.unsubscribe(handle.id);
}

#[test]
fn test_catch_up_respects_max_snapshot_bytes() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    store
        .register_state(StateRegistration {
            id: "big".to_string(),
            strategy: StateStrategy::AppendLog {
                delta_snapshot_every: 100,
                full_snapshot_every: 10,
            },
            initial_value: None,
        })
        .unwrap();

    // Add enough data to exceed a small snapshot limit
    for i in 0..50 {
        store
            .update_state(
                "big",
                StateOperation::Append(
                    format!("\"item_{:04}_with_some_padding_data\"", i).into_bytes(),
                ),
            )
            .unwrap();
    }

    // Get current sequence so we can catch up from "now" (gets full current state)
    let current_seq = store.current_branch().unwrap().head;

    // Subscribe with a very small max_snapshot_bytes, catch up from current
    let handle = store.subscribe(SubscriptionConfig {
        from_sequence: Some(current_seq),
        filter: SubscriptionFilter::states(vec!["big".to_string()]),
        max_snapshot_bytes: 200, // Small — 50 items won't fit in 200 bytes
        ..Default::default()
    });

    store.catch_up_subscription(handle.id).unwrap();

    // Should receive a truncated snapshot
    let mut got_truncated_snapshot = false;
    loop {
        match handle.recv_timeout(Duration::from_secs(2)) {
            Ok(StoreEvent::StateSnapshot {
                truncated,
                total_length,
                from_index,
                ..
            }) => {
                if truncated {
                    got_truncated_snapshot = true;
                    assert_eq!(
                        total_length.unwrap(), 50,
                        "Total length should be 50"
                    );
                    assert!(
                        from_index.unwrap() > 0,
                        "from_index should be > 0 for truncated snapshot"
                    );
                }
            }
            Ok(StoreEvent::CaughtUp) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    assert!(
        got_truncated_snapshot,
        "Should receive a truncated state snapshot"
    );

    store.unsubscribe(handle.id);
}

// ===========================================================================
// Live subscription events
// ===========================================================================

#[test]
fn test_live_subscription_receives_records() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let handle = store.subscribe(SubscriptionConfig {
        from_sequence: None,
        filter: SubscriptionFilter::records(),
        ..Default::default()
    });

    // Must call catch_up first to activate the subscription
    store.catch_up_subscription(handle.id).unwrap();

    // Drain CaughtUp event
    let _ = handle.recv_timeout(Duration::from_millis(100));

    // Add a record after subscribing
    store
        .append(RecordInput::json("test.event", &json!({"live": true})).unwrap())
        .unwrap();

    let event = handle.recv_timeout(Duration::from_secs(1)).unwrap();
    match event {
        StoreEvent::Record { record } => {
            assert_eq!(record.record_type, "test.event");
        }
        other => panic!("Expected Record, got {:?}", other),
    }

    store.unsubscribe(handle.id);
}

#[test]
fn test_live_subscription_receives_branch_events() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let handle = store.subscribe(SubscriptionConfig {
        from_sequence: None,
        filter: SubscriptionFilter::branches(),
        ..Default::default()
    });

    store.catch_up_subscription(handle.id).unwrap();
    let _ = handle.recv_timeout(Duration::from_millis(100)); // drain CaughtUp

    store.create_branch("new-branch", None).unwrap();

    // Should receive branch created event
    let event = handle.recv_timeout(Duration::from_secs(1)).unwrap();
    match event {
        StoreEvent::BranchCreated { branch } => {
            assert_eq!(branch.name, "new-branch");
        }
        other => panic!("Expected BranchCreated, got {:?}", other),
    }

    store.unsubscribe(handle.id);
}

#[test]
fn test_unsubscribe_stops_events() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    let handle = store.subscribe(SubscriptionConfig {
        from_sequence: None,
        filter: SubscriptionFilter::records(),
        ..Default::default()
    });

    store.catch_up_subscription(handle.id).unwrap();
    let _ = handle.recv_timeout(Duration::from_millis(100)); // drain CaughtUp

    // Verify subscription is active (count = 1)
    assert_eq!(store.subscription_count(), 1);

    store.unsubscribe(handle.id);

    // Verify subscription is removed (count = 0)
    assert_eq!(store.subscription_count(), 0);

    // Add records after unsubscribing
    for _ in 0..5 {
        store
            .append(RecordInput::json("post_unsub", &json!({})).unwrap())
            .unwrap();
    }

    // Drain any events — should get Dropped or nothing, but NOT the post_unsub records
    let mut post_unsub_records = 0;
    while let Ok(event) = handle.recv_timeout(Duration::from_millis(50)) {
        if let StoreEvent::Record { record } = event {
            if record.record_type == "post_unsub" {
                post_unsub_records += 1;
            }
        }
    }
    assert_eq!(
        post_unsub_records, 0,
        "Should not receive records after unsubscribe"
    );
}
