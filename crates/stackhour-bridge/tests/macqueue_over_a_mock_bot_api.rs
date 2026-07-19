//! Mac-lane parity tests: the Telegram traffic `dispatchMac`, `pollResults`
//! and `cancelQueuedMac` actually emit.
//!
//! SAFETY: every one of these runs against the local mock in
//! `common/mock_bot_api.rs`. The owner's Node coordinator holds the only
//! legitimate long poll on the real bot token, and a second client on that
//! token would steal his messages, so no test here may ever be pointed at
//! `api.telegram.org`.
//!
//! The unit tests in `src/macqueue.rs` cover the lane's decisions (which
//! session key, which engine label, which error string). These cover the
//! wire: exact method sequence, exact request bodies, exact status text.

#[path = "common/mock_bot_api.rs"]
mod mock;

use mock::{MockApi, Reply};
use serde_json::{json, Value};
use stackhour_bridge::local_lane::{LaneContext, LocalTarget};
use stackhour_bridge::macqueue::{MacContext, MacLane};
use stackhour_bridge::telegram::{Tg, TgConfig};
use stackhour_bridge::{jobs, BridgePaths};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

const CHAT: i64 = 4242;

/// A lane context backed by the real registry defaults, so every string these
/// tests assert on is the shipped template body.
struct Ctx {
    reg: stackhour_core::registry::Registry,
    now: AtomicI64,
    logs: Mutex<Vec<String>>,
    sessions: Mutex<Vec<(String, String, Option<String>)>>,
}

impl Ctx {
    fn new() -> Arc<Ctx> {
        Arc::new(Ctx {
            reg: stackhour_core::registry::load(Path::new("/nonexistent-config-dir")),
            now: AtomicI64::new(0),
            logs: Mutex::new(Vec::new()),
            sessions: Mutex::new(Vec::new()),
        })
    }
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
        json!({ "inline_keyboard": [[{ "text": "🧠 Claude", "callback_data": "e:claude" }]] })
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

impl MacContext for Ctx {
    fn mac_label(&self) -> String {
        "🖥️ Mac".to_string()
    }
}

struct Harness {
    api: MockApi,
    lane: MacLane,
    ctx: Arc<Ctx>,
    dir: tempfile::TempDir,
    paths: BridgePaths,
}

fn harness() -> Harness {
    let api = MockApi::start();
    let dir = tempfile::tempdir().unwrap();
    let paths = BridgePaths::from_runtime_dir(dir.path());
    paths.ensure_dirs().unwrap();

    let mut cfg = TgConfig::new("test-token", CHAT).with_api_root(api.base.clone());
    cfg.backoff_base_ms = 1; // the ladder's SHAPE is tested in transport_telegram.rs
    let ctx = Ctx::new();
    let lane = MacLane::new(
        Arc::new(Tg::with_config(cfg)),
        Arc::clone(&ctx) as Arc<dyn MacContext>,
        paths.clone(),
    );
    Harness { api, lane, ctx, dir, paths }
}

fn write_result(paths: &BridgePaths, id: &str, payload: Value) {
    std::fs::write(
        paths.results_dir.join(format!("{id}.json")),
        payload.to_string(),
    )
    .unwrap();
}

// ---- dispatch ----

/// The status message posted at dispatch: plain text, no parse_mode, the Stop
/// button attached, and `working…` because the heartbeat is fresh.
#[test]
fn dispatch_posts_a_plain_working_status_with_the_stop_button() {
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 501 })));

    h.lane.dispatch("run it", "claude", None, None).unwrap();

    assert_eq!(h.api.methods(), vec!["sendMessage"]);
    let req = h.api.nth(0);
    assert_eq!(req.body["chat_id"], CHAT);
    assert_eq!(req.body["text"], "▹ Claude · 🖥️ Mac · working…");
    assert_eq!(req.body["disable_web_page_preview"], true);
    assert!(
        req.body.get("parse_mode").is_none(),
        "the first status render is plain and UNESCAPED"
    );
    assert_eq!(
        req.body["reply_markup"],
        json!({ "inline_keyboard": [[{ "text": "⏹ Stop", "callback_data": "stop" }]] })
    );
}

