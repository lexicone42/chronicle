# Chronicle On-Disk Format

This document describes the on-disk format Chronicle writes to its store
directory. It's intended as a reference for:

- Readers who want to understand or audit the wire format
- External tools that want to read Chronicle stores without linking the crate
- Future maintainers deciding whether a change is backwards-compatible
- Downstream consumers (like exo-self) that need to know what's stable

## Overview

A Chronicle store is a directory containing the following files:

```
<store>/
├── MANIFEST                 # Store identity and format version gate
├── LOCK                     # Exclusive access lock (not a data file)
├── records.log              # Append-only binary log of all records
├── wal.log                  # Write-ahead log for crash recovery
├── state.bin                # Per-state chain head metadata (MessagePack)
├── branches.bin             # Branch metadata (MessagePack)
└── blobs/
    └── <xx>/<full_hash>     # Content-addressed blob files (BLAKE3)
```

There is no `records.idx` on disk; the record index is rebuilt from
`records.log` on startup (see *Computed state* below).

## Version numbers

**Chronicle does not have a single "store version" number.** Each file has
its own magic bytes and version byte, and they move independently. The table
below shows current values:

| File | Magic | Version |
|------|-------|---------|
| `MANIFEST` | `RST\0` | 2 |
| `records.log` (per-record header) | `REC\0` | 2 |
| `state.bin` | `STI\0` | 2 |
| `branches.bin` | `BRI\0` | 1 |
| `wal.log` | `WAL\0` | 1 |
| `blobs/.../<hash>` | `BLB\0` | 1 |

When we say "Chronicle v2," we mean the current collective state of these
files — not that every file is at version 2. This distinction matters for
migration: a version bump in `records.log` can happen without touching
`branches.bin`, and vice versa.

## File formats

Integers are little-endian. `var` indicates variable-length data whose length
is given by the immediately preceding field.

### `MANIFEST`

```
Offset  Size  Field
0       4     Magic: "RST\0"
4       1     Version: 0x02
```

Total size: 5 bytes. No checksum. Rewritten whenever the store is created.
Opening a store with an unknown version fails with `InvalidFormat`.

### `records.log`

An append-only sequence of records. Each record is length-delimited by its
internal field layout (there is no per-record framing length; parsers walk
the fields to know where the next record starts).

Per-record layout:

```
Offset            Size       Field                         Encoding
0                 4          Magic: "REC\0"                ASCII
4                 1          Version: 0x02                 u8
5                 1          Flags                         u8 (reserved, 0x00)
6                 8          Record ID                     u64
14                8          Sequence (per-branch)         u64
22                8          Branch ID                     u64
30                8          Timestamp (microseconds)      i64
38                2          Type length (bytes)           u16
40                var        Type string                   UTF-8 (u16-prefixed)
...               1          Encoding                      u8: 0=JSON, 1=MsgPack, 2=Raw
...               4          Payload length                u32
...               var        Payload bytes                 raw
...               2          Caused-by count               u16
...               var        Caused-by IDs                 u64 each
...               2          Linked-to count               u16
...               var        Linked-to IDs                 u64 each
...               4          Checksum                      u32 (CRC32)
```

**Checksum coverage** (v2): CRC32-fast over **id, sequence, branch, timestamp,
type_bytes, encoding, payload, caused_by IDs, linked_to IDs**. v1 only
covered the payload bytes; v2 extends coverage to metadata. The checksum is
NOT computed over the magic, version, or flags bytes.

**Size limits:**
- Payload: `MAX_PAYLOAD_SIZE` = 256 MiB (u32 max value further constrained)
- Type string: 65,535 bytes (u16 max)
- Caused-by / linked-to counts: 65,535 each (u16 max)

**Sync policy:** By default `records.log` is fsynced every 100 writes. This
is tunable via `open_with_sync_interval()`. Use `sync_interval=1` for
strongest durability (fsync every write), higher values for throughput.

**Open-time scan:** On `RecordLog::open`, Chronicle calls `find_max_id()`
which sequentially walks the log to find the highest record ID. This is
O(N) and dominates open time for large stores. The result is not persisted.
If the log is truncated or corrupted mid-record, the scan stops and
`next_id` is set to `max_found.saturating_add(1)` — any records after the
corruption are unreachable and will be overwritten by subsequent appends
(their file space is leaked).

