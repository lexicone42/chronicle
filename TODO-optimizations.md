# Chronicle Optimization Opportunities

Identified during security audit / code review session (April 2026).
These need discussion with maintainers before implementation — they
touch core data paths and may affect API contracts.

## High Impact

### 1. JSON parse-reserialize on every Append
**File:** `src/state/operations.rs:41-76`

Every `Append` to an AppendLog state parses the entire JSON array,
pushes one item, and re-serializes everything. For a 10K-item state,
this is O(n) work per append.

**Options:**
- Drop-in `simd-json` for 2-4x faster parse/serialize
- Raw JSON byte concatenation for O(1) appends
- Switch state storage to MessagePack (WAL already uses it)

### 2. Iterator double-read
**File:** `src/records/log.rs:471-500`

`RecordIterator` reads each record twice — once to return it, once to
find the next offset. During index rebuild (O(N) scan on startup), this
doubles all I/O.

**Fix:** Have `read_record` return `(Record, u64)` with the post-read
file position. Small signature change, eliminates the second read.

### 3. State cache clones full Vec on hit
**File:** `src/state/manager.rs:330-356`

Every `get_state()` cache hit copies the entire value. A 5MB state
means a 5MB memcpy on every read.

**Fix:** Use `Arc<Vec<u8>>` in the cache. Cache hit becomes a pointer
copy instead of a full clone. Requires API signature changes.

## Medium Impact

### 4. Index rebuild lock contention
**File:** `src/records/index.rs:84-119`

5+ write lock acquisitions per record during rebuild. For 1M records
that's 5M+ lock ops at startup.

**Fix:** Batch inserts behind a single lock, or use `DashMap`.

### 5. find_max_id scans entire log on every open
**File:** `src/records/log.rs:384-458`

O(N) scan to find the highest record ID. Could persist the last ID
in a small metadata file (32 bytes) and only scan if it's missing.

### 6. String clones in type index
**File:** `src/records/index.rs:98-102`

`record_type.to_string()` on every record during rebuild, even though
there are typically few unique types. Clone only on first insertion.

### 7. Chain reconstruction cloning
**File:** `src/state/manager.rs:370-415`

Operations are cloned during chain walk. For large chains with big
payloads, this is significant. `Arc<StateOperation>` or `Cow` could help.

## Already Optimized

- `crc32fast` — uses hardware CRC32 on x86-64
- `blake3` — SIMD-optimized internally (AVX2/SSE4.1)
- `Vec::with_capacity` — pre-allocated in record deserialization
- `BTreeMap` for `(BranchId, Sequence)` entries — good for range queries
- `parking_lot` — faster than std locks

## Data Structure Opportunities (needs profiling)

- **Roaring bitmaps** for type index — more compact, faster intersections.
  Current HashMap<String, Vec<RecordId>> is fine for point lookups but
  poor for set operations (e.g., "type X intersect branch Y").
- **Arena allocation** for record payloads during chain reconstruction —
  avoid per-operation heap allocations during the walk-and-apply loop.

## Notes

- simd-json is the highest ROI change (near drop-in, biggest speedup)
- Iterator double-read is the easiest fix (small code change)
- Arc<Vec<u8>> cache is the most impactful for read-heavy workloads
- These should be benchmarked with `benches/performance.rs` before/after