/// With a stale heartbeat the wording changes and nothing else does — the job
/// is still written and still tracked, it just waits for the Mac to wake.
#[test]
fn a_stale_heartbeat_changes_the_wording_to_queued() {
    let h = harness();
    std::fs::write(
        &h.paths.heartbeat_path,
        (jobs::now_ms() - 120_000).to_string(),
    )
    .unwrap();
    h.api.push(Reply::ok(json!({ "message_id": 502 })));

    let id = h.lane.dispatch("later", "codex", None, None).unwrap();
    assert_eq!(
        h.api.nth(0).body["text"],
        "▹ Codex · 🖥️ Mac · queued (Mac offline — runs when it wakes)"
    );
    assert!(h.paths.jobs_dir.join(format!("{id}.json")).exists());
    assert_eq!(h.lane.pending_len(), 1);
}

/// A status message that never sent (the API gave up after its retry ladder)
/// must not stop the job: the work is already queued on disk. The pending
/// entry simply carries no message id, and delivery later skips the delete.
#[test]
fn a_failed_status_send_still_leaves_the_job_queued_and_tracked() {
    let h = harness();
    h.api.push_n(5, Reply::err(500, "Internal Server Error"));

    let id = h.lane.dispatch("regardless", "claude", None, None).unwrap();
    assert!(h.paths.jobs_dir.join(format!("{id}.json")).exists());
    assert_eq!(h.lane.pending_len(), 1);

    // Now deliver its result: no deleteMessage, because there is nothing to
    // delete.
    h.api.clear();
    h.api.set_default(Reply::err(400, "Bad Request: unknown method"));
    write_result(&h.paths, &id, json!({ "text": "done" }));
    h.lane.poll_results();
    assert!(
        !h.api.methods().contains(&"deleteMessage".to_string()),
        "saw {:?}",
        h.api.methods()
    );
}

// ---- delivery ----

/// `deliverFinal` ordering, which is load-bearing: the rich send is attempted
/// FIRST, the status message is deleted whether or not it worked, and only
/// then does the chunked-HTML fallback run. `sendRichMessage` is not a real
/// Bot API method today, so the 400 and the retry-without-markup are both
/// expected traffic.
#[test]
fn a_result_is_delivered_rich_first_then_the_status_is_deleted_then_html() {
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 601 })));
    let id = h.lane.dispatch("q", "claude", None, None).unwrap();
    h.api.clear();

    // Two 400s: the first for the rich send WITH reply_markup, the second for
    // the retry without it.
    h.api.push(Reply::err(400, "Bad Request: unknown method"));
    h.api.push(Reply::err(400, "Bad Request: unknown method"));
    h.api.push(Reply::ok(json!(true))); // deleteMessage
    h.api.push(Reply::ok(json!({ "message_id": 602 }))); // the HTML fallback

    h.ctx.now.store(42_000, Ordering::SeqCst);
    write_result(
        &h.paths,
        &id,
        json!({ "engine": "claude", "text": "the answer", "sessionId": "s-1", "code": 0 }),
    );
    assert_eq!(h.lane.poll_results(), 1);

    assert_eq!(
        h.api.methods(),
        vec![
            "sendRichMessage",
            "sendRichMessage",
            "deleteMessage",
            "sendMessage"
        ]
    );
    // The rich send carries the whole final text, footer included.
    assert_eq!(
        h.api.nth(0).body["rich_message"]["markdown"],
        "the answer\n\n— Claude · 🖥️ Mac · 42s"
    );
    // The retry drops the markup and keeps the text.
    assert!(h.api.nth(1).body.get("reply_markup").is_none());
    assert_eq!(h.api.nth(2).body["message_id"], 601);
    // The fallback is HTML and carries the CONTROL keyboard, not the Stop one.
    let fallback = h.api.nth(3);
    assert_eq!(fallback.body["parse_mode"], "HTML");
    assert_eq!(fallback.body["text"], "the answer\n\n— Claude · 🖥️ Mac · 42s");
    assert_eq!(fallback.body["reply_markup"], h.ctx.control_keyboard());

    assert_eq!(
        *h.ctx.sessions.lock().unwrap(),
        vec![("mac".to_string(), "claude".to_string(), Some("s-1".to_string()))]
    );
}

