//! Wire compatibility with the EXISTING Node Mac-worker scripts.
//!
//! The Mac worker is not being ported in lockstep with the coordinator: for
//! as long as the owner runs the shipped `claim.mjs` / `return.mjs` on his
//! laptop, the Rust coordinator has to speak exactly the protocol those two
//! scripts speak. These tests therefore do NOT re-implement the Node side —
//! they run it, with `node`, as a real child process, against a temp runtime
//! directory:
//!
//! * the coordinator's [`WorkerLane::dispatch`] writes a job → the real
//!   `claim.mjs` claims it, atomically, and prints it back;
//! * the real `return.mjs` publishes a result → the coordinator's
//!   [`WorkerLane::poll_results`] picks it up and delivers the answer.
//!
//! SAFETY: the Telegram half runs against the local mock in
//! `common/mock_bot_api.rs`, never `api.telegram.org`. The filesystem half
//! runs in a `tempfile::tempdir()`, never `~/.claude-remote/` — the scripts
//! resolve their queue directories from their OWN location, so copying them
//! into the temp dir is what keeps the owner's live queue untouched.
//!
//! The scripts under test are the repo copies in `src/bridge/`. `claim.mjs`
//! is byte-identical to the deployed one; `return.mjs` differs only by the
//! job-id validation the repo added, which every real (UUID) id passes.

#[path = "common/mock_bot_api.rs"]
mod mock;

use mock::{MockApi, Reply};
use serde_json::{json, Value};
use stackhour_bridge::local_lane::{LaneContext, LocalTarget};
use stackhour_bridge::telegram::{Tg, TgConfig};
use stackhour_bridge::worker_lane::{WorkerContext, WorkerLane};
use stackhour_bridge::{jobs, BridgePaths};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

const CHAT: i64 = 4242;

// ---------------------------------------------------------------- harness

struct Ctx {
    reg: stackhour_core::registry::Registry,
    now: AtomicI64,
    logs: Mutex<Vec<String>>,
    sessions: Mutex<Vec<(String, String, Option<String>)>>,
}

impl LaneContext for Ctx {
    fn engine(&self, name: &str) -> Option<stackhour_core::registry::EngineDef> {
        self.reg.engines.get(name).cloned()
    }
    fn target(&self, _name: &str, _engine: &str) -> Option<LocalTarget> {
        None
    }
    fn prompt(&self, name: &str, vars: &[(&str, &str)]) -> String {
        self.reg.prompts.render(name, vars)
    }
    fn control_keyboard(&self) -> Value {
        json!({ "inline_keyboard": [] })
    }
    fn session(&self, _t: &str, _e: &str, _a: Option<&str>) -> Option<String> {
        None
    }
    fn set_session(&self, t: &str, e: &str, _a: Option<&str>, id: Option<String>) {
        self.sessions
            .lock()
            .unwrap()
            .push((t.to_string(), e.to_string(), id));
    }
    fn log(&self, line: &str) {
        self.logs.lock().unwrap().push(line.to_string());
    }
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
    fn default_target(&self) -> String {
        "gcp".to_string()
    }
}

impl WorkerContext for Ctx {
    fn worker_label(&self, target: &str) -> String {
        if target == "mac" {
            "🖥️ Mac".to_string()
        } else {
            target.to_string()
        }
    }
}

struct Harness {
    api: MockApi,
    lane: WorkerLane,
    ctx: Arc<Ctx>,
    dir: tempfile::TempDir,
    paths: BridgePaths,
}

