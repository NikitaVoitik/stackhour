//! JSONL tailing shared by the claude/codex watchers.
//!
//! read_new_lines: first sight of a file -> offset=EOF, no read; truncation
//! clamp when size < offset; 5MiB lossy catch-up skipping to size−5MiB (the
//! mid-line fragment is dropped — quirk kept); the offset is committed only
//! through the last newline; a chunk without any newline -> offset stays at
//! start; per-line JSON parse with silent drops.
//!
//! Port of `src/agent/tail.js`.

use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// `MAX_READ_PER_FILE` — a tick never reads more than 5MiB from one file.
const MAX_READ_PER_FILE: u64 = 5 * 1024 * 1024;
/// `MAX_HEAD_LINE` — the codex session-metadata head window.
pub const MAX_HEAD_LINE: usize = 1024 * 1024;

/// The offsets map is keyed by the file path exactly as the walker produced
/// it, matching JS `offsets[file]` with a string key.
fn key(file: &Path) -> String {
    file.to_string_lossy().into_owned()
}

/// Read newly appended JSON lines of `file`, updating `offsets[file]`.
///
/// Three behaviours are load-bearing and pinned by tests:
///
/// 1. FIRST SIGHT sets the offset to EOF and returns nothing, so enrolling a
///    machine does not replay months of transcripts as "just now".
/// 2. The offset advances only through the LAST NEWLINE. A writer that has
///    flushed half a JSON object at EOF keeps that fragment for the next
///    tick instead of losing the record forever.
/// 3. `size <= prev` (truncation or rotation) clamps rather than reads.
pub fn read_new_lines(file: &Path, offsets: &mut Map<String, Value>) -> Vec<Value> {
    let name = key(file);
    let Ok(md) = std::fs::metadata(file) else {
        return Vec::new();
    };
    let size = md.len();

    let Some(prev) = offsets.get(&name).and_then(Value::as_u64) else {
        // First sight — or a non-numeric offset, which we treat the same way
        // rather than reading from a garbage position.
        offsets.insert(name, json!(size));
        return Vec::new();
    };
    if size <= prev {
        offsets.insert(name, json!(prev.min(size)));
        return Vec::new();
    }

    // A file that grew by more than 5MiB since the last tick is caught up
    // lossily: we jump to size-5MiB and accept that the first (partial) line
    // there is dropped by the per-line parse.
    let start = if size - prev > MAX_READ_PER_FILE {
        size - MAX_READ_PER_FILE
    } else {
        prev
    };
    let len = (size - start) as usize;

    let mut buf = vec![0u8; len];
    let read = (|| -> std::io::Result<usize> {
        let mut fh = std::fs::File::open(file)?;
        fh.seek(SeekFrom::Start(start))?;
        // read_exact would fail on a concurrent truncation; a short read is
        // fine, we just parse fewer bytes.
        let mut filled = 0;
        while filled < len {
            match fh.read(&mut buf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        Ok(filled)
    })();
    let Ok(filled) = read else {
        return Vec::new();
    };
    buf.truncate(filled);

    let Some(last_newline) = buf.iter().rposition(|b| *b == b'\n') else {
        // No complete record in the new bytes: leave the offset where the
        // read began so the whole fragment is re-read next tick.
        offsets.insert(name, json!(start));
        return Vec::new();
    };
    offsets.insert(name, json!(start + last_newline as u64 + 1));

    String::from_utf8_lossy(&buf[..last_newline])
        .split('\n')
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            // A garbled or half-written line is dropped silently; it must not
            // block the complete records that follow it.
            serde_json::from_str::<Value>(trimmed).ok()
        })
        .collect()
}

/// Read and parse the first JSON line within a byte `limit` window (1MiB for
/// the codex head-line protocol); an oversized first line -> None.
///
/// The `newline < 0 && size > bytes_read` guard is the "oversized" test: no
/// newline inside the window while the file continues past it means the first
/// line is longer than the window, and we must not load the whole rollout to
/// find out where it ends.
pub fn read_first_json_line(file: &Path, limit: usize) -> Option<Value> {
    let md = std::fs::metadata(file).ok()?;
    let size = md.len();
    if size == 0 {
        return None;
    }
    let len = std::cmp::min(size, limit as u64) as usize;
    let mut buf = vec![0u8; len];

    let mut fh = std::fs::File::open(file).ok()?;
    let mut filled = 0;
    while filled < len {
        match fh.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }

    let newline = buf[..filled].iter().position(|b| *b == b'\n');
    let end = match newline {
        Some(idx) => idx,
        // No newline in the window and more file beyond it: oversized head.
        None if size > filled as u64 => return None,
        None => filled,
    };
    serde_json::from_str::<Value>(&String::from_utf8_lossy(&buf[..end])).ok()
}

/// Prune an offsets map: only when it has >= 2000 entries (`max`), keep only
/// keys in `live`.
///
/// The size gate matters — pruning every tick would forget the offset of a
/// file that is merely unreadable this instant (a mounted volume, a
/// permissions blip) and replay it wholesale when it comes back.
pub fn prune_offsets<S: std::hash::BuildHasher>(
    map: &mut Map<String, Value>,
    live: &HashSet<String, S>,
    max: usize,
) {
    if map.len() < max {
        return;
    }
    map.retain(|k, _| live.contains(k));
}

/// `pruneOffsets(offsets, files)` — the default 2000-entry threshold.
pub const DEFAULT_PRUNE_MAX: usize = 2000;

/// `Date.parse(line.timestamp) / 1000`, as epoch seconds.
///
/// Returns `None` where JS produces NaN. Claude and Codex both write RFC 3339
/// with an explicit `Z`; anything else is treated as unparseable rather than
/// guessed at in local time, so a weird line is skipped instead of being
/// attributed to the wrong hour.
pub fn parse_ts_seconds(value: Option<&Value>) -> Option<f64> {
    let text = value?.as_str()?;
    let dt = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    Some(dt.timestamp_millis() as f64 / 1000.0)
}

