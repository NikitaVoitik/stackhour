//! The full worker round trip through the coordinator's lane.
//!
//! Both halves of the queue protocol, end to end against a temp runtime dir:
//!
//! * the coordinator's [`WorkerLane::dispatch`] writes a job → `claim` takes
//!   it, atomically, and hands back its raw JSON;
//! * `return` publishes a result → the coordinator's
//!   [`WorkerLane::poll_results`] picks it up and delivers the answer.
//!
//! These tests used to spawn the shipped `claim.mjs` / `return.mjs` with
//! `node`, because the Mac worker ran those scripts while the coordinator was
//! being ported. Node is retired: a worker now runs
//! `<remoteDir>/stackhour bridge claim|return` over SSH, so the protocol under
//! test is reached through [`jobs`] directly. The subprocess-level contract —
//! argv, stdout, exit codes — is covered separately, against the real compiled
//! binary, by `crates/stackhour/tests/mac_worker_cli_wire_compat.rs`.
//!
//! SAFETY: the Telegram half runs against the local mock in
//! `common/mock_bot_api.rs`, never `api.telegram.org`. The filesystem half
//! runs in a `tempfile::tempdir()`, never `~/.claude-remote/`.

#[path = "common/mock_bot_api.rs"]
mod mock;

use mock::{MockApi, Reply};
use serde_json::{json, Value};
use stackhour_bridge::local_lane::{LaneContext, LocalTarget};
use stackhour_bridge::telegram::{Tg, TgConfig};
use stackhour_bridge::worker_lane::{WorkerContext, WorkerLane};
use stackhour_bridge::{jobs, BridgePaths};
use std::path::Path;
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

/// A temp runtime dir with the queue layout the worker lane expects.
fn harness() -> Harness {
    let api = MockApi::start();
    let dir = tempfile::tempdir().unwrap();
    let paths = BridgePaths::from_runtime_dir(dir.path());
    paths.ensure_dirs().unwrap();

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

/// One `bridge claim` poll. Returns what the verb would print on stdout:
/// either one raw job JSON, or empty for "no work".
///
/// This is the body of [`jobs::run_claim`]'s loop without its blocking wait —
/// that verb polls for up to ~25s before reporting an empty claim, which
/// would add half a minute to every "nothing claimable" assertion here for no
/// extra coverage. The claim itself, the heartbeat, and the `.json` filter are
/// the same calls the verb makes.
fn claim(dir: &Path) -> String {
    let paths = BridgePaths::from_runtime_dir(dir);
    jobs::beat(&jobs::heartbeat_path_for(&paths, None));
    jobs::try_claim_target(&paths.jobs_dir, &paths.inprogress_dir, None).unwrap_or_default()
}

/// `bridge return <id>` with `payload` as its stdin. Returns the exit code.
fn ret(dir: &Path, id: &str, payload: &str) -> i32 {
    jobs::run_return_with(dir, id, payload)
}

// ------------------------------------------------- coordinator -> worker

/// The job file the coordinator writes must be claimable verbatim: same
/// directory, same `.json` filter, same rename-into-`inprogress` claim, and
/// the same fields a worker reads.
#[test]
fn a_rust_dispatched_job_is_claimed_intact() {
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 1 })));

    let id = h
        .lane
        .dispatch("ship the thing", "codex", None, None)
        .expect("dispatched");

    let claimed = claim(h.dir.path());
    let job: Value = serde_json::from_str(&claimed).expect("claim printed valid job json");
    assert_eq!(job["id"], id);
    assert_eq!(job["prompt"], "ship the thing");
    assert_eq!(job["engine"], "codex");
    assert_eq!(job["media"], Value::Null, "an explicit null, not a missing key");
    assert_eq!(job["sessionId"], Value::Null);
    assert_eq!(job["target"], "mac", "the roster stamp");
    assert!(job["ts"].is_number());

    // The rename IS the claim: gone from jobs/, present in inprogress/.
    assert!(!h.paths.jobs_dir.join(format!("{id}.json")).exists());
    assert!(h.paths.inprogress_dir.join(format!("{id}.json")).exists());

    // Nothing left to claim, and the second poll still exits 0 with empty
    // stdout — that is how the worker tells "no work" from "a job".
    assert_eq!(claim(h.dir.path()), "");
}

