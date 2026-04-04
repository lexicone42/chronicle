//! Fuzz target: record log deserialization from arbitrary bytes.
//!
//! Feeds malformed binary data to RecordLog::read_at() to find panics,
//! unbounded allocations, or other crashes in the binary parsing path.
//! This exercises the payload size limits and checksum verification.

#![no_main]

use chronicle::{RecordLog, RecordInput, Sequence, BranchId};
use libfuzzer_sys::fuzz_target;
use std::io::Write;
use tempfile::TempDir;

fuzz_target!(|data: &[u8]| {
    // Skip tiny inputs that can't contain a valid record header
    if data.len() < 8 {
        return;
    }

    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("fuzz.log");

    // Write raw bytes as if they were a record log
    {
        let mut file = std::fs::File::create(&log_path).unwrap();
        file.write_all(data).unwrap();
        file.sync_all().unwrap();
    }

    // Try to open and iterate — must not panic
    if let Ok(log) = RecordLog::open(&log_path) {
        // Try reading at offset 0
        let _ = log.read_at(0);

        // Try iterating
        for result in log.iter() {
            match result {
                Ok(_) => {}
                Err(_) => break, // Errors are fine
            }
        }
    }
});
