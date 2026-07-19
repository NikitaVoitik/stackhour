//! The filesystem job protocol between coordinator and mac worker.
//!
//! Job files are `<uuid-v4>.json` under jobs/; claim (hidden verb
//! `stackhour bridge claim`) polls for 25s at 1s cadence, writes the
//! worker-heartbeat file EVERY iteration, takes files in lexicographic
//! order, tolerates rename races (losing a rename just continues), prints
//! the raw job JSON to stdout, exits 0/1. return (`stackhour bridge return
//! <id>`) gates on the strict UUID regex (exit 2), pipes stdin to
//! results/<id>.json.tmp then renames (direct-write fallback), and cleans
//! the inprogress marker. The installer writes node-invokable
//! claim.mjs/return.mjs shims that exec these verbs.

use serde_json::Value;
use stackhour_core::Result;
use std::path::Path;

/// Write a job file; returns the generated job id (uuid v4).
pub fn write_job(dir: &Path, job: &Value) -> Result<String> {
    let _ = (dir, job);
    todo!()
}

/// The `stackhour bridge claim` hidden verb. Returns the process exit code.
pub fn run_claim(dir: &Path) -> i32 {
    let _ = dir;
    todo!()
}

/// The `stackhour bridge return <id>` hidden verb. Returns the exit code
/// (2 on an invalid job id).
pub fn run_return(dir: &Path, id: &str) -> i32 {
    let _ = (dir, id);
    todo!()
}

/// Whether the worker heartbeat is fresh (< 60s old).
pub fn worker_alive(dir: &Path) -> bool {
    let _ = dir;
    todo!()
}

/// Delete media files older than 7 days (best-effort).
///
/// The sweep itself lives in [`crate::media::prune_media`] alongside the rest
/// of the attachment lifecycle; this is the fixed-horizon alias the
/// coordinator and worker startup paths call.
pub fn prune_media(dir: &Path) {
    crate::media::prune_media(dir, crate::media::PRUNE_MAX_AGE);
}

/// The strict claim/return UUID validation gate.
pub fn uuid_ok(s: &str) -> bool {
    let _ = s;
    todo!()
}