/// The tmp+rename publish: claim filters on `.json`, so the in-flight
/// `.json.tmp` a dispatch writes is invisible to it and can never be claimed
/// half-written.
#[test]
fn the_tmp_publish_file_is_invisible_to_claim() {
    let h = harness();
    // A torn write left behind by a crashed dispatch.
    std::fs::write(h.paths.jobs_dir.join("aaaa.json.tmp"), r#"{"id":"aaaa","pro"#).unwrap();
    assert_eq!(claim(h.dir.path()), "", "a .json.tmp must not be claimable");
    assert!(h.paths.jobs_dir.join("aaaa.json.tmp").exists());
    assert_eq!(std::fs::read_dir(&h.paths.inprogress_dir).unwrap().count(), 0);
}

/// Every claim poll refreshes the heartbeat, and the coordinator's liveness
/// test must read the file the NODE side writes. This is the whole
/// online/offline signal, and it is a bare `Date.now()` with no newline.
#[test]
fn the_heartbeat_claim_writes_reads_as_online() {
    let h = harness();
    assert!(!h.lane.worker_alive(), "no heartbeat file yet: offline");

    claim(h.dir.path()); // claims nothing, beats anyway

    let raw = std::fs::read_to_string(&h.paths.heartbeat_path).expect("claim wrote a heartbeat");
    assert!(!raw.ends_with('\n'), "bare ms epoch, no newline: {raw:?}");
    assert!(raw.parse::<i64>().is_ok(), "decimal ms: {raw:?}");
    assert!(h.lane.worker_alive(), "a just-written heartbeat is online");

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
    let job: Value = serde_json::from_str(&claim(h.dir.path())).unwrap();
    assert_eq!(job["id"], id);
    assert!(h.lane.worker_alive(), "the wake-up poll flipped it online");
}

// ------------------------------------------------- worker -> coordinator

/// The other direction: a published result must be consumed by
/// `pollResults`, delivered to the user, and clear the inprogress marker.
#[test]
fn a_published_result_is_delivered_by_poll_results() {
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 55 })));
    let id = h.lane.dispatch("what is 2+2", "claude", None, None).unwrap();
    claim(h.dir.path());
    h.ctx.now.store(3_500, Ordering::SeqCst); // 3.5s of "work"

    let payload = json!({
        "id": id, "engine": "codex", "text": "4",
        "sessionId": "sess-from-mac", "code": 0, "error": null
    })
    .to_string();
    assert_eq!(ret(h.dir.path(), &id, &payload), 0);

    // Published atomically, with no litter for the 1s poller to trip over.
    let names: Vec<String> = std::fs::read_dir(&h.paths.results_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![format!("{id}.json")], "no .json.tmp left behind");
    assert!(
        !h.paths.inprogress_dir.join(format!("{id}.json")).exists(),
        "return clears the inprogress marker"
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

/// The worker pipes nothing when the engine produced nothing. `return`
/// turns an empty payload into `{}`, and the coordinator must render a
/// message rather than go silent.
#[test]
fn an_empty_return_payload_still_reaches_the_user() {
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 60 })));
    let id = h.lane.dispatch("silence", "claude", None, None).unwrap();
    claim(h.dir.path());

    assert_eq!(ret(h.dir.path(), &id, ""), 0);
    assert_eq!(
        std::fs::read_to_string(h.paths.results_dir.join(format!("{id}.json"))).unwrap(),
        "{}"
    );

    h.api.push(Reply::ok(json!({ "ok": true })));
    h.api.push(Reply::ok(json!({ "message_id": 61 })));
    assert_eq!(h.lane.poll_results(), 1);
    assert_eq!(delivered_text(&h.api), "(no output)\n\n— Claude · 🖥️ Mac · 0s");
}

/// The full loop, one job: dispatch → claim → return → deliver, ending with
/// all three directories empty.
#[test]
fn the_whole_job_round_trip_runs_through_both_verbs() {
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 70 })));
    let id = h.lane.dispatch("round trip", "claude", None, None).unwrap();

    let job: Value = serde_json::from_str(&claim(h.dir.path())).unwrap();
    let echo = json!({ "id": job["id"], "engine": job["engine"], "text": "done",
                       "sessionId": "s9", "code": 0, "error": null });
    assert_eq!(ret(h.dir.path(), &id, &echo.to_string()), 0);

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
