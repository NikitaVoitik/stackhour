//! LIVE STATUS STREAMING parity: one status message per job, EDITED as the
//! engine reports activity, deleted when the answer lands.
//!
//! SAFETY: every Telegram call here goes to the in-process mock in
//! `common/mock_bot_api.rs`. The owner's Node coordinator holds the only
//! legitimate `getUpdates` poll on the real token; nothing in this file may
//! ever be pointed at api.telegram.org.
//!
//! ## Where the expected sequence comes from
//!
//! It is not guessed from reading coordinator.mjs — it was RECORDED. A copy of
//! `/home/nikita/.claude-remote/coordinator.mjs` (the reference, untouched)
//! was pointed at a local mock Bot API with a fake token and a fake `claude`
//! that speaks `--output-format stream-json`, and every request it made was
//! captured. The harness that does this is `test/parity/live-status.mjs`; run
//! `cargo build --release && node test/parity/live-status.mjs` to re-record and
//! re-diff both implementations for yourself. The Node emitted, in order:
//!
//! ```text
//! setMessageReaction  msg=77 👀
//! sendMessage         "▹ Claude · ☁️ GCP · working…"   no parse_mode, stop kb
//! sendChatAction      typing
//! editMessageText     "▹ Claude · ☁️ GCP · ⚙️ Bash: ls -la /tmp"   HTML, stop kb
//! sendChatAction      typing
//! editMessageText     "▹ Claude · ☁️ GCP · ⚙️ Read: /tmp/notes.md" HTML, stop kb
//! sendChatAction      typing
//! sendRichMessage     (400s — not a real Bot API method)
//! sendRichMessage     (retry without reply_markup, 400s again)
//! deleteMessage       the status message
//! sendMessage         the answer + provenance footer, HTML, control kb
//! ```
//!
//! The properties that matter and that this file pins:
//! * exactly ONE `sendMessage` before delivery — a job never spams the chat;
//! * every later render is an `editMessageText` of that SAME message id;
//! * a typing indicator accompanies the first status and every activity edit;
//! * the inbound message gets the 👀 reaction, once;
//! * the status message is deleted only after the answer has been attempted.

#[path = "common/mock_bot_api.rs"]
mod mock;

use mock::{MockApi, Reply};
use serde_json::{json, Value};
use stackhour_bridge::config::load_coordinator_cfg;
use stackhour_bridge::coordinator::Runtime;
use stackhour_bridge::registry_ctx::RegistryCtx;
use stackhour_bridge::telegram::{Tg, TgConfig};
use stackhour_bridge::BridgePaths;
use std::path::Path;

const CHAT: i64 = 999_111;
const INBOUND_MSG_ID: i64 = 77;
/// The id the mock hands back for the status `sendMessage`.
const STATUS_MSG_ID: i64 = 4001;

/// A fake `claude` that speaks `--output-format stream-json`. The `sleep`s
/// straddle the 800ms activity throttle so each event produces exactly one
/// status render, as they did in the recorded Node run.
const FAKE_ENGINE: &str = r#"#!/usr/bin/env bash
cat > /dev/null
echo '{"type":"system","subtype":"init","session_id":"sess-parity-1"}'
sleep 1
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la /tmp"}}]}}'
sleep 1
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/notes.md"}}]}}'
sleep 1
echo '{"type":"result","subtype":"success","session_id":"sess-parity-1","result":"the parity answer"}'
"#;

/// A fake `claude` that emits a rapid burst inside the throttle window and
/// then repeats one activity verbatim, to pin the throttle and the
/// `last_shown` dedupe.
const FAKE_ENGINE_BURST: &str = r#"#!/usr/bin/env bash
cat > /dev/null
echo '{"type":"system","subtype":"init","session_id":"sess-burst"}'
for i in 1 2 3 4 5; do
  echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"burst '"$i"'"}}]}}'
done
sleep 1.2
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep","input":{"pattern":"same"}}]}}'
sleep 1.2
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep","input":{"pattern":"same"}}]}}'
sleep 1.2
echo '{"type":"result","subtype":"success","session_id":"sess-burst","result":"burst done"}'
"#;

