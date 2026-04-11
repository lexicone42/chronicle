//! Performance benchmarks for the record store.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use chronicle::{
    RecordInput, StateOperation, StateRegistration, StateStrategy, Store, StoreConfig,
};
use serde_json::json;
use tempfile::TempDir;

fn create_store(dir: &TempDir) -> Store {
    Store::create(StoreConfig {
        path: dir.path().join("store"),
        blob_cache_size: 1000,
        create_if_missing: true,
    })
    .unwrap()
}

/// Benchmark state reconstruction with varying chain depths
fn bench_state_reconstruction(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_reconstruction");

    for chain_depth in [10, 100, 500, 1000] {
        group.bench_with_input(
            BenchmarkId::new("chain_depth", chain_depth),
            &chain_depth,
            |b, &depth| {
                let dir = TempDir::new().unwrap();
                let store = create_store(&dir);

                store
                    .register_state(StateRegistration {
                        id: "items".to_string(),
                        strategy: StateStrategy::AppendLog {
                            delta_snapshot_every: 10000, // No snapshots during bench
                            full_snapshot_every: 1000,
                        },
                        initial_value: None,
                    })
                    .unwrap();

                // Build chain
                for i in 0..depth {
                    store
                        .update_state(
                            "items",
                            StateOperation::Append(format!("{}", i).into_bytes()),
                        )
                        .unwrap();
                }

                b.iter(|| {
                    black_box(store.get_state("items").unwrap());
                });
            },
        );
    }

    group.finish();
}

/// Benchmark state reconstruction with snapshots
fn bench_state_with_snapshots(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_with_snapshots");

    // Fixed total operations, varying snapshot frequency
    let total_ops = 1000;

    for snapshot_every in [10, 50, 100, 500] {
        group.bench_with_input(
            BenchmarkId::new("snapshot_every", snapshot_every),
            &snapshot_every,
            |b, &snap_freq| {
                let dir = TempDir::new().unwrap();
                let store = create_store(&dir);

                store
                    .register_state(StateRegistration {
                        id: "items".to_string(),
                        strategy: StateStrategy::AppendLog {
                            delta_snapshot_every: snap_freq,
                            full_snapshot_every: 10,
                        },
                        initial_value: None,
                    })
                    .unwrap();

                // Build chain with periodic snapshots
                for i in 0..total_ops {
                    if i > 0 && i % snap_freq as usize == 0 {
                        // Create snapshot
                        let current = store.get_state("items").unwrap().unwrap();
                        store
                            .update_state("items", StateOperation::Snapshot(current))
                            .unwrap();
                    } else {
                        store
                            .update_state(
                                "items",
                                StateOperation::Append(format!("{}", i).into_bytes()),
                            )
                            .unwrap();
                    }
                }

                b.iter(|| {
                    black_box(store.get_state("items").unwrap());
                });
            },
        );
    }

    group.finish();
}

/// Benchmark record append
fn bench_record_append(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let store = create_store(&dir);

    c.bench_function("record_append", |b| {
        b.iter(|| {
            let input = RecordInput::json("event", &json!({"data": "test"})).unwrap();
            black_box(store.append(input).unwrap());
        });
    });
}

/// Benchmark blob operations
fn bench_blob_store(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let store = create_store(&dir);

    let content: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();

    c.bench_function("blob_store_10kb", |b| {
        b.iter(|| {
            black_box(store.store_blob(&content, "application/octet-stream").unwrap());
        });
    });
}

/// Benchmark with historical records (untraversed)
fn bench_with_history(c: &mut Criterion) {
    let mut group = c.benchmark_group("with_history");

    for history_size in [100, 1000, 10000] {
        group.bench_with_input(
            BenchmarkId::new("history_records", history_size),
            &history_size,
            |b, &size| {
                let dir = TempDir::new().unwrap();
                let store = create_store(&dir);

                // Create historical records (untraversed)
                for i in 0..size {
                    let input = RecordInput::json("history", &json!({"seq": i})).unwrap();
                    store.append(input).unwrap();
                }

                // Register state AFTER history
                store
                    .register_state(StateRegistration {
                        id: "current".to_string(),
                        strategy: StateStrategy::AppendLog {
                            delta_snapshot_every: 50,
                            full_snapshot_every: 10,
                        },
                        initial_value: None,
                    })
                    .unwrap();

                // Add some state updates
                for i in 0..100 {
                    store
                        .update_state(
                            "current",
                            StateOperation::Append(format!("{}", i).into_bytes()),
                        )
                        .unwrap();
                }

                // Benchmark reading current state (should not traverse history)
                b.iter(|| {
                    black_box(store.get_state("current").unwrap());
                });
            },
        );
    }

    group.finish();
}

