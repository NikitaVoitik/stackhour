//! The command surface end to end: a typed message or a tapped button is
//! planned into [`Action`]s, the actions are executed against a Bot API, and
//! the resulting REQUESTS are compared to what coordinator.mjs would have
//! sent.
//!
//! SAFETY: everything here runs against the in-process mock in
//! `common/mock_bot_api.rs`. The owner's Node coordinator holds the only
//! legitimate long poll on the real token; no test in this crate may point at
//! api.telegram.org.
//!
//! The unit tests in `commands.rs` pin the planner's decisions. This file
//! pins the WIRE: method names, `parse_mode` presence, `reply_markup`
//! presence, and the fact that a state change is persisted before the reply
//! goes out.

#[path = "common/mock_bot_api.rs"]
mod mock;

use indexmap::IndexMap;
use mock::{MockApi, Reply};
use serde_json::{json, Value};
use stackhour_bridge::commands::{self, Action, CommandEnv, ShipCfg, StopOutcome};
use stackhour_bridge::state::{self, BridgeState};
use stackhour_bridge::telegram::{Tg, TgConfig};
use stackhour_core::registry::{self, Registry};

const CHAT: i64 = 4242;

fn tg(api: &MockApi) -> Tg {
    let mut cfg = TgConfig::new("test-token", CHAT).with_api_root(api.base.clone());
    cfg.backoff_base_ms = 1;
    cfg.retry_after_slack_secs = 0;
    Tg::with_config(cfg)
}

fn empty_registry() -> Registry {
    registry::load(std::path::Path::new("/nonexistent/stackhour-commands-e2e"))
}

fn labels() -> IndexMap<String, String> {
    [("gcp", "☁️ GCP"), ("mac", "🖥️ Mac"), ("blort", "🚀 Blort")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn env<'a>(reg: &'a Registry, labels: &'a IndexMap<String, String>) -> CommandEnv<'a> {
    CommandEnv {
        reg,
        target_labels: labels,
        worker_alive: true,
        busy: false,
        ship: ShipCfg::default(),
    }
}

/// The coordinator's executor, reduced to the parts this area owns: perform
/// each planned action against `tg`, persisting state where asked.
///
/// `stop` is what the runtime discovered when it actually tried to stop
/// something; the planner cannot know it.
fn execute(
    tg: &Tg,
    env: &CommandEnv,
    state: &BridgeState,
    dir: &std::path::Path,
    actions: &[Action],
    stop: StopOutcome,
) {
    for action in actions {
        match action {
            Action::SaveState => state.save(dir),
            Action::Send { text, html, keyboard } => {
                let extra = keyboard.clone().map(|kb| json!({ "reply_markup": kb }));
                tg.send_message(text, if *html { Some("HTML") } else { None }, extra.as_ref());
            }
            Action::Stop => {
                tg.send_message(&commands::stop_text(env, stop), None, None);
            }
            Action::AnswerCallback { id, text } => {
                tg.answer_cb_text(id, text.as_deref());
            }
            Action::RefreshKeyboard { message_id, text } => {
                let extra = json!({ "reply_markup": commands::control_keyboard(state, env.reg) });
                tg.edit_message(*message_id, text, None, Some(&extra));
            }
            // The prompt lane and the declarative runner are other areas'.
            Action::RoutePrompt { .. } | Action::RunCommand { .. } | Action::Confirm { .. } => {}
        }
    }
}

/// Plan + execute one typed message, returning the mock's recorded requests.
fn run(api: &MockApi, dir: &std::path::Path, state: &mut BridgeState, text: &str) -> Vec<mock::Recorded> {
    let reg = empty_registry();
    let l = labels();
    let e = env(&reg, &l);
    let actions = commands::plan_text(&e, state, text, Some(11));
    execute(&tg(api), &e, state, dir, &actions, StopOutcome::default());
    api.requests()
}

// ---------------------------------------------------------------------------

/// A switch must send ONE plain message carrying the repainted keyboard —
/// no parse_mode, exactly as `sendMessage(switchEngine(…), undefined, {…})`.
#[test]
fn switching_engine_sends_one_plain_message_with_the_new_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let mut state = BridgeState::default();

    let seen = run(&api, dir.path(), &mut state, "/codex");
    assert_eq!(seen.len(), 1, "saw {:?}", api.methods());
    let req = &seen[0];
    assert_eq!(req.method, "sendMessage");
    assert_eq!(req.body["chat_id"], CHAT);
    assert_eq!(req.body["text"], "Switched to Codex on ☁️ GCP. (new session)");
    assert_eq!(req.body["disable_web_page_preview"], true);
    assert!(
        req.body.get("parse_mode").is_none(),
        "a switch reply must carry no parse_mode"
    );
    assert_eq!(
        req.body["reply_markup"]["inline_keyboard"][0][1]["text"],
        "✅ 🛠 Codex"
    );

    // The switch was persisted before the send, so a crash here keeps it.
    assert_eq!(state::load(dir.path()).engine, "codex");
}

/// `/help` is the one reply sent as HTML, and it carries the keyboard.
#[test]
fn help_is_sent_as_html_with_the_control_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let mut state = BridgeState::default();

    let seen = run(&api, dir.path(), &mut state, "/start");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].body["parse_mode"], "HTML");
    let text = seen[0].body["text"].as_str().unwrap();
    assert!(text.starts_with("<b>Claude + Codex bridge</b> (distributed)"));
    assert!(text.contains("🚀 /ship — ship a Blort task (Notion→PR)"));
    assert!(seen[0].body.get("reply_markup").is_some());
}