/// A runtime dir holding a config with a FAKE token, pointed at the mock.
fn runtime_dir(api: &MockApi, engine_script: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("fake-claude");
    std::fs::write(&bin, engine_script).expect("write fake engine");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    let cfg = json!({
        // A FAKE token. The real one lives only in the owner's config.json and
        // must never appear in this repo.
        "token": "111111:FAKE-TOKEN-FOR-LOCAL-MOCK-ONLY",
        "chatId": CHAT,
        "apiRoot": api.base,
        "defaultTarget": "gcp",
        "targets": {
            "gcp": {
                "label": "☁️ GCP",
                "type": "local",
                "claudeBin": bin.to_string_lossy(),
                "permissionMode": "bypassPermissions",
            },
            "mac": { "label": "🖥️ Mac", "type": "remote" },
        },
    });
    std::fs::write(
        dir.path().join("config.json"),
        serde_json::to_string_pretty(&cfg).expect("cfg json"),
    )
    .expect("write config");
    dir
}

fn runtime(api: &MockApi, dir: &Path) -> Runtime {
    let cfg = load_coordinator_cfg(&dir.join("config.json")).expect("load cfg");
    let mut tg_cfg = TgConfig::new(&cfg.token, cfg.chat_id).with_api_root(api.base.clone());
    tg_cfg.backoff_base_ms = 1;
    tg_cfg.retry_after_slack_secs = 0;
    let paths = BridgePaths {
        runtime_dir: dir.to_path_buf(),
        ..BridgePaths::resolve(&|_| None, dir)
    };
    let reg = RegistryCtx::from_registry(stackhour_core::registry::load_with(
        Path::new("/nonexistent-stackhour-config"),
        stackhour_core::registry::EnvSource::fixed(&[]),
    ));
    Runtime::new(cfg, paths, Tg::with_config(tg_cfg), reg)
}

fn text_update(text: &str) -> Value {
    json!({
        "update_id": 501,
        "message": {
            "message_id": INBOUND_MSG_ID,
            "date": 1,
            "chat": { "id": CHAT },
            "from": { "id": CHAT, "is_bot": false },
            "text": text,
        },
    })
}

/// The mock's default reply is a `sendMessage` result; `sendRichMessage` has
/// to 400 the way the live API does, so the fallback path is what delivers.
fn script(api: &MockApi) {
    api.set_default(Reply::ok(json!({ "message_id": STATUS_MSG_ID })));
    api.set_method_reply(
        "sendRichMessage",
        Reply::err(400, "Bad Request: method not found"),
    );
}

fn drive(runtime: &Runtime, update: &Value) {
    runtime.handle_update(update);
    for _ in 0..1200 {
        if !runtime.local.is_busy() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("the local lane never finished");
}

/// Method names with `sendRichMessage` collapsed: the transport retries it
/// once without `reply_markup`, which is a transport detail, not a lane one.
fn shape(api: &MockApi) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in api.methods() {
        if m == "sendRichMessage" && out.last().map(String::as_str) == Some("sendRichMessage") {
            continue;
        }
        out.push(m);
    }
    out
}

