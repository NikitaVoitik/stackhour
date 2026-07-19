//! queue.jsonl — the offline heartbeat queue.
//!
//! Tolerant read (missing file -> [], bad lines silently dropped), append
//! with O_APPEND + fsync + chmod 0600, `save_queue(&[])` deletes the file,
//! otherwise an atomic rewrite. Batch selection is greedy over a serialized
//! ≤4MiB estimate with the FIRST-ROW-ALWAYS-INCLUDED rule (an oversized
//! single row is still sent).

use serde_json::Value;
use stackhour_core::Result;
use std::path::Path;

/// Read the queue; missing -> `[]`; unparsable lines silently dropped.
pub fn read_queue(data_dir: &Path) -> Vec<Value> {
    let _ = data_dir;
    todo!()
}

/// Append rows (one JSON line each) with fsync + chmod 0600.
pub fn append_queue(data_dir: &Path, rows: &[Value]) -> Result<()> {
    let _ = (data_dir, rows);
    todo!()
}

/// Rewrite the queue atomically; an empty slice deletes the file.
pub fn save_queue(data_dir: &Path, rows: &[Value]) -> Result<()> {
    let _ = (data_dir, rows);
    todo!()
}

/// How many leading rows fit in `budget` serialized bytes (first row always
/// included). Returns the prefix length.
pub fn take_send_batch(pending: &[Value], budget: usize) -> usize {
    let _ = (pending, budget);
    todo!()
}