### `wal.log` (version 1)

A write-ahead log used for crash recovery. Operations are written here
*before* they modify the main store. On `Store::open`, pending entries are
replayed.

Header:

```
Offset  Size  Field
0       4     Magic: "WAL\0"
4       1     Version: 0x01
```

Per-entry layout (after header):

```
Offset  Size  Field                                Encoding
0       4     Entry length                         u32
4       var   Entry                                MessagePack (rmp_serde)
...     4     Checksum                             u32 (CRC32 of entry bytes)
```

Entry schema (MessagePack-encoded):

```rust
struct WalEntry {
    seq: u64,
    status: enum { Pending, Committed, RolledBack },
    operation: WalOperation,
    timestamp: u64,  // Unix seconds
}

enum WalOperation {
    AppendRecord { record_type, payload, encoding, caused_by, linked_to,
                   branch_id, log_fence },
    UpdateState { state_id, operation_data, branch_id, log_fence },
    StoreBlob { content, content_type },
    CreateBranch { name, from },
    CommitMarker,
}
```

**Idempotent replay via `log_fence`.** Each `AppendRecord`/`UpdateState`
entry stores the record log file size *before* the intended write. On
replay, if `records.log` has grown past that offset, Chronicle assumes the
write already completed and skips it. This is how replay avoids
duplicating records after a crash-then-restart. The fence value is *not*
checksummed beyond the entry-level CRC32, and a corrupted fence would
break idempotency.

**Size limit:** 16 MiB per entry (`Corruption` error on exceed).

**Sync policy:** `log()` buffers entries but does not fsync. `commit()`
writes a marker but also does not fsync. `sync()` explicitly fsyncs the
WAL. This is intentional: the fence-based replay is idempotent, so losing
a commit marker on crash only causes redundant replay, not corruption.

### `state.bin`

In-memory state chain metadata, serialized on `save()`.

```
Offset  Size  Field
0       4     Magic: "STI\0"
4       1     Version: 0x02
5       8     Encoded length                       u64
13      var   Encoded StateIndex                   MessagePack
```

**No checksum.** If `save()` is interrupted mid-write, corruption could go
undetected until `load()` fails to deserialize.

**Size limit:** 64 MiB (`PayloadTooLarge` on exceed).

`StateIndex` structure (MessagePack):

```rust
struct StateIndex {
    heads: HashMap<(BranchId, String), StateChainHead>,
    strategies: HashMap<String, StateStrategy>,
}

struct StateChainHead {
    head_offset: u64,                     // offset in records.log
    ops_since_delta_snapshot: u64,
    delta_snapshots_since_full: u64,
    last_delta_snapshot_offset: Option<u64>,
    last_full_snapshot_offset: Option<u64>,
    has_non_append_since_snapshot: bool,
    item_count: usize,
}
```

### `branches.bin` (version 1)

```
Offset  Size  Field
0       4     Magic: "BRI\0"
4       1     Version: 0x01
5       8     Current branch ID                    u64
13      8     Encoded length                       u64
21      var   Encoded BranchIndex                  MessagePack
```

**No checksum.** Same caveat as `state.bin`: partial writes are
undetectable.

**Size limit:** 64 MiB.

`BranchIndex` structure:

```rust
struct BranchIndex {
    branches: HashMap<BranchId, Branch>,
    name_to_id: HashMap<String, BranchId>,
    next_id: u64,
}

struct Branch {
    id: BranchId,            // u64
    name: String,
    head: Sequence,          // u64
    parent: Option<BranchId>,
    branch_point: Option<Sequence>,
    created: Timestamp,      // i64 microseconds
}
```

### `blobs/<shard>/<hex_hash>` (version 1)

Content-addressed blob storage. The shard is the first two hex characters
of the BLAKE3 hash; `<hex_hash>` is the full 64-character hex string.

```
Offset       Size  Field                              Encoding
0            4     Magic: "BLB\0"                     ASCII
4            1     Version: 0x01                      u8
5            2     Content-type length                u16
7            var   Content-type string                UTF-8
...          8     Content length                     u64
...          var   Content bytes                      raw
...          4     Checksum                           u32 (CRC32 of content)
```

**Hash function:** BLAKE3 (changed from SHA-256 in v2 of the overall
format). The blob filename is the hex-encoded hash.