/// The status message is deleted even when EVERY delivery attempt fails. The
/// user watches it vanish and gets nothing back — ugly, and exactly what the
/// reference does, so a port must not "helpfully" keep it around.
#[test]
fn the_status_message_is_deleted_even_when_delivery_fails_entirely() {
    let h = harness();
    h.api.push(Reply::ok(json!({ "message_id": 701 })));
    let id = h.lane.dispatch("q", "claude", None, None).unwrap();
    h.api.clear();
    h.api.set_default(Reply::err(400, "Bad Request: nope"));

    write_result(&h.paths, &id, json!({ "text": "lost" }));
    h.lane.poll_results();

    let deleted = h
        .api
        .requests()
        .into_iter()
        .find(|r| r.method == "deleteMessage")
        .expect("the status message must still be deleted");
    assert_eq!(deleted.body["message_id"], 701);
}

/// An orphan result (the coordinator restarted since dispatch) is delivered
/// with the literal `done` in place of a duration, and with no delete —
/// which is why the old status message keeps a live Stop button forever.
#[test]
fn an_orphan_result_renders_a_done_footer_and_deletes_nothing() {
    let h = harness();
    h.api.set_default(Reply::err(400, "Bad Request: unknown method"));
    write_result(
        &h.paths,
        "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
        json!({ "engine": "codex", "text": "from before the restart" }),
    );
    assert_eq!(h.lane.poll_results(), 1);

    assert_eq!(
        h.api.nth(0).body["rich_message"]["markdown"],
        "from before the restart\n\n— Codex · 🖥️ Mac · done"
    );
    assert!(!h.api.methods().contains(&"deleteMessage".to_string()));
}

/// The worker's error and exit-code payloads, end to end.
#[test]
fn a_worker_error_payload_renders_the_mac_error_line() {
    for (payload, expected) in [
        (
            json!({ "engine": "claude", "error": "ssh died", "text": "" }),
            "⚠️ Mac error: ssh died\n\n— Claude · 🖥️ Mac · done",
        ),
        (
            json!({ "engine": "codex", "text": "", "code": 137 }),
            "⚠️ Codex exited on Mac (code 137).\n\n— Codex · 🖥️ Mac · done",
        ),
        (
            json!({ "engine": "claude", "text": "", "code": 0 }),
            "(no output)\n\n— Claude · 🖥️ Mac · done",
        ),
    ] {
        let h = harness();
        h.api.set_default(Reply::err(400, "Bad Request: unknown method"));
        write_result(&h.paths, "3f2504e0-4f89-41d3-9a0c-0305e82c3301", payload);
        h.lane.poll_results();
        assert_eq!(h.api.nth(0).body["rich_message"]["markdown"], expected);
    }
}

// ---- cancellation ----

/// `🛑 Cancelled.` is edited in with NO parse_mode and an EMPTY extra, which
/// leaves the ⏹ Stop keyboard attached to a job that is no longer running.
/// Deliberate in the reference; preserved here.
#[test]
fn cancelling_edits_the_status_message_and_leaves_the_stop_button_attached() {
    let h = harness();
    h.api.push(Reply::ok(json!({ "message_id": 801 })));
    let id = h.lane.dispatch("nope", "claude", None, None).unwrap();
    h.api.clear();
    h.api.push(Reply::ok(json!({ "message_id": 801 })));

    let out = h.lane.cancel_queued();
    assert_eq!((out.cancelled, out.running), (1, 0));
    assert!(!h.paths.jobs_dir.join(format!("{id}.json")).exists());

    assert_eq!(h.api.methods(), vec!["editMessageText"]);
    let req = h.api.nth(0);
    assert_eq!(req.body["message_id"], 801);
    assert_eq!(req.body["text"], "🛑 Cancelled.");
    assert!(req.body.get("parse_mode").is_none());
    assert!(
        req.body.get("reply_markup").is_none(),
        "an empty extra leaves the existing keyboard in place"
    );
}