/// `/new` writes an explicit null AND sends the only keyboard-less reply.
#[test]
fn new_persists_a_cleared_session_and_sends_no_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let mut state = BridgeState::default();
    state.set_session(Some("live-session".into()));
    state.save(dir.path());

    let seen = run(&api, dir.path(), &mut state, "/new");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].body["text"], "🆕 Fresh Claude session on ☁️ GCP.");
    assert!(
        seen[0].body.get("reply_markup").is_none(),
        "/new is the one state command with no keyboard"
    );

    let reloaded = state::load(dir.path());
    assert_eq!(reloaded.session(), None);
    assert_eq!(
        reloaded.sessions.get("gcp:claude"),
        Some(&None),
        "a cleared session is a null, not a removed key"
    );
}

/// The whole point of `/stop`: the reply is assembled from what the runtime
/// actually managed to stop, not from what was asked.
#[test]
fn stop_reports_the_runtime_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let reg = empty_registry();
    let l = labels();
    let e = env(&reg, &l);
    let mut state = BridgeState::default();

    let actions = commands::plan_text(&e, &mut state, "/stop", None);
    execute(
        &tg(&api),
        &e,
        &state,
        dir.path(),
        &actions,
        StopOutcome {
            stopped_local: true,
            cancelled: 1,
            running: 2,
        },
    );
    assert_eq!(
        api.last().body["text"],
        "🛑 Stopped GCP job.\n🛑 Cancelled 1 queued Mac job(s).\n\
         ⚠️ 2 Mac job(s) already running — can't interrupt remotely yet."
    );
    assert!(api.last().body.get("reply_markup").is_none());
}

/// A tapped engine button: answer the query with the JS's hardcoded toast,
/// then repaint the tapped message's markers with its own text unchanged.
#[test]
fn a_tapped_button_answers_then_repaints_the_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let reg = empty_registry();
    let l = labels();
    let e = env(&reg, &l);
    let mut state = BridgeState::default();

    let cb = json!({
        "id": "cb-77",
        "data": "e:codex",
        "message": { "message_id": 501, "text": "🎛 Controls — Claude on ☁️ GCP" },
    });
    let actions = commands::plan_callback(&e, &mut state, &cb);
    execute(
        &tg(&api),
        &e,
        &state,
        dir.path(),
        &actions,
        StopOutcome::default(),
    );

    assert_eq!(
        api.methods(),
        vec!["sendMessage", "answerCallbackQuery", "editMessageText"]
    );

    let answer = api.nth(1);
    assert_eq!(answer.body["callback_query_id"], "cb-77");
    assert_eq!(
        answer.body["text"], "Using Codex",
        "the toast is the JS literal, not the label"
    );
    assert!(
        answer.body.get("show_alert").is_none(),
        "the JS never sets show_alert"
    );

    let edit = api.nth(2);
    assert_eq!(edit.body["message_id"], 501);
    assert_eq!(
        edit.body["text"], "🎛 Controls — Claude on ☁️ GCP",
        "the repaint resends the text UNCHANGED; only reply_markup moves"
    );
    assert!(
        edit.body.get("parse_mode").is_none(),
        "the repaint passes parse_mode undefined"
    );
    assert_eq!(
        edit.body["reply_markup"]["inline_keyboard"][0][1]["text"],
        "✅ 🛠 Codex"
    );
    assert_eq!(state::load(dir.path()).engine, "codex");
}

/// Telegram answers a no-op repaint with "message is not modified"; the
/// transport must swallow it, so tapping the ALREADY-active button is silent
/// rather than an error the user sees.
#[test]
fn re_tapping_the_active_button_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let reg = empty_registry();
    let l = labels();
    let e = env(&reg, &l);
    let mut state = BridgeState::default(); // already claude on gcp

    api.push(Reply::ok(json!({ "message_id": 1 }))); // the switch reply
    api.push(Reply::ok(json!(true))); // answerCallbackQuery
    api.push(Reply::err(400, "Bad Request: message is not modified"));

    let cb = json!({
        "id": "cb-1",
        "data": "e:claude",
        "message": { "message_id": 9, "text": "🧠 Claude on ☁️ GCP" },
    });
    let actions = commands::plan_callback(&e, &mut state, &cb);
    execute(
        &tg(&api),
        &e,
        &state,
        dir.path(),
        &actions,
        StopOutcome::default(),
    );

    // One attempt at the edit: 'not modified' is terminal and unlogged, so
    // the retry ladder must not run.
    assert_eq!(
        api.methods().iter().filter(|m| *m == "editMessageText").count(),
        1
    );
    assert_eq!(state.engine, "claude");
}

