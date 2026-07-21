#![cfg(feature = "bridge")]
// Every test here drives a verb that only exists when the bridge module is
// compiled in, and the file `use`s `stackhour_bridge` at the top level —
// so without this gate a reduced build fails to COMPILE, which is the
// likeliest way a feature break lands looking green.
//! The other half of the Mac-worker protocol: the shipped BINARY standing in
//! for `claim.mjs` / `return.mjs`.
//!
//! The worker invokes these two over SSH (`node <dir>/claim.mjs`, `node
//! <dir>/return.mjs <id>`), so during a half-migrated rollout — Rust on the
//! GCP box, the untouched Node worker on the Mac, or the reverse — the
//! `stackhour bridge claim|return` verbs must be drop-in for the scripts.
//! These tests drive the real compiled binary as a child process and check
//! the observable contract the worker depends on: stdout, exit code, and what
//! is left on disk.
//!
//! SAFETY: no Telegram at all on this path — `claim` and `return` are pure
//! filesystem verbs. Everything runs in a `tempfile::tempdir()`, never the
//! owner's live `~/.claude-remote/` queue.

use serde_json::{json, Value};
use stackhour_bridge::{jobs, BridgePaths};
use std::path::Path;
use std::process::{Command, Stdio};

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

/// `stackhour bridge claim --runtime-dir <dir>` → (exit code, stdout).
fn cli_claim(dir: &Path) -> (i32, String) {
    let out = cli(&["bridge", "claim", "--runtime-dir"], dir)
        .arg(dir)
        .output()
        .expect("spawn stackhour bridge claim");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
    )
}