/// A temp runtime dir with the queue layout AND the two Node scripts copied
/// in beside it, exactly as they sit in `~/.claude-remote/`.
fn harness() -> Harness {
    let api = MockApi::start();
    let dir = tempfile::tempdir().unwrap();
    let paths = BridgePaths::from_runtime_dir(dir.path());
    paths.ensure_dirs().unwrap();
    for script in ["claim.mjs", "return.mjs"] {
        std::fs::copy(node_script(script), dir.path().join(script)).unwrap();
    }

    let mut cfg = TgConfig::new("test-token", CHAT).with_api_root(api.base.clone());
    cfg.backoff_base_ms = 1;
    let ctx = Arc::new(Ctx {
        reg: stackhour_core::registry::load(Path::new("/nonexistent-config-dir")),
        now: AtomicI64::new(0),
        logs: Mutex::new(Vec::new()),
        sessions: Mutex::new(Vec::new()),
    });
    let lane = WorkerLane::new(
        Arc::new(Tg::with_config(cfg)),
        Arc::clone(&ctx) as Arc<dyn WorkerContext>,
        paths.clone(),
        "mac",
    );
    Harness {
        api,
        lane,
        ctx,
        dir,
        paths,
    }
}

/// The text of the final answer the user reads. `deliverFinal` sends it as a
/// rich (markdown) message, distinct from the plain status line posted at
/// dispatch; delivery also issues a `deleteMessage` for that status line.
fn delivered_text(api: &MockApi) -> String {
    let req = api
        .requests()
        .into_iter()
        .rfind(|r| r.method == "sendRichMessage")
        .unwrap_or_else(|| panic!("no delivery; saw {:?}", api.methods()));
    req.body["rich_message"]["markdown"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn node_script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../src/bridge")
        .join(name)
}

/// `node` is not a build dependency of this crate, so a box without it skips
/// rather than fails — but say so loudly, because a silent skip would let the
/// protocol rot.
fn node_available() -> bool {
    match Command::new("node").arg("--version").output() {
        Ok(o) if o.status.success() => true,
        _ => {
            eprintln!("SKIPPED: `node` is not on PATH; the Node wire-compat half did not run");
            false
        }
    }
}

/// Run the real `claim.mjs` in the temp runtime dir. Returns its stdout,
/// which is either one raw job JSON or empty.
fn node_claim(dir: &Path) -> String {
    let out = Command::new("node")
        .arg(dir.join("claim.mjs"))
        .output()
        .expect("spawn claim.mjs");
    assert!(out.status.success(), "claim.mjs exit: {:?}", out.status);
    String::from_utf8(out.stdout).expect("claim.mjs stdout is utf8")
}

/// Run the real `return.mjs <id>` with `payload` on stdin. Returns its exit
/// code.
fn node_return(dir: &Path, id: &str, payload: &str) -> i32 {
    use std::io::Write as _;
    let mut child = Command::new("node")
        .arg(dir.join("return.mjs"))
        .arg(id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn return.mjs");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    out.status.code().unwrap_or(-1)
}

// ------------------------------------------------- coordinator -> worker

/// The job file the Rust coordinator writes must be claimable, verbatim, by
/// the worker script the owner is running today: same directory, same
/// `.json` filter, same rename-into-`inprogress` claim, same fields.
#[test]
fn a_rust_dispatched_job_is_claimed_intact_by_the_real_claim_mjs() {
    if !node_available() {
        return;
    }
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 1 })));

    let id = h
        .lane
        .dispatch("ship the thing", "codex", None, None)
        .expect("dispatched");

    let claimed = node_claim(h.dir.path());
    let job: Value = serde_json::from_str(&claimed).expect("claim.mjs printed valid job json");
    assert_eq!(job["id"], id);
    assert_eq!(job["prompt"], "ship the thing");
    assert_eq!(job["engine"], "codex");
    assert_eq!(job["media"], Value::Null, "an explicit null, not a missing key");
    assert_eq!(job["sessionId"], Value::Null);
    assert_eq!(
        job["target"], "mac",
        "the roster stamp — an unknown field the Node worker ignores"
    );
    assert!(job["ts"].is_number());

    // The rename IS the claim: gone from jobs/, present in inprogress/.
    assert!(!h.paths.jobs_dir.join(format!("{id}.json")).exists());
    assert!(h.paths.inprogress_dir.join(format!("{id}.json")).exists());

    // Nothing left to claim, and the second poll still exits 0 with empty
    // stdout — that is how the worker tells "no work" from "a job".
    assert_eq!(node_claim(h.dir.path()), "");
}

