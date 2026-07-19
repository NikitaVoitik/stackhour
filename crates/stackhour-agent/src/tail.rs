//! JSONL tailing shared by the claude/codex watchers.
//!
//! read_new_lines: first sight of a file -> offset=EOF, no read; truncation
//! clamp when size < offset; 5MiB lossy catch-up skipping to size−5MiB (the
//! mid-line fragment is dropped — quirk kept); the offset is committed only
//! through the last newline; a chunk without any newline -> offset stays at
//! start; per-line JSON parse with silent drops.

use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::Path;

/// Read newly appended JSON lines of `file`, updating `offsets[file]`.
pub fn read_new_lines(file: &Path, offsets: &mut Map<String, Value>) -> Vec<Value> {
    let _ = (file, offsets);
    todo!()
}

/// Read and parse the first JSON line within a byte `limit` window (1MiB for
/// the codex head-line protocol); an oversized first line -> None.
pub fn read_first_json_line(file: &Path, limit: usize) -> Option<Value> {
    let _ = (file, limit);
    todo!()
}

/// Prune an offsets map: only when it has >= 2000 entries (`max`), keep only
/// keys in `live`.
pub fn prune_offsets(map: &mut Map<String, Value>, live: &HashSet<String>, max: usize) {
    let _ = (map, live, max);
    todo!()
}
