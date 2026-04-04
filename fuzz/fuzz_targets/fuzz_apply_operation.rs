//! Fuzz target: apply_operation on arbitrary state + operation sequences.
//!
//! This is the core state reconstruction logic. We feed arbitrary sequences
//! of operations and verify that apply_operation never panics — any failure
//! must be a clean Result::Err, not an unwrap/index-out-of-bounds/etc.

#![no_main]

use arbitrary::Arbitrary;
use chronicle::{apply_operation, StateOperation, TreeEntry};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug, Clone)]
enum FuzzOp {
    Set(Vec<u8>),
    Append(Vec<u8>),
    Redact { start: usize, end: usize },
    Edit { index: usize, new_value: Vec<u8> },
    Snapshot(Vec<u8>),
    DeltaSnapshot(Vec<u8>),
    TreeSet { path: String, blob_hash: String, size: u64 },
    TreeRemove { path: String },
}

impl FuzzOp {
    fn into_state_op(self) -> StateOperation {
        match self {
            FuzzOp::Set(v) => StateOperation::Set(v),
            FuzzOp::Append(v) => StateOperation::Append(v),
            FuzzOp::Redact { start, end } => StateOperation::Redact { start, end },
            FuzzOp::Edit { index, new_value } => StateOperation::Edit { index, new_value },
            FuzzOp::Snapshot(v) => StateOperation::Snapshot(v),
            FuzzOp::DeltaSnapshot(v) => StateOperation::DeltaSnapshot(v),
            FuzzOp::TreeSet { path, blob_hash, size } => StateOperation::TreeSet {
                path,
                entry: TreeEntry { blob_hash, size, mode: 0o644 },
            },
            FuzzOp::TreeRemove { path } => StateOperation::TreeRemove { path },
        }
    }
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    initial_state: Vec<u8>,
    operations: Vec<FuzzOp>,
}

fuzz_target!(|input: FuzzInput| {
    // Limit operation count to avoid timeouts
    let ops = if input.operations.len() > 20 {
        &input.operations[..20]
    } else {
        &input.operations
    };

    let mut state = input.initial_state;
    for op in ops.iter() {
        // apply_operation must never panic — only clean errors allowed
        match apply_operation(state.clone(), op.clone().into_state_op()) {
            Ok(new_state) => state = new_state,
            Err(_) => {} // Errors are fine, panics are not
        }
    }
});