/// Depth-limited recursive walk collecting files whose basename satisfies
/// `keep`. Mirrors the `function* jsonlFiles(dir, depth)` generators: `depth
/// > 4` stops, an unreadable directory is skipped silently.
pub fn walk_files(dir: &Path, depth: u32, keep: &dyn Fn(&str) -> bool, out: &mut Vec<std::path::PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let full = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk_files(&full, depth + 1, keep, out),
            Ok(_) if keep(&name) => out.push(full),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn append(path: &Path, text: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    /// First sight must not replay history: the offset jumps to EOF and
    /// nothing is emitted, then only appended records come back.
    #[test]
    fn first_sight_starts_at_eof_and_emits_nothing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.jsonl");
        append(&file, "{\"old\":1}\n{\"old\":2}\n");
        let mut offsets = Map::new();

        assert!(read_new_lines(&file, &mut offsets).is_empty());
        assert_eq!(offsets[&key(&file)], json!(20));

        append(&file, "{\"new\":3}\n");
        let rows = read_new_lines(&file, &mut offsets);
        assert_eq!(rows, vec![json!({"new": 3})]);
    }

    /// The headline durability property: a writer that has flushed only half
    /// a record must not lose it. The offset stops at the last newline and
    /// the fragment is re-read once it is complete.
    #[test]
    fn an_incomplete_final_record_is_kept_for_the_next_tick() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.jsonl");
        append(&file, "");
        let mut offsets = Map::new();
        read_new_lines(&file, &mut offsets);

        append(&file, "{\"a\":1}\n{\"b\":");
        let rows = read_new_lines(&file, &mut offsets);
        assert_eq!(rows, vec![json!({"a": 1})]);

        append(&file, "2}\n");
        let rows = read_new_lines(&file, &mut offsets);
        assert_eq!(rows, vec![json!({"b": 2})]);
    }

    /// New bytes with no newline at all: the offset must NOT advance, or the
    /// record is lost when the rest arrives.
    #[test]
    fn a_chunk_without_any_newline_leaves_the_offset_alone() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.jsonl");
        append(&file, "");
        let mut offsets = Map::new();
        read_new_lines(&file, &mut offsets);

        append(&file, "{\"partial\":");
        assert!(read_new_lines(&file, &mut offsets).is_empty());
        assert_eq!(offsets[&key(&file)], json!(0));

        append(&file, "1}\n");
        assert_eq!(read_new_lines(&file, &mut offsets), vec![json!({"partial": 1})]);
    }

    /// One malformed complete line is dropped silently and must not block the
    /// valid records around it.
    #[test]
    fn a_malformed_line_is_skipped_without_blocking_later_records() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.jsonl");
        append(&file, "");
        let mut offsets = Map::new();
        read_new_lines(&file, &mut offsets);

        append(&file, "{\"a\":1}\nnot json at all\n\n{\"b\":2}\n");
        assert_eq!(
            read_new_lines(&file, &mut offsets),
            vec![json!({"a": 1}), json!({"b": 2})]
        );
    }

    /// Rotation/truncation clamps the offset instead of reading from past
    /// the new EOF.
    #[test]
    fn truncation_clamps_the_offset() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.jsonl");
        append(&file, "{\"a\":1}\n{\"b\":2}\n");
        let mut offsets = Map::new();
        read_new_lines(&file, &mut offsets);

        std::fs::write(&file, "{\"c\":3}\n").unwrap();
        assert!(read_new_lines(&file, &mut offsets).is_empty());
        assert_eq!(offsets[&key(&file)], json!(8));
    }

    /// A missing file is not an error and must not disturb its offset.
    #[test]
    fn a_missing_file_yields_nothing() {
        let tmp = TempDir::new().unwrap();
        let mut offsets = Map::new();
        assert!(read_new_lines(&tmp.path().join("gone.jsonl"), &mut offsets).is_empty());
        assert!(offsets.is_empty());
    }

    /// The head reader must handle a session-metadata line larger than the
    /// 64KiB default read buffer without loading the whole rollout, and must
    /// give up rather than scan past its window.
    #[test]
    fn head_reader_handles_a_large_first_line_and_refuses_an_oversized_one() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("rollout.jsonl");

        // ~100KiB first line: bigger than any single read, well inside 1MiB.
        let big = "x".repeat(100_000);
        append(&file, &format!("{{\"cwd\":\"{big}\"}}\n{{\"later\":1}}\n"));
        let head = read_first_json_line(&file, MAX_HEAD_LINE).expect("head parses");
        assert_eq!(head["cwd"].as_str().unwrap().len(), 100_000);

        // A first line past the window, with the file continuing: None.
        assert_eq!(read_first_json_line(&file, 1000), None);

        // An empty file has no head line.
        let empty = tmp.path().join("empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(read_first_json_line(&empty, MAX_HEAD_LINE), None);
    }

    /// Pruning is gated on size: below the threshold a temporarily missing
    /// file keeps its offset, so it is not replayed wholesale when it returns.
    #[test]
    fn pruning_removes_only_stale_files_and_only_past_the_threshold() {
        let mut map = Map::new();
        map.insert("/a".into(), json!(1));
        map.insert("/b".into(), json!(2));
        let live: HashSet<String> = std::iter::once("/a".to_string()).collect();

        prune_offsets(&mut map, &live, DEFAULT_PRUNE_MAX);
        assert_eq!(map.len(), 2, "below the threshold nothing is pruned");

        prune_offsets(&mut map, &live, 2);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("/a"));
    }
}