/// Benchmark Arc-based state reads — tests the get_state_arc zero-copy path.
/// Compare against bench_state_reconstruction (which uses get_state and clones).
fn bench_state_reconstruction_arc(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_reconstruction_arc");

    for chain_depth in [10, 100, 500, 1000] {
        group.bench_with_input(
            BenchmarkId::new("chain_depth", chain_depth),
            &chain_depth,
            |b, &depth| {
                let dir = TempDir::new().unwrap();
                let store = create_store(&dir);

                store
                    .register_state(StateRegistration {
                        id: "items".to_string(),
                        strategy: StateStrategy::AppendLog {
                            delta_snapshot_every: 10000, // No snapshots during bench
                            full_snapshot_every: 1000,
                        },
                        initial_value: None,
                    })
                    .unwrap();

                for i in 0..depth {
                    store
                        .update_state(
                            "items",
                            StateOperation::Append(format!("{}", i).into_bytes()),
                        )
                        .unwrap();
                }

                b.iter(|| {
                    black_box(store.get_state_arc("items").unwrap());
                });
            },
        );
    }

    group.finish();
}

/// Benchmark index rebuild — exercises both the log iterator and the
/// index's batched insertion path. This is the store-open critical path.
fn bench_index_rebuild(c: &mut Criterion) {
    use chronicle::{RecordIndex, RecordLog};

    let mut group = c.benchmark_group("index_rebuild");

    for record_count in [100, 1000, 5000] {
        group.bench_with_input(
            BenchmarkId::new("records", record_count),
            &record_count,
            |b, &count| {
                // Create a log with N records
                let dir = TempDir::new().unwrap();
                let log_path = dir.path().join("bench.log");
                let idx_path = dir.path().join("bench.idx");
                let log = RecordLog::open(&log_path).unwrap();
                for i in 0..count {
                    // Vary record type to exercise type_index, and add some
                    // causation links to exercise caused_by_index
                    let rtype = if i % 3 == 0 { "type_a" } else if i % 3 == 1 { "type_b" } else { "type_c" };
                    let mut input = RecordInput::json(rtype, &json!({"i": i})).unwrap();
                    if i > 0 {
                        input = input.with_caused_by(vec![chronicle::RecordId(i as u64)]);
                    }
                    log.append(input, chronicle::BranchId(1), chronicle::Sequence(i as u64 + 1))
                        .unwrap();
                }

                b.iter(|| {
                    let index = RecordIndex::rebuild_from_log(&idx_path, &log).unwrap();
                    black_box(index.count());
                });
            },
        );
    }

    group.finish();
}

/// Benchmark record log iteration — exercises RecordIterator, which is used
/// during index rebuild on store open. Critical for startup time.
fn bench_log_iteration(c: &mut Criterion) {
    use chronicle::RecordLog;

    let mut group = c.benchmark_group("log_iteration");

    for record_count in [100, 1000, 10000] {
        group.bench_with_input(
            BenchmarkId::new("records", record_count),
            &record_count,
            |b, &count| {
                // Create a log with N records
                let dir = TempDir::new().unwrap();
                let log_path = dir.path().join("bench.log");
                let log = RecordLog::open(&log_path).unwrap();
                for i in 0..count {
                    let input = RecordInput::json("bench", &json!({"i": i})).unwrap();
                    log.append(input, chronicle::BranchId(1), chronicle::Sequence(i as u64 + 1))
                        .unwrap();
                }

                b.iter(|| {
                    let mut n = 0u64;
                    for result in log.iter() {
                        let (_, record) = result.unwrap();
                        n = n.wrapping_add(record.id.0);
                    }
                    black_box(n);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_state_reconstruction,
    bench_state_reconstruction_arc,
    bench_state_with_snapshots,
    bench_record_append,
    bench_blob_store,
    bench_with_history,
    bench_log_iteration,
    bench_index_rebuild,
);

criterion_main!(benches);
