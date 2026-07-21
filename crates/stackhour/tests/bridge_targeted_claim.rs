#![cfg(feature = "bridge")]
// Every test here drives a verb that only exists when the bridge module is
// compiled in, and the file `use`s `stackhour_bridge` at the top level —
// so without this gate a reduced build fails to COMPILE, which is the
// likeliest way a feature break lands looking green.
//! `stackhour bridge claim <target>` — the targeted half of the pull-worker
//! protocol, driven through the REAL compiled binary.
//!
//! A queue holding jobs for two different targets must yield only the
//! matching job to a targeted claim, leave the other target's job (and any
//! target-less legacy job) untouched, and beat the per-target
//! `worker-heartbeat-<target>` file rather than the shared legacy one. The
//! no-arg invocation stays byte-compatible with the live Node Mac worker —
//! that contract is pinned in `mac_worker_cli_wire_compat.rs`.
//!
//! SAFETY: no Telegram at all on this path — `claim` is a pure filesystem
//! verb. Everything runs in a `tempfile::tempdir()`, never the owner's live
//! queue.

use serde_json::{json, Value};
use stackhour_bridge::{jobs, BridgePaths};
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_stackhour");

fn runtime() -> (tempfile::TempDir, BridgePaths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = BridgePaths::from_runtime_dir(dir.path());
    paths.ensure_dirs().unwrap();
    (dir, paths)
}

/// The binary, spawned in an ISOLATED environment.
///
/// `bridge claim` / `bridge return` resolve everything they touch from
/// `--runtime-dir`, but the module gate in main.rs runs before dispatch and
/// reads `$STACKHOUR_CONFIG` / `$HOME/.config/stackhour/config.json` to decide
/// whether the bridge module is switched on. With an inherited environment
/// these wire-compat tests would therefore fail on exactly the machine this
/// feature exists for — a box whose config.json says
/// `{"modules":{"bridge":false}}` — testing the developer's config instead of
/// the runtime-dir protocol. HOME points at the throwaway runtime dir, where
/// no `.config/stackhour/config.json` exists, so the gate always fails open.
fn cli(args: &[&str], dir: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .env_clear()
        .env("HOME", dir)
        .env("PATH", std::env::var("PATH").unwrap_or_default());
    cmd
}

/// `stackhour bridge claim [target] --runtime-dir <dir>` → (exit, stdout).
fn cli_claim(dir: &Path, target: Option<&str>) -> (i32, String) {
    let mut cmd = cli(&["bridge", "claim"], dir);
    if let Some(t) = target {
        cmd.arg(t);
    }
    cmd.arg("--runtime-dir").arg(dir);
    let out = cmd.output().expect("spawn stackhour bridge claim");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
    )
}

/// A job file in the exact shape the roster-era coordinator writes.
fn stamped_job(paths: &BridgePaths, id: &str, prompt: &str, target: Option<&str>) {
    let mut body = json!({
        "id": id, "prompt": prompt, "engine": "codex",
        "media": null, "sessionId": null,
    });
    if let Some(t) = target {
        body["target"] = json!(t);
    }
    body["ts"] = json!(jobs::now_ms());
    std::fs::write(
        paths.jobs_dir.join(format!("{id}.json")),
        serde_json::to_string(&body).unwrap(),
    )
    .unwrap();
}

const MAC_ID: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
const PI_ID: &str = "aaaa0000-1111-4222-8333-444455556666";
const BARE_ID: &str = "bbbb0000-1111-4222-8333-444455556666";

/// The whole targeted contract in one pass: filtering, exit codes, the
/// untouched siblings, and the per-target heartbeat.
#[test]
fn a_targeted_claim_takes_only_its_own_jobs_and_beats_its_own_heartbeat() {
    let (dir, paths) = runtime();
    stamped_job(&paths, MAC_ID, "for the mac", Some("mac"));
    stamped_job(&paths, PI_ID, "for the pi", Some("pi"));
    stamped_job(&paths, BARE_ID, "legacy job", None);

    // `bridge claim pi` prints exactly pi's job and exits 0.
    let (code, stdout) = cli_claim(dir.path(), Some("pi"));
    assert_eq!(code, 0);
    let job: Value = serde_json::from_str(&stdout).expect("raw job JSON on stdout");
    assert_eq!(job["id"], PI_ID);
    assert_eq!(job["target"], "pi");
    assert_eq!(job["prompt"], "for the pi");

    // Only pi's job moved to inprogress/; mac's and the target-less legacy
    // job are untouched.
    assert!(paths.inprogress_dir.join(format!("{PI_ID}.json")).exists());
    assert!(paths.jobs_dir.join(format!("{MAC_ID}.json")).exists());
    assert!(paths.jobs_dir.join(format!("{BARE_ID}.json")).exists());

    // The targeted claim beat its OWN heartbeat file, not the legacy one.
    let own = dir.path().join("worker-heartbeat-pi");
    assert!(
        own.is_file(),
        "worker-heartbeat-pi must exist after a targeted claim"
    );
    let raw = std::fs::read_to_string(&own).unwrap();
    assert!(!raw.ends_with('\n'), "bare ms epoch, no newline: {raw:?}");
    assert!(raw.parse::<i64>().is_ok(), "decimal ms: {raw:?}");
    assert!(
        !paths.heartbeat_path.exists(),
        "a targeted claim must not touch the legacy shared heartbeat"
    );

    // `bridge claim mac` gets the mac job — and the legacy job STILL stays,
    // because a target-less job matches no filter.
    let (code, stdout) = cli_claim(dir.path(), Some("mac"));
    assert_eq!(code, 0);
    let job: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(job["id"], MAC_ID);
    assert!(paths.jobs_dir.join(format!("{BARE_ID}.json")).exists());

    // The no-arg claim (the live Node Mac worker's invocation) sweeps up the
    // legacy job and beats the shared heartbeat.
    let (code, stdout) = cli_claim(dir.path(), None);
    assert_eq!(code, 0);
    let job: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(job["id"], BARE_ID);
    assert!(
        paths.heartbeat_path.is_file(),
        "the no-arg claim beats the legacy file"
    );
    assert_eq!(std::fs::read_dir(&paths.jobs_dir).unwrap().count(), 0);
}
