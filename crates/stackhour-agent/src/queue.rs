//! queue.jsonl — the offline heartbeat queue.
//!
//! Tolerant read (missing file -> [], bad lines silently dropped), append
//! with O_APPEND + fsync + chmod 0600, `save_queue(&[])` deletes the file,
//! otherwise an atomic rewrite. Batch selection is greedy over a serialized
//! ≤4MiB estimate with the FIRST-ROW-ALWAYS-INCLUDED rule (an oversized
//! single row is still sent).

use serde_json::Value;
use stackhour_core::{Error, Result};
use std::path::{Path, PathBuf};

/// The default send budget: 4 MiB.
pub const MAX_SEND_BYTES: usize = 4 * 1024 * 1024;

/// `<data_dir>/queue.jsonl`.
pub fn queue_path(data_dir: &Path) -> PathBuf {
    data_dir.join("queue.jsonl")
}

/// Read the queue; missing -> `[]`; unparsable lines silently dropped.
pub fn read_queue(data_dir: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(queue_path(data_dir)) else {
        return Vec::new();
    };
    text.split('\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// One JSON object per line, with a trailing newline. Shared by append and
/// rewrite so both produce byte-identical framing.
fn encode(rows: &[Value]) -> String {
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&serde_json::to_string(row).unwrap_or_else(|_| "null".to_string()));
    }
    out.push('\n');
    out
}

/// Append rows (one JSON line each) with fsync + chmod 0600.
pub fn append_queue(data_dir: &Path, rows: &[Value]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(data_dir)
        .map_err(|e| Error::msg(format!("cannot create {}: {e}", data_dir.display())))?;
    let path = queue_path(data_dir);
    stackhour_core::fsutil::append_fsync_0600(&path, encode(rows).as_bytes())
        .map_err(|e| Error::msg(format!("cannot append to {}: {e}", path.display())))
}

/// Rewrite the queue atomically; an empty slice deletes the file.
pub fn save_queue(data_dir: &Path, rows: &[Value]) -> Result<()> {
    let path = queue_path(data_dir);
    if rows.is_empty() {
        // `fs.rmSync(path, { force: true })` — a missing file is fine.
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::msg(format!("cannot remove {}: {e}", path.display()))),
        };
    }
    std::fs::create_dir_all(data_dir)
        .map_err(|e| Error::msg(format!("cannot create {}: {e}", data_dir.display())))?;
    stackhour_core::fsutil::atomic_write_0600(&path, encode(rows).as_bytes())
        .map_err(|e| Error::msg(format!("cannot write {}: {e}", path.display())))
}

/// How many leading rows fit in `budget` serialized bytes (first row always
/// included). Returns the prefix length.
///
/// The accounting mirrors the JS byte estimate exactly: 2 bytes for the
/// enclosing `[]`, each row's serialized length, plus 1 byte for the comma
/// that precedes every row after the first.
pub fn take_send_batch(pending: &[Value], budget: usize) -> usize {
    let mut count = 0usize;
    let mut bytes = 2usize;
    for row in pending {
        let row_bytes = serde_json::to_string(row).map_or(4, |s| s.len()) + usize::from(count > 0);
        // The FIRST row is always taken, even when it alone exceeds the
        // budget — otherwise one oversized heartbeat would wedge the queue
        // forever.
        if count > 0 && bytes + row_bytes > budget {
            break;
        }
        count += 1;
        bytes += row_bytes;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn row(i: usize) -> Value {
        json!({ "time": i, "machine": "box", "source": "editor-files" })
    }

    #[test]
    fn a_missing_queue_reads_as_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(read_queue(tmp.path()).is_empty());
        assert!(read_queue(&tmp.path().join("nope")).is_empty());
    }

    #[test]
    fn append_then_read_round_trips_in_order() {
        let tmp = TempDir::new().unwrap();
        append_queue(tmp.path(), &[row(1), row(2)]).unwrap();
        append_queue(tmp.path(), &[row(3)]).unwrap();
        assert_eq!(read_queue(tmp.path()), vec![row(1), row(2), row(3)]);
    }

    /// A torn or corrupt line must not poison the whole queue — the good
    /// rows around it still drain.
    #[test]
    fn corrupt_lines_are_dropped_not_fatal() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            queue_path(tmp.path()),
            "{\"time\":1}\n{ truncated\n\n{\"time\":2}\n",
        )
        .unwrap();
        assert_eq!(
            read_queue(tmp.path()),
            vec![json!({"time":1}), json!({"time":2})]
        );
    }

    /// The queue holds heartbeats for an unreachable server; it must not be
    /// world-readable.
    #[test]
    fn the_queue_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        append_queue(tmp.path(), &[row(1)]).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&queue_path(tmp.path())), 0o600);
        save_queue(tmp.path(), &[row(2)]).unwrap();
        assert_eq!(mode(&queue_path(tmp.path())), 0o600);
    }

    /// Draining the queue completely REMOVES the file — doctor's
    /// offline-queue check distinguishes absent from zero-length.
    #[test]
    fn saving_an_empty_queue_deletes_the_file() {
        let tmp = TempDir::new().unwrap();
        append_queue(tmp.path(), &[row(1)]).unwrap();
        assert!(queue_path(tmp.path()).exists());
        save_queue(tmp.path(), &[]).unwrap();
        assert!(!queue_path(tmp.path()).exists());
        // Idempotent: deleting an already-absent queue is not an error.
        save_queue(tmp.path(), &[]).unwrap();
    }

    #[test]
    fn save_queue_replaces_rather_than_appends() {
        let tmp = TempDir::new().unwrap();
        append_queue(tmp.path(), &[row(1), row(2), row(3)]).unwrap();
        save_queue(tmp.path(), &[row(3)]).unwrap();
        assert_eq!(read_queue(tmp.path()), vec![row(3)]);
    }

    #[test]
    fn appending_nothing_does_not_create_a_file() {
        let tmp = TempDir::new().unwrap();
        append_queue(tmp.path(), &[]).unwrap();
        assert!(!queue_path(tmp.path()).exists());
    }

    #[test]
    fn batches_are_capped_at_the_byte_budget() {
        let rows: Vec<Value> = (0..100).map(row).collect();
        assert_eq!(take_send_batch(&rows, MAX_SEND_BYTES), 100);
        let n = take_send_batch(&rows, 200);
        assert!(n > 0 && n < 100, "got {n}");
        let bytes: usize = 2
            + rows[..n]
                .iter()
                .map(|r| serde_json::to_string(r).unwrap().len())
                .sum::<usize>()
            + (n - 1);
        assert!(bytes <= 200, "batch of {n} was {bytes} bytes");
    }

    /// A single row larger than the whole budget is still sent, otherwise it
    /// would block the queue forever.
    #[test]
    fn an_oversized_first_row_is_always_included() {
        let huge = json!({ "entity": "x".repeat(10_000) });
        assert_eq!(take_send_batch(std::slice::from_ref(&huge), 100), 1);
        // ...but it does not drag a second row along with it.
        assert_eq!(take_send_batch(&[huge, row(1)], 100), 1);
    }

    #[test]
    fn an_empty_pending_list_yields_an_empty_batch() {
        assert_eq!(take_send_batch(&[], MAX_SEND_BYTES), 0);
    }
}