/// The whole recorded Node sequence, reproduced by the Rust bridge for the
/// same job: react, one status message, one edit per activity with a typing
/// indicator each, then rich-attempt -> delete -> the answer.
#[test]
fn a_running_job_edits_one_status_message_and_then_delivers() {
    let api = MockApi::start();
    // sendRichMessage 400s so the chunked-HTML fallback delivers, exactly as
    // against the real API where the method does not exist.
    script(&api);

    let dir = runtime_dir(&api, FAKE_ENGINE);
    let rt = runtime(&api, dir.path());
    drive(&rt, &text_update("summarise the parity slice"));

    assert_eq!(
        shape(&api),
        vec![
            "setMessageReaction",
            "sendMessage",
            "sendChatAction",
            "editMessageText",
            "sendChatAction",
            "editMessageText",
            "sendChatAction",
            "sendRichMessage",
            "deleteMessage",
            "sendMessage",
        ],
        "the API call sequence diverged from the recorded coordinator.mjs run"
    );

    let seen = api.requests();

    // 👀 on the inbound message, once.
    let reacts: Vec<_> = seen.iter().filter(|r| r.method == "setMessageReaction").collect();
    assert_eq!(reacts.len(), 1, "the inbound message is reacted to exactly once");
    assert_eq!(reacts[0].body["message_id"], INBOUND_MSG_ID);
    assert_eq!(reacts[0].body["reaction"][0]["emoji"], "👀");

    // Exactly ONE status message: a job never spams the chat.
    let sends: Vec<_> = seen.iter().filter(|r| r.method == "sendMessage").collect();
    assert_eq!(sends.len(), 2, "one status message plus one delivered answer");

    let status = sends[0];
    assert_eq!(status.body["text"], "▹ Claude · ☁️ GCP · working…");
    assert!(
        status.body.get("parse_mode").is_none(),
        "the FIRST status render carries no parse_mode (JS parity)"
    );
    assert_eq!(
        status.body["reply_markup"]["inline_keyboard"][0][0]["callback_data"], "stop",
        "the status message carries the stop button while the job runs"
    );

    // Every activity is an EDIT of that same message id, HTML, stop keyboard.
    let edits: Vec<_> = seen.iter().filter(|r| r.method == "editMessageText").collect();
    assert_eq!(edits.len(), 2, "one edit per throttled activity event");
    for edit in &edits {
        assert_eq!(edit.body["message_id"], STATUS_MSG_ID, "edits reuse one message");
        assert_eq!(edit.body["parse_mode"], "HTML");
        assert_eq!(
            edit.body["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
            "stop"
        );
    }
    assert_eq!(edits[0].body["text"], "▹ Claude · ☁️ GCP · ⚙️ Bash: ls -la /tmp");
    assert_eq!(
        edits[1].body["text"],
        "▹ Claude · ☁️ GCP · ⚙️ Read: /tmp/notes.md"
    );

    // A typing indicator with the first status and with every activity edit.
    let typings: Vec<_> = seen.iter().filter(|r| r.method == "sendChatAction").collect();
    assert_eq!(typings.len(), 3, "typing accompanies the status and each edit");
    for t in &typings {
        assert_eq!(t.body["action"], "typing");
        assert_eq!(t.body["chat_id"], CHAT);
    }

    // The status message is deleted only after the answer has been attempted.
    let delete = seen
        .iter()
        .find(|r| r.method == "deleteMessage")
        .expect("deleted");
    assert_eq!(delete.body["message_id"], STATUS_MSG_ID);

    // The answer carries the provenance footer and the control keyboard.
    let answer = sends[1];
    let text = answer.body["text"].as_str().expect("answer text");
    assert!(
        text.starts_with("the parity answer") && text.contains("— Claude · ☁️ GCP · "),
        "the delivered answer must carry the provenance footer:\n{text}"
    );
    assert_eq!(answer.body["parse_mode"], "HTML");
    assert!(
        answer.body["reply_markup"]["inline_keyboard"]
            .as_array()
            .is_some_and(|k| !k.is_empty()),
        "the answer carries the control keyboard, not the stop keyboard"
    );
}

/// A burst inside the throttle window collapses to one edit, and a repeated
/// activity produces NO edit at all — but still fires a typing indicator.
/// That last quirk is the Node's (`showStatus` dedupes, `typing()` does not)
/// and was reproduced by the recorded run, so it is pinned rather than tidied.
#[test]
fn bursts_are_throttled_and_repeats_are_deduped() {
    let api = MockApi::start();
    script(&api);

    let dir = runtime_dir(&api, FAKE_ENGINE_BURST);
    let rt = runtime(&api, dir.path());
    drive(&rt, &text_update("burst please"));

    let seen = api.requests();
    let edits: Vec<_> = seen.iter().filter(|r| r.method == "editMessageText").collect();
    assert_eq!(
        edits.len(),
        2,
        "five rapid events collapse to one edit and the repeat is deduped; saw {:?}",
        edits.iter().map(|e| e.body["text"].clone()).collect::<Vec<_>>()
    );
    assert_eq!(edits[0].body["text"], "▹ Claude · ☁️ GCP · ⚙️ Bash: burst 1");
    assert_eq!(edits[1].body["text"], "▹ Claude · ☁️ GCP · ⚙️ Grep: same");

    assert_eq!(
        seen.iter().filter(|r| r.method == "sendChatAction").count(),
        4,
        "the deduped repeat still fires typing — JS parity"
    );
    assert_eq!(
        seen.iter().filter(|r| r.method == "sendMessage").count(),
        2,
        "still exactly one status message plus one answer"
    );
}