/// `stackhour bridge return <id> --runtime-dir <dir>` with `payload` on
/// stdin → (exit code, stderr).
fn cli_return(dir: &Path, id: &str, payload: &str) -> (i32, String) {
    use std::io::Write as _;
    let mut child = cli(&["bridge", "return", id, "--runtime-dir"], dir)
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stackhour bridge return");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A job file in the exact shape `dispatchMac` writes.
fn node_shaped_job(paths: &BridgePaths, id: &str, prompt: &str) {
    let body = json!({
        "id": id, "prompt": prompt, "engine": "codex",
        "media": null, "sessionId": null, "ts": jobs::now_ms()
    });
    std::fs::write(
        paths.jobs_dir.join(format!("{id}.json")),
        serde_json::to_string(&body).unwrap(),
    )
    .unwrap();
}

const ID: &str = "45f2db13-ee19-4487-96e3-5a1467041246";

/// A job written the Node way is claimed by the binary: raw JSON on stdout,
/// exit 0, renamed into `inprogress/`, heartbeat refreshed. The worker reads
/// stdout verbatim, so a trailing newline (or a log line) would break it.
#[test]
fn the_binary_claims_a_node_shaped_job_and_prints_it_verbatim() {
    let (dir, paths) = runtime();
    node_shaped_job(&paths, ID, "hello from node dispatch");
    let on_disk = std::fs::read_to_string(paths.jobs_dir.join(format!("{ID}.json"))).unwrap();

    let (code, stdout) = cli_claim(dir.path());
    assert_eq!(code, 0);
    assert_eq!(stdout, on_disk, "stdout is the file, byte for byte");
    assert!(!stdout.ends_with('\n'), "no trailing newline");

    assert!(!paths.jobs_dir.join(format!("{ID}.json")).exists());
    assert!(paths.inprogress_dir.join(format!("{ID}.json")).exists());
    assert!(
        jobs::worker_alive(&paths.heartbeat_path),
        "every claim poll must refresh the heartbeat"
    );
}

/// The result the binary publishes must be exactly what the coordinator's
/// `pollResults` expects: `results/<id>.json`, atomically, with the
/// inprogress marker cleared and no `.json.tmp` litter.
#[test]
fn the_binary_publishes_a_result_where_poll_results_looks_for_it() {
    let (dir, paths) = runtime();
    std::fs::write(paths.inprogress_dir.join(format!("{ID}.json")), "{}").unwrap();

    let payload = json!({ "id": ID, "engine": "codex", "text": "4",
                          "sessionId": "s1", "code": 0, "error": null })
    .to_string();
    let (code, _) = cli_return(dir.path(), ID, &payload);
    assert_eq!(code, 0);

    let names: Vec<String> = std::fs::read_dir(&paths.results_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![format!("{ID}.json")]);
    assert_eq!(
        std::fs::read_to_string(paths.results_dir.join(format!("{ID}.json"))).unwrap(),
        payload
    );
    assert!(!paths.inprogress_dir.join(format!("{ID}.json")).exists());
}

/// No work: exit 0 with empty stdout, which is how the worker tells an idle
/// poll from a job. It blocks for the full ~25s claim window first, so this
/// also pins the deadline the worker's SSH loop is built around.
#[test]
fn an_idle_claim_blocks_for_the_claim_window_then_exits_clean() {
    let (dir, paths) = runtime();
    let started = jobs::now_ms();
    let (code, stdout) = cli_claim(dir.path());
    let elapsed = jobs::now_ms() - started;

    assert_eq!(code, 0, "an empty claim is success, not failure");
    assert_eq!(stdout, "");
    assert!(
        elapsed >= 25_000,
        "claim returned after {elapsed}ms; the worker would hammer SSH"
    );
    assert!(elapsed < 40_000, "claim overran its window: {elapsed}ms");
    assert!(
        jobs::worker_alive(&paths.heartbeat_path),
        "an idle poll still beats — this is what keeps the Mac 'online'"
    );
}

/// `return.mjs` exits 2 with `return.mjs: missing job id` when argv[2] is
/// absent. The binary matches the message so the worker's logs read the same.
#[test]
fn a_missing_job_id_is_exit_two_with_the_reference_message() {
    let (dir, _paths) = runtime();
    let out = cli(&["bridge", "return", "--runtime-dir"], dir.path())
        .arg(dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("return.mjs: missing job id"),
        "stderr was {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The live `~/.claude-remote/return.mjs` joins `argv[2]` into a path with no
/// validation, so a crafted id writes outside `results/`. The repo's
/// `return.mjs` and this binary both refuse.
#[test]
fn a_traversing_job_id_is_refused_before_anything_is_written() {
    let (dir, paths) = runtime();
    let (code, stderr) = cli_return(dir.path(), "../../pwned", r#"{"text":"pwn"}"#);
    assert_eq!(code, 2);
    assert!(stderr.contains("return.mjs: invalid job id"), "{stderr}");
    assert_eq!(std::fs::read_dir(&paths.results_dir).unwrap().count(), 0);
    assert!(!dir.path().join("pwned.json").exists());
}

/// Two claimers, one job: the rename is the mutual exclusion, so exactly one
/// process may print it. This is the property the whole queue rests on.
#[test]
fn two_concurrent_binary_claims_never_both_win_the_same_job() {
    let (dir, _paths) = runtime();
    let (d1, d2) = (dir.path().to_path_buf(), dir.path().to_path_buf());
    let paths = BridgePaths::from_runtime_dir(dir.path());
    node_shaped_job(&paths, ID, "only one of you gets this");

    let a = std::thread::spawn(move || cli_claim(&d1));
    let b = std::thread::spawn(move || cli_claim(&d2));
    let (ra, rb) = (a.join().unwrap(), b.join().unwrap());

    let winners: Vec<&(i32, String)> = [&ra, &rb].into_iter().filter(|r| !r.1.is_empty()).collect();
    assert_eq!(winners.len(), 1, "got {ra:?} and {rb:?}");
    let job: Value = serde_json::from_str(&winners[0].1).unwrap();
    assert_eq!(job["id"], ID);
    assert_eq!(std::fs::read_dir(&paths.inprogress_dir).unwrap().count(), 1);
}