**Hash verification on read:** After loading, Chronicle re-hashes the
content with BLAKE3 and compares to the requested hash. Mismatch raises
`HashMismatch`.

**Size limit:** 1 GiB (`PayloadTooLarge`).

**Checksum scope:** CRC32 covers only the content bytes, not the header
or content-type. The hash check catches content corruption anyway; the
CRC32 is redundant defense.

**Deduplication:** Storing the same content twice is a no-op after the
first store; the file already exists.

### `LOCK`

Empty file whose exclusive filesystem lock grants a process exclusive
access to the store. Not a data file. Chronicle assumes single-writer
single-reader access — opening a locked store returns `StoreError::Locked`.

## Computed (not persisted) state

The following are reconstructed on `Store::open` and never written to disk:

- **RecordIndex** — all record lookups (by ID, by type, causation, etc.).
  Rebuilt by walking `records.log` once. Cost: O(N) in record count. For a
  1M-record store on SSD, typically 1-3 seconds. The
  `feature/optimizations` branch reduces lock overhead during rebuild.
- **RecordLog next_id** — scanned from the log via `find_max_id()`. Same
  cost as the index rebuild.

Both are deliberate trade-offs: slower startup for faster writes (no
persistent-index sync overhead).

## Known gaps and caveats

The following are documented intentionally so external consumers can
account for them:

1. **Decoupled version numbers.** As noted above, "v2" is a collective
   label. A parser that wants to read a Chronicle store must check each
   file's magic+version independently and handle any mixture.

2. **No checksum in `state.bin` or `branches.bin`.** A crash mid-`save()`
   could produce a partially written file that fails deserialization but
   is otherwise silently corrupt. This is low-risk in practice (`save()`
   is infrequent and atomic writes are not attempted) but worth knowing.

3. **`log_fence` is not independently protected.** The WAL entry CRC32
   covers the whole entry including the fence, so typical bit-flip
   corruption is detected. But a higher-level corruption that produces a
   valid-looking entry with a wrong fence would break idempotent replay.

4. **RecordId overflow is unguarded at `u64::MAX`.** On debug builds, the
   next append panics. On release builds, the ID wraps to 0, potentially
   creating a duplicate. This is 584 years of continuous 1 GHz appends
   away, but it's unguarded.

5. **`find_max_id` stops at corruption.** If the log is corrupted
   mid-file, everything after the corruption is effectively lost and may
   be overwritten by subsequent appends. There is no "last known good
   offset" recovery hint persisted anywhere.

6. **State operations are JSON, not MessagePack.** `StateUpdateRecord` is
   serialized via `serde_json` inside the record log's raw payload.
   That's the source of the `apply_operation` parse-reserialize O(n)
   cost. It's also the reason state chain records are partially
   human-readable in a hex dump.

7. **Type strings and content-type strings have no explicit length
   limit beyond u16 max** (65 KiB). Truncated UTF-8 is handled lossily
   (`String::from_utf8_lossy`) rather than erroring.

## Format stability

Chronicle is pre-1.0 and the format is **not yet stable**. External
consumers should:

- Pin to a specific Chronicle version
- Not assume forward compatibility
- Not assume a v1 store can be read by a v2 library (there is no migrator)
- Re-verify assumptions on every Chronicle upgrade

Once the format is frozen (decision still pending), downgrade tools and a
migration story should be added.

## Open questions

These are the things that need to be decided before calling the format
"stable":

- **State operation encoding.** Should state updates continue using
  `serde_json`, switch to `simd-json`, switch to MessagePack, or use
  incremental JSON byte concatenation? (See `TODO-optimizations.md`.)
  Affects on-disk layout if we change away from JSON.

- **Persistent `next_id` anchor.** Should we add a small metadata file
  (`records.meta`) caching the last record ID so we don't have to scan
  the whole log on open? Additive change, not breaking.

- **Checksums for `state.bin` / `branches.bin`.** Should these small
  metadata files get CRC32 headers for corruption detection? Low cost,
  adds bytes to the format.

- **Version synchronization policy.** Should all components' version
  numbers move together, or stay independent? Current practice is
  independent — codifying that would help future maintainers.

- **Downgrade / migration story.** What happens when we do need to bump
  a version? Is there a v1→v2 converter? Should there be?
