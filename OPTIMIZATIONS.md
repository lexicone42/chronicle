# Chronicle Optimization Results

Measured on branch `feature/optimizations` against the `dev` baseline.

## Summary

| Optimization | Benchmark | Baseline | After | Speedup |
|--------------|-----------|----------|-------|---------|
| Iterator double-read fix | `log_iteration/1000` | 25.17 ms | 12.84 ms | **1.96x** |
| Iterator double-read fix | `log_iteration/10000` | 253.17 ms | 132.29 ms | **1.91x** |
| Arc<Vec<u8>> cache (opt-in) | `state_reconstruction_arc/500` | n/a | 192 ns (vs 279 ns Vec) | **1.45x** |
| Arc<Vec<u8>> cache (opt-in) | `state_reconstruction_arc/1000` | n/a | 252 ns (vs 378 ns Vec) | **1.51x** |
| Index rebuild batching | `index_rebuild/5000` | n/a | 73.4 ms | new bench |

## Changes

### 1. Iterator double-read (`src/records/log.rs`)

**Problem:** `RecordIterator::next()` called `read_at(offset)` to return the
record, then re-acquired the file lock, re-seeked, re-read the record, and
called `stream_position()` just to find the next record's offset.

**Fix:** Added `read_at_with_end(offset)` which returns `(Record, u64)` —
both the record and the post-read file position. The iterator now uses this
directly.

**Impact:** Nearly 2x speedup on record log iteration, which is the
startup-critical path (`RecordIndex::rebuild_from_log` walks the entire log).
For a 1M-record store, this drops index rebuild from ~25s to ~13s.

### 2. Arc<Vec<u8>> state cache (`src/state/manager.rs`, `src/store.rs`)

**Problem:** `StateManager::get_state()` returned `Vec<u8>` by cloning the
cached value on every cache hit. For a 5MB state, that's a 5MB memcpy on every
read, regardless of whether the caller mutates the result.

**Fix:** `CachedState::value` is now `Arc<Vec<u8>>`. New API:
- `get_state_arc()` returns `Arc<Vec<u8>>` — cache hits are O(1) refcount
  increments regardless of state size
- `get_state()` is unchanged (still returns `Vec<u8>`) and clones out of the
  Arc directly; the common-case path doesn't incur Arc overhead

**Why the opt-in design:** There are 200+ callers of `get_state()` across
tests and internal code. Changing the return type would cascade everywhere.
The Arc version is available for performance-sensitive consumers
(benchmarks, internal chains, future exo-self reads).

**Impact:** At depth 1000, `get_state_arc` is 1.51x faster than `get_state`.
The speedup grows with state size.

**Design note:** The first version had `get_state` call `get_state_arc`
internally and clone out. This introduced a ~10% regression because every
`get_state` call went through an extra Arc allocation + pattern match. The
fix was to give `get_state` its own direct path. Lesson:
don't layer a fast path on top of a slow path; give them sibling
implementations that share lookups but return the right type directly.

### 3. Batched index rebuild (`src/records/index.rs`)

**Problem:** `rebuild_from_log` called `index.add()` for each record, which
acquires up to 5 write locks (one per sub-index). For 1M records that's 5M+
lock acquisitions during startup.

**Fix:** `rebuild_from_log` now builds all maps locally without touching the
locks, then swaps them in under single-lock acquisitions per sub-index.

**Impact:** Reduces lock acquisitions during rebuild from O(N×5) to O(5).
Not directly benchmarked as a before/after (index rebuild was not in the
original suite), but the batched path clearly eliminates the contention.

### 4. Batched causation inserts on single-record add (`src/records/index.rs`)

**Problem:** `RecordIndex::add()` acquired the `caused_by_index` lock once
per causation link. A record with 10 `caused_by` entries = 10 lock
acquisitions.

**Fix:** Acquire the lock once if the slice is non-empty, then do all
inserts. Same fix for `linked_to` and for `rebuild_causation_for`.

### 5. Type index string clone optimization (`src/records/index.rs`)

**Problem:** `add()` always cloned `record_type` as a `String` to insert
into the type index, even when the type already existed (the common case).

**Fix:** Look up by `&str` first. Only clone on insertion of a new type.

**Impact:** Reduces one allocation per append when the type is not new —
which is essentially always, after startup.

## Observations and Caveats

### record_append noise

The `record_append` benchmark measured ~51 µs on the initial baseline and
~64 µs on later runs. Investigation showed:

- Stashing all optimization changes did not restore the 51 µs baseline
- The regression appears when the benchmark suite is larger (I added new
  benchmarks that run before `record_append`)
- `record_append` is fsync-dominated, and fsync latency depends on dirty
  page state from prior benchmarks

**Conclusion:** The apparent regression is a benchmark ordering artifact, not
a real performance loss. Running `record_append` in isolation on both branches
would give a cleaner comparison, but would be outside the scope of this
optimization pass.

### Small-state benchmarks show no Arc speedup

`state_reconstruction_arc/10` and `state_reconstruction_arc/100` are roughly
tied with their Vec counterparts. The Arc win only shows up at chain depth
≥500 where the state is ~2+ KB. This is expected: the Arc optimization
eliminates a memcpy proportional to state size, so it can't help when the
state is small enough that the memcpy is in the noise.

For Chronicle's intended workloads — exo-self traces, agent conversations,
audit trails — states are typically much larger than 2 KB, so the benefit
should be substantial in practice.

### Not done in this pass

The TODO document (`TODO-optimizations.md`) lists three more optimizations
that weren't tackled:

- **JSON parse-reserialize on Append** — requires a design decision
  (simd-json drop-in vs raw byte concatenation vs MessagePack). Affects the
  on-disk format unless we pick simd-json.
- **find_max_id persistent metadata** — O(N) → O(1) store open, but adds a
  new metadata file to the format.
- **Chain reconstruction cloning** — would need `Arc<StateOperation>` or
  `Cow`; moderate refactor.

These should be considered during the schema freeze review.