/// A stale keyboard from an older bridge still dismisses the spinner and
/// changes nothing.
#[test]
fn an_unknown_callback_only_dismisses_the_spinner() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let reg = empty_registry();
    let l = labels();
    let e = env(&reg, &l);
    let mut state = BridgeState::default();

    let cb = json!({ "id": "cb-x", "data": "z:gone", "message": { "message_id": 3, "text": "🧠 x" } });
    let actions = commands::plan_callback(&e, &mut state, &cb);
    execute(
        &tg(&api),
        &e,
        &state,
        dir.path(),
        &actions,
        StopOutcome::default(),
    );

    assert_eq!(api.methods(), vec!["answerCallbackQuery"]);
    assert!(
        api.last().body.get("text").is_none(),
        "the default branch answers with no text"
    );
}

/// `/ship <task>` parks the user and hands the engine the whole instruction,
/// with the task's case intact — lowercasing would corrupt ticket ids.
#[test]
fn ship_parks_the_target_and_routes_the_task_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let reg = empty_registry();
    let l = labels();
    let e = env(&reg, &l);
    let mut state = BridgeState::default();

    let actions = commands::plan_text(&e, &mut state, "/Ship ECM-4821 fix CSV export", Some(11));
    execute(
        &tg(&api),
        &e,
        &state,
        dir.path(),
        &actions,
        StopOutcome::default(),
    );

    // Nothing is SENT: the task goes to the engine, not the chat.
    assert!(api.requests().is_empty(), "saw {:?}", api.methods());
    assert_eq!(
        actions[1],
        Action::RoutePrompt {
            text: "/ship ECM-4821 fix CSV export".into(),
            message_id: Some(11),
        }
    );
    let reloaded = state::load(dir.path());
    assert_eq!(
        (reloaded.active.as_str(), reloaded.engine.as_str()),
        ("blort", "claude")
    );
}

/// A bare `/ship` still parks — there is no `/unship` — and the keyboard it
/// sends shows NEITHER Mac nor GCP checked.
#[test]
fn a_bare_ship_parks_and_leaves_both_targets_unmarked() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let mut state = BridgeState::default();

    let seen = run(&api, dir.path(), &mut state, "/ship");
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].body["text"],
        "🚀 Ship mode: Claude on the Blort repo. Send the task (text, ECM-xxxx, or a Slack link)."
    );
    let row = &seen[0].body["reply_markup"]["inline_keyboard"][1];
    assert_eq!(row[0]["text"], "🖥️ Mac");
    assert_eq!(row[1]["text"], "☁️ GCP");
    assert_eq!(state::load(dir.path()).active, "blort");
}

/// `/where` names a target the registry does not know about, because labels
/// come from config rather than a hardcoded pair.
#[test]
fn where_renders_a_config_only_target() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let mut state = BridgeState::default();

    run(&api, dir.path(), &mut state, "/ship");
    api.clear();
    let seen = run(&api, dir.path(), &mut state, "/status");
    assert_eq!(
        seen[0].body["text"],
        "Engine: Claude\nTarget: 🚀 Blort\nSession: none (fresh)\nMac worker: online\nGCP busy: no"
    );
}

/// A failed send must not roll back the state change: the JS switches first
/// and sends second, so the user's next command sees the new selection even
/// though they never got a confirmation.
#[test]
fn a_failed_send_still_leaves_the_state_switched() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let reg = empty_registry();
    let l = labels();
    let e = env(&reg, &l);
    let mut state = BridgeState::default();

    api.set_default(Reply::err(500, "Internal Server Error"));
    let actions = commands::plan_text(&e, &mut state, "/mac", None);
    execute(
        &tg(&api),
        &e,
        &state,
        dir.path(),
        &actions,
        StopOutcome::default(),
    );

    assert_eq!(state::load(dir.path()).active, "mac");
    // 500 is retried the full ladder and then swallowed — never fatal.
    assert_eq!(api.request_count(), 5);
}

/// The registration payload the coordinator sends at startup, on the wire.
#[test]
fn set_my_commands_sends_the_ten_entry_node_payload() {
    let api = MockApi::start();
    let reg = empty_registry();
    api.push(Reply::ok(Value::Bool(true)));
    // NOTE: `set_my_commands` adds the `{ "commands": … }` wrapper itself,
    // so it takes the ARRAY. Handing it the wrapped body double-wraps and
    // Telegram 400s — which tg() swallows, silently leaving the bot with no
    // registered commands.
    tg(&api).set_my_commands(commands::my_commands_list(&reg));

    let req = api.nth(0);
    assert_eq!(req.method, "setMyCommands");
    let names: Vec<&str> = req.body["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["command"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["claude", "codex", "mac", "gcp", "ship", "where", "new", "stop", "menu", "help"]
    );
    // '&' is RAW here: setMyCommands is not HTML.
    assert_eq!(
        req.body["commands"][5]["description"],
        "Show active target & session"
    );
}
