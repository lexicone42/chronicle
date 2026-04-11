# Schema Review (April 2026)

This is the companion to `FORMAT.md` — while FORMAT.md describes what the
format *is*, this document describes what decisions are still open and
what I'd recommend before freezing the format for downstream consumers
like exo-self.

## Summary

The format is in a "mostly frozen" state. The security hardening and
optimization passes didn't change anything structural — they added size
guards, checksum coverage, and (in `dev`) a BLAKE3 swap for content
addressing. The remaining open questions are smaller than they first
appeared.

**Recommendation: freeze the format now** with the three small additive
changes below. The larger open question (state operation encoding) can be
deferred because it has a clean migration path: the wrapper record type
(`"state_update"`) can hold either a JSON or MessagePack payload, and the
encoding byte in the record header already tells us which.

## What I'd change before freezing

### 1. Synchronize version numbers — or explicitly don't

**Current state:** `MANIFEST`, `records.log`, and `state.bin` are at
version 2. `branches.bin`, `wal.log`, and `blobs/...` are at version 1.
The "v2" label is collective, not literal.

**Two options:**

- **A) Bump everything to 2.** Changes four lines of code (version
  constants) and makes "v2" mean what it looks like it means. Downside:
  a v1-v2-upgraded store's `branches.bin` would look different from a
  fresh v2 store's, even though the layout is identical.

- **B) Codify that version numbers are independent.** Add a comment in
  each file's version constant saying "this version moves independently
  from other file versions" and document the current matrix in FORMAT.md
  (already done).

**Recommendation:** **Option B.** It's honest about what we actually do.
Bumping version numbers just to make them match is cosmetic and obscures
the fact that `branches.bin` hasn't actually changed format.

**Effort:** A comment per constant. Already documented in FORMAT.md.

### 2. Add CRC32 to `state.bin` and `branches.bin`

**Problem:** A crash during `save()` could leave a partially-written
index file. On next open, `load()` will fail to deserialize — which is
noisy but at least a clear error. But a more subtle corruption (e.g.,
flipped bits from a filesystem hiccup) could produce a plausible-looking
file that misses entries or corrupts head offsets.

**Fix:** Wrap the MessagePack payload in a CRC32 envelope:

```
Offset  Size  Field
0       4     Magic
4       1     Version (bump to 3 for state.bin, 2 for branches.bin)
5       8     Encoded length
13      var   Encoded index
...     4     CRC32 of encoded index (new)
```

**Effort:** ~30 lines each. Requires version bump and a migration check
on load (accept either old or new version for one release).

**Recommendation:** **Do it** — small, cheap, isolates a real failure
mode. Include it in the freeze.

### 3. Persistent `next_id` anchor for `records.log`

**Problem:** `find_max_id()` does an O(N) scan of the entire record log
on every `Store::open`. For a 1M-record store that's 1-3 seconds; for
10M records it's tens of seconds. This is the biggest startup cost.

**Fix:** Add `records.meta`:

```
Offset  Size  Field
0       4     Magic: "RMT\0"
4       1     Version: 1
5       8     Last known max RecordId
13      8     Last known RecordLog file size (sanity check)
21      4     CRC32 of above fields
```

On open: if `records.meta` exists and its file-size matches the actual
`records.log` file size, use `max_id + 1` directly. If the sanity check
fails (file is longer than expected), fall back to the scan.

On close/sync: write `records.meta` atomically.

**Effort:** ~100 lines. Easy to get right because the fallback is just
the current behavior.

**Recommendation:** **Do it.** Startup cost is the only meaningful
user-visible overhead in Chronicle right now, and this kills it
completely for well-behaved shutdowns while keeping the scan as a safety
net for crashes.

## What I'd explicitly defer

### State operation encoding

**Current:** `StateUpdateRecord` is serialized to JSON and stored as the
raw payload of a `"state_update"` record. Every Append goes through a
full JSON parse-modify-serialize cycle on the accumulated state.

**Why defer:**

1. The record header already has an `encoding` byte (0=JSON, 1=MsgPack,
   2=Raw). Switching state updates to MessagePack just means writing a
   different encoding byte — no format restructuring required.
2. The hard work (O(n)→O(1) incremental appends) is orthogonal to the
   encoding choice. Even with MessagePack, we'd still be reparsing and
   rewriting the whole array on every append.
3. This decision has enough design options (simd-json, msgpack, byte
   concatenation, operation deltas) that it should be discussed rather
   than picked unilaterally.

**What to do:** Leave the current encoding alone. Document that the
record encoding byte means state updates can switch encodings in a
future version *without a format break*, because old records carry their
encoding with them.

### Finer-grained WAL fence integrity

**Current:** The WAL entry CRC32 covers the fence as part of the whole
entry, which catches typical corruption. A pathological corruption that
produces a valid-looking entry with a wrong fence is theoretically
possible but practically unreachable.

**Why defer:** It's theoretical. The real risk would be hitting it,
which we have no evidence of. And the fix (separate hash over fence +
operation data) would add complexity.

**What to do:** Note the caveat in FORMAT.md (done) and move on.

## Testing the format

Before declaring the format frozen, I'd add two specific tests:

1. **Golden files.** Write a handful of known stores (one with each
   strategy, one with branches, etc.) and commit their raw bytes as
   test fixtures. A test that tries to open these on every commit
   catches any unintended format change.

2. **Format round-trip test.** For each file type (MANIFEST, record
   log, state index, branches index, blob, WAL), write a small valid
   file with known bytes, then parse it and assert the parsed struct
   matches. This catches silent changes to field order or size.

These aren't blockers for the freeze but they're the long-term guardrails
that prevent format drift.

## Recommended next steps

1. **Decide:** Do options 1, 2, 3 above go in before the freeze, or after?
2. **Implement:** Whichever the answer is.
3. **Tag:** Once decisions are in, tag `v2-frozen` on the fork so
   downstream consumers have a specific commit to pin to.
4. **Build exo-self** against `v2-frozen`.
5. **Iterate** on exo-self knowing the storage layer won't shift under it.

My lean: do all three (version doc, CRC32 on small indices, `records.meta`)
in a single commit, tag the freeze, then start exo-self. Estimated effort
for the three changes: one focused session.