/// The tmp+rename publish: `claim.mjs` filters on `.json`, so the in-flight
/// `.json.tmp` a Rust dispatch writes is invisible to it and can never be
/// claimed half-written.
#[test]
fn the_tmp_publish_file_is_invisible_to_the_real_claim_mjs() {
    if !node_available() {
        return;
    }
    let h = harness();
    // A torn write left behind by a crashed dispatch.
    std::fs::write(h.paths.jobs_dir.join("aaaa.json.tmp"), r#"{"id":"aaaa","pro"#).unwrap();
    assert_eq!(node_claim(h.dir.path()), "", "a .json.tmp must not be claimable");
    assert!(h.paths.jobs_dir.join("aaaa.json.tmp").exists());
    assert_eq!(std::fs::read_dir(&h.paths.inprogress_dir).unwrap().count(), 0);
}

/// Every claim poll refreshes the heartbeat, and the coordinator's liveness
/// test must read the file the NODE side writes. This is the whole
/// online/offline signal, and it is a bare `Date.now()` with no newline.
#[test]
fn the_heartbeat_the_real_claim_mjs_writes_reads_as_online_in_rust() {
    if !node_available() {
        return;
    }
    let h = harness();
    assert!(!h.lane.worker_alive(), "no heartbeat file yet: offline");

    node_claim(h.dir.path()); // claims nothing, beats anyway

    let raw = std::fs::read_to_string(&h.paths.heartbeat_path).expect("claim.mjs wrote a heartbeat");
    assert!(!raw.ends_with('\n'), "bare ms epoch, no newline: {raw:?}");
    assert!(raw.parse::<i64>().is_ok(), "decimal ms: {raw:?}");
    assert!(h.lane.worker_alive(), "a just-written Node heartbeat is online");

    // The 60s liveness window, from both sides of the boundary.
    let beat_at = raw.parse::<i64>().unwrap();
    std::fs::write(&h.paths.heartbeat_path, (beat_at - 59_000).to_string()).unwrap();
    assert!(h.lane.worker_alive(), "59s old is still online");
    std::fs::write(&h.paths.heartbeat_path, (beat_at - 60_001).to_string()).unwrap();
    assert!(!h.lane.worker_alive(), "60s+ old is offline");
}

/// Offline worker: the job is still written and still queued, the wording
/// changes, and the job survives on disk until the Mac wakes up and claims
/// it — which is the point of the queue.
#[test]
fn a_job_dispatched_while_the_worker_is_offline_is_claimed_when_it_wakes() {
    if !node_available() {
        return;
    }
    let h = harness();
    std::fs::write(&h.paths.heartbeat_path, (jobs::now_ms() - 120_000).to_string()).unwrap();
    h.api.push(Reply::ok(json!({ "message_id": 7 })));

    let id = h
        .lane
        .dispatch("later", "claude", None, None)
        .expect("dispatched");
    assert_eq!(
        h.api.nth(0).body["text"],
        "▹ Claude · 🖥️ Mac · queued (Mac offline — runs when it wakes)"
    );
    assert!(h.paths.jobs_dir.join(format!("{id}.json")).exists());
    assert_eq!(h.lane.pending_len(), 1, "still pending, not dropped");

    // The Mac wakes: its first poll both beats and takes the queued job.
    let job: Value = serde_json::from_str(&node_claim(h.dir.path())).unwrap();
    assert_eq!(job["id"], id);
    assert!(h.lane.worker_alive(), "the wake-up poll flipped it online");
}

// ------------------------------------------------- worker -> coordinator

/// The other direction: a result published by the real `return.mjs` must be
/// consumed by `pollResults`, delivered to the user, and clear the
/// inprogress marker.
#[test]
fn a_result_published_by_the_real_return_mjs_is_delivered_by_poll_results() {
    if !node_available() {
        return;
    }
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 55 })));
    let id = h.lane.dispatch("what is 2+2", "claude", None, None).unwrap();
    node_claim(h.dir.path());
    h.ctx.now.store(3_500, Ordering::SeqCst); // 3.5s of "work"

    let payload = json!({
        "id": id, "engine": "codex", "text": "4",
        "sessionId": "sess-from-mac", "code": 0, "error": null
    })
    .to_string();
    assert_eq!(node_return(h.dir.path(), &id, &payload), 0);

    // Published atomically, with no litter for the 1s poller to trip over.
    let names: Vec<String> = std::fs::read_dir(&h.paths.results_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![format!("{id}.json")], "no .json.tmp left behind");
    assert!(
        !h.paths.inprogress_dir.join(format!("{id}.json")).exists(),
        "return.mjs clears the inprogress marker"
    );

    h.api.push(Reply::ok(json!({ "ok": true }))); // deleteMessage
    h.api.push(Reply::ok(json!({ "message_id": 56 }))); // the answer
    assert_eq!(h.lane.poll_results(), 1);

    assert_eq!(h.lane.pending_len(), 0);
    assert!(!h.paths.results_dir.join(format!("{id}.json")).exists());
    // The worker's echoed engine wins over the dispatch-time one, and its
    // session id is filed under `mac`.
    assert_eq!(
        *h.ctx.sessions.lock().unwrap(),
        vec![(
            "mac".to_string(),
            "codex".to_string(),
            Some("sess-from-mac".to_string())
        )]
    );
    // 3500ms rounds to 4s, exactly as the reference `fmtDur` does.
    assert_eq!(delivered_text(&h.api), "4\n\n— Codex · 🖥️ Mac · 4s");
}