/// Cancellation edits arrive oldest-first. The JS `pending` is a
/// insertion-ordered Map; a Rust port using a HashMap would shuffle them.
#[test]
fn cancellation_edits_follow_dispatch_order() {
    let h = harness();
    for id in [901, 902, 903] {
        h.api.push(Reply::ok(json!({ "message_id": id })));
        h.lane.dispatch("x", "claude", None, None).unwrap();
    }
    h.api.clear();

    assert_eq!(h.lane.cancel_queued().cancelled, 3);
    let ids: Vec<i64> = h
        .api
        .requests()
        .into_iter()
        .map(|r| r.body["message_id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![901, 902, 903]);
}

/// A job the worker already claimed cannot be cancelled: no edit is sent, it
/// stays pending, and its result is still delivered when it arrives.
#[test]
fn a_claimed_job_is_reported_as_running_and_never_edited() {
    let h = harness();
    h.api.push(Reply::ok(json!({ "message_id": 1001 })));
    let id = h.lane.dispatch("already gone", "claude", None, None).unwrap();
    // The worker claims it.
    jobs::try_claim(&h.paths.jobs_dir, &h.paths.inprogress_dir).unwrap();
    h.api.clear();

    let out = h.lane.cancel_queued();
    assert_eq!((out.cancelled, out.running), (0, 1));
    assert_eq!(h.api.request_count(), 0, "nothing to say about a running job");
    assert_eq!(h.lane.pending_len(), 1);

    // And when it does come back, it is delivered against its status message.
    h.api.set_default(Reply::err(400, "Bad Request: unknown method"));
    jobs::run_return_with(h.dir.path(), &id, &json!({ "text": "finished" }).to_string());
    assert_eq!(h.lane.poll_results(), 1);
    assert!(h.api.methods().contains(&"deleteMessage".to_string()));
}

// ---- the whole lane, end to end ----

/// Dispatch on the coordinator, claim and return as the worker would, then
/// poll: the full round trip over the real disk protocol and the mock API.
#[test]
fn a_prompt_survives_dispatch_claim_return_and_delivery() {
    let h = harness();
    jobs::beat(&h.paths.heartbeat_path);
    h.api.push(Reply::ok(json!({ "message_id": 1101 })));

    let id = h.lane.dispatch("summarise the log", "codex", None, None).unwrap();
    h.api.clear();
    h.api.set_default(Reply::err(400, "Bad Request: unknown method"));

    // --- the Mac worker's side of the protocol ---
    let raw = jobs::try_claim(&h.paths.jobs_dir, &h.paths.inprogress_dir).expect("claimed");
    let job: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(job["id"], id.as_str());
    assert_eq!(job["prompt"], "summarise the log");
    assert_eq!(job["engine"], "codex");

    let payload = json!({
        "id": id, "engine": "codex", "text": "42 errors, all the same",
        "sessionId": "codex-session", "code": 0, "error": null,
    });
    assert_eq!(
        jobs::run_return_with(h.dir.path(), &id, &payload.to_string()),
        0
    );
    assert!(!h.paths.inprogress_dir.join(format!("{id}.json")).exists());

    // --- back on the coordinator ---
    h.ctx.now.store(7_400, Ordering::SeqCst);
    assert_eq!(h.lane.poll_results(), 1);
    assert_eq!(
        h.api.nth(0).body["rich_message"]["markdown"],
        "42 errors, all the same\n\n— Codex · 🖥️ Mac · 7s"
    );
    assert_eq!(
        *h.ctx.sessions.lock().unwrap(),
        vec![(
            "mac".to_string(),
            "codex".to_string(),
            Some("codex-session".to_string())
        )],
        "the resumed session is filed under mac:codex"
    );
    assert_eq!(h.lane.pending_len(), 0);
    assert_eq!(std::fs::read_dir(&h.paths.results_dir).unwrap().count(), 0);
}
