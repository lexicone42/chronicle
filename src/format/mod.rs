//! On-disk format constants — the single source of truth for Chronicle's
//! wire format.
//!
//! This module centralizes every magic-byte sequence, version byte, and
//! size limit that Chronicle writes to disk. If a constant controls what
//! bytes end up in a file, it lives here.
//!
//! See `FORMAT.md` at the repository root for the full format specification.
//!
//! # Why a central module?
//!
//! Chronicle's on-disk format was originally defined inline with the parser
//! for each file type (`records/log.rs`, `blobs/storage.rs`, etc.). That
//! worked when the format was small, but as it grew, schema drift became
//! possible without any compile-time or test-time check. By pulling all
//! format constants into one module:
//!
//! - You can read the whole schema by reading this file.
//! - Changing any constant requires editing this module, which is harder
//!   to do accidentally.
//! - The `CURRENT_VERSIONS` matrix is asserted by a smoke test, so an
//!   unintentional version bump fails loudly.
//! - Cross-referencing with `FORMAT.md` is easier — one file, one spec.
//!
//! # Version independence
//!
//! Chronicle does **not** have a single "store version" number. Each file
//! on disk has its own independent version byte, and they move on
//! different schedules. When a change to `records.log` doesn't affect
//! `branches.bin`, only the record log version bumps. See
//! `CURRENT_VERSIONS` below for the current matrix, and `FORMAT.md` §
//! "Version numbers" for the rationale.

/// Manifest file format — gates the store against incompatible Chronicle
/// versions. See `FORMAT.md` § MANIFEST.
pub mod manifest {
    /// Magic bytes at offset 0. ASCII "RST\0" (Record Store).
    pub const MAGIC: &[u8; 4] = b"RST\0";

    /// Current version byte. Bumped when the store as a whole becomes
    /// incompatible with prior Chronicle readers (not merely when one
    /// component's format changes).
    pub const VERSION: u8 = 2;
}

/// Record log (`records.log`) — append-only binary log of every record.
/// See `FORMAT.md` § records.log.
pub mod record_log {
    /// Magic bytes at the start of every record. ASCII "REC\0".
    pub const MAGIC: &[u8; 4] = b"REC\0";

    /// Current record format version.
    ///
    /// v1: CRC32 covered payload bytes only.
    /// v2: CRC32 covers all record fields (id, seq, branch, timestamp,
    ///     type, encoding, payload, caused_by, linked_to). This is a
    ///     format break from v1 — the bytes written are the same layout
    ///     but the checksum calculation is different, so a v2 reader
    ///     cannot accept v1 records and vice versa.
    pub const VERSION: u8 = 2;

    /// Maximum payload size in a single record. Reading a record whose
    /// payload length exceeds this raises `PayloadTooLarge`. This prevents
    /// unbounded allocations from malformed files.
    pub const MAX_PAYLOAD_SIZE: u32 = 256 * 1024 * 1024;

    /// Default sync interval for the record log (number of writes between
    /// fsyncs). 0 means "sync every write"; 100 is a good
    /// durability/throughput balance.
    pub const DEFAULT_SYNC_INTERVAL: u64 = 100;
}

/// Blob storage (`blobs/<shard>/<hash>`) — content-addressed binary blobs.
/// See `FORMAT.md` § blobs.
pub mod blob {
    /// Magic bytes at offset 0 of every blob file. ASCII "BLB\0".
    pub const MAGIC: &[u8; 4] = b"BLB\0";

    /// Current blob file version. Note: still v1 even as the overall store
    /// moved to v2 — the blob format hasn't needed to change since
    /// inception. When the store moved from SHA-256 to BLAKE3, the blob
    /// *contents* didn't change; only the hash function used to name the
    /// file changed, so the on-disk layout was unaffected.
    pub const VERSION: u8 = 1;

    /// Maximum blob content size. Reading a blob whose content-length
    /// field exceeds this raises `PayloadTooLarge`.
    pub const MAX_SIZE: u64 = 1024 * 1024 * 1024;
}

/// Write-ahead log (`wal.log`) — crash recovery. See `FORMAT.md` § WAL.
pub mod wal {
    /// Magic bytes at offset 0. ASCII "WAL\0".
    pub const MAGIC: &[u8; 4] = b"WAL\0";

    /// Current WAL format version. Moves independently from the other
    /// components.
    pub const VERSION: u8 = 1;

