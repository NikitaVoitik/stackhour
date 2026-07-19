//! The mac worker daemon.
//!
//! Worker-config load with the loose 5-key runtime check and
//! codexBin/remoteNode defaults. Serial ssh-claim loop (BatchMode=yes,
//! ServerAliveInterval=15, ConnectTimeout=15 verbatim; exit / empty /
//! parse-failure branches with exact log lines + sleep cadence). UUID gate
//! (invalid ids are silently stranded — parity). scp media with the
//! '<remoteDir>/media/' prefix path check and error-result short-circuit;
//! local media-prompt rebuild via media::media_prompt; engine run WITHOUT
//! live status (partial_messages off) + resume retry; the result JSON piped
//! to `ssh <remoteNode> <remoteDir>/return.mjs <id>`; startup media prune
//! once; worker.log logging.

use std::path::Path;

/// Run the worker daemon forever.
pub fn run_worker(runtime_dir: &Path) -> ! {
    let _ = runtime_dir;
    todo!()
}