/// The worker pipes nothing when the engine produced nothing. `return.mjs`
/// turns empty stdin into `{}`, and the coordinator must render a message
/// rather than go silent.
#[test]
fn an_empty_payload_from_the_real_return_mjs_still_reaches_the_user() {
    if !node_available() {
        return;
    }
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 60 })));
    let id = h.lane.dispatch("silence", "claude", None, None).unwrap();
    node_claim(h.dir.path());

    assert_eq!(node_return(h.dir.path(), &id, ""), 0);
    assert_eq!(
        std::fs::read_to_string(h.paths.results_dir.join(format!("{id}.json"))).unwrap(),
        "{}"
    );

    h.api.push(Reply::ok(json!({ "ok": true })));
    h.api.push(Reply::ok(json!({ "message_id": 61 })));
    assert_eq!(h.lane.poll_results(), 1);
    assert_eq!(delivered_text(&h.api), "(no output)\n\n— Claude · 🖥️ Mac · 0s");
}

/// The full loop, both scripts, one job: dispatch → claim → return →
/// deliver, ending with all three directories empty.
#[test]
fn the_whole_job_round_trip_runs_through_both_node_scripts() {
    if !node_available() {
        return;
    }
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 70 })));
    let id = h.lane.dispatch("round trip", "claude", None, None).unwrap();

    let job: Value = serde_json::from_str(&node_claim(h.dir.path())).unwrap();
    let echo = json!({ "id": job["id"], "engine": job["engine"], "text": "done",
                       "sessionId": "s9", "code": 0, "error": null });
    assert_eq!(node_return(h.dir.path(), &id, &echo.to_string()), 0);

    h.api.push(Reply::ok(json!({ "ok": true })));
    h.api.push(Reply::ok(json!({ "message_id": 71 })));
    assert_eq!(h.lane.poll_results(), 1);

    for dir in [&h.paths.jobs_dir, &h.paths.inprogress_dir, &h.paths.results_dir] {
        assert_eq!(
            std::fs::read_dir(dir).unwrap().count(),
            0,
            "{} should be drained",
            dir.display()
        );
    }
}