    /// Maximum size of a single WAL entry (serialized MessagePack). Reading
    /// a larger entry raises `Corruption`.
    pub const MAX_ENTRY_SIZE: u32 = 16 * 1024 * 1024;
}

/// State index (`state.bin`) — per-state chain head metadata.
/// See `FORMAT.md` § state.bin.
pub mod state_index {
    /// Magic bytes at offset 0. ASCII "STI\0".
    pub const MAGIC: &[u8; 4] = b"STI\0";

    /// Current state index version.
    ///
    /// v1: initial layout with simpler StateChainHead (no item_count).
    /// v2: StateChainHead gained metadata for delta snapshots + item count.
    /// v3: Added trailing CRC32 over the encoded index for corruption
    ///     detection.
    pub const VERSION: u8 = 3;

    /// Minimum readable version. Readers accept versions in
    /// `MIN_READABLE_VERSION..=VERSION`. v2 is accepted for backwards
    /// compatibility (no CRC32) but writes always produce v3.
    pub const MIN_READABLE_VERSION: u8 = 2;

    /// Maximum size of the encoded state index. Guards against unbounded
    /// allocation when reading potentially corrupted files.
    pub const MAX_SIZE: u64 = 64 * 1024 * 1024;
}

/// Branch index (`branches.bin`) — branch metadata.
/// See `FORMAT.md` § branches.bin.
pub mod branch_index {
    /// Magic bytes at offset 0. ASCII "BRI\0".
    pub const MAGIC: &[u8; 4] = b"BRI\0";

    /// Current branch index version.
    ///
    /// v1: initial layout without a checksum.
    /// v2: adds trailing CRC32 over the encoded index.
    pub const VERSION: u8 = 2;

    /// Minimum readable version. Readers accept v1 (without CRC32) and
    /// v2 (with). Writes always produce v2.
    pub const MIN_READABLE_VERSION: u8 = 1;

    /// Maximum size of the encoded branch index.
    pub const MAX_SIZE: u64 = 64 * 1024 * 1024;
}

/// Records metadata (`records.meta`) — persistent `next_id` anchor that
/// avoids the O(N) `find_max_id` scan on store open.
/// See `FORMAT.md` § records.meta.
pub mod records_meta {
    /// Magic bytes at offset 0. ASCII "RMT\0".
    pub const MAGIC: &[u8; 4] = b"RMT\0";

    /// Current records.meta version.
    pub const VERSION: u8 = 1;
}

/// Current format version matrix for every file Chronicle writes.
///
/// The format smoke test opens a fresh store and asserts that each file's
/// magic+version on disk matches this matrix. Intended as a tripwire:
/// bumping a version constant without updating this matrix (or vice versa)
/// breaks the test loudly.
///
/// Tuple layout: `(logical_name, magic_bytes, version)`.
pub const CURRENT_VERSIONS: &[(&str, &[u8; 4], u8)] = &[
    ("manifest", manifest::MAGIC, manifest::VERSION),
    ("record_log", record_log::MAGIC, record_log::VERSION),
    ("blob", blob::MAGIC, blob::VERSION),
    ("wal", wal::MAGIC, wal::VERSION),
    ("state_index", state_index::MAGIC, state_index::VERSION),
    ("branch_index", branch_index::MAGIC, branch_index::VERSION),
    ("records_meta", records_meta::MAGIC, records_meta::VERSION),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_magic_bytes_are_distinct() {
        let mut seen: Vec<&[u8; 4]> = Vec::new();
        for (name, magic, _) in CURRENT_VERSIONS {
            assert!(
                !seen.contains(&magic),
                "Duplicate magic bytes for {}: {:?}",
                name,
                magic
            );
            seen.push(magic);
        }
    }

    #[test]
    fn all_magic_bytes_are_four_ascii_chars() {
        for (name, magic, _) in CURRENT_VERSIONS {
            // First three bytes should be printable ASCII, last should be NUL
            for (i, &b) in magic[..3].iter().enumerate() {
                assert!(
                    (32..=126).contains(&b),
                    "{}: byte {} is not printable ASCII: {}",
                    name,
                    i,
                    b
                );
            }
            assert_eq!(magic[3], 0, "{}: fourth byte should be NUL", name);
        }
    }

    #[test]
    fn version_sanity() {
        // All versions should be nonzero (v0 is reserved as "invalid")
        for (name, _, version) in CURRENT_VERSIONS {
            assert!(*version > 0, "{}: version should be > 0", name);
        }
    }
}
