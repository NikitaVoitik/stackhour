//! THE integration proof: one Telegram update walks the entire bridge.
//!
//! Every other test in this crate exercises one area against a double. This
//! one wires the real `Runtime` — the real config loader, the real registry,
//! the real command table, the real `Action` interpreter, the real local lane,
//! the real engine runner and the real transport — and pushes a single update
//! in at the top:
//!
//!   getUpdates payload -> chat gate -> command table -> config-declared
//!   command -> target + engine selection -> spawn -> streamed status (ONE
//!   message, edited) -> final rendered delivery
//!
//! Nothing in Rust knows the names `greet` or `fake-echo`; both arrive as
//! files in a config directory, which is the point. If this test needs a
//! source change to pass, the areas have stopped composing.
//!
//! SAFETY: the transport points at the in-process mock in
//! `common/mock_bot_api.rs`. The owner's Node coordinator holds the only
//! legitimate poll on the real token, and this file must never be pointed at
//! it — the token below is a fake and the API root is loopback.

#[path = "common/mock_bot_api.rs"]
mod mock;

use mock::{MockApi, Reply};
use serde_json::{json, Value};
use stackhour_bridge::coordinator::Runtime;
use stackhour_bridge::registry_ctx::RegistryCtx;
use stackhour_bridge::telegram::{Tg, TgConfig};
use stackhour_bridge::BridgePaths;
use std::fs;
use std::path::Path;

const CHAT: i64 = 4242;
/// Obviously fake. The real token lives in the owner's config and never here.
const TOKEN: &str = "test-token-not-a-real-bot";

/// A config-declared engine. No Rust source mentions `fake-echo`.
const ENGINE_TOML: &str = r#"
label = "Fake Echo"
emoji = "🧪"
bin = "fake-echo"
kind = "plain-lines"
args = ["--run", "-"]
# The house rules ride a flag rather than being prepended to the prompt, so
# the prompt the engine reads on stdin is exactly what the command rendered.
system_prompt_args = ["--system", "{{system_prompt}}"]
"#;

/// The engine binary: reads the prompt on stdin, emits a couple of status
/// lines, then the answer. `plain-lines` treats the last line as the result.
const ENGINE_SH: &str = r#"#!/bin/sh
prompt=$(cat)
echo "thinking about it"
echo "still working"
echo "Hello, $prompt!"
"#;

/// A config-declared command. `kind = "prompt"` renders a template and routes
/// the result exactly like typed text.
const COMMAND_TOML: &str = r#"
description = "Greet somebody"
kind = "prompt"
template = "greet"
"#;

const GREET_MD: &str = "{{args}}";

/// A target's `extraPath` REPLACES the child's whole PATH (coordinator.mjs
/// spawns with `PATH: tgt.extraPath || process.env.PATH`), so a target that
/// wants the system tools has to list them — as the real config does. The
/// fake engine is a shell script that shells out, so it needs them.
fn target_path(bin_dir: &Path) -> String {
    match std::env::var("PATH") {
        Ok(p) if !p.is_empty() => format!("{}:{p}", bin_dir.display()),
        _ => bin_dir.display().to_string(),
    }
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
    fs::write(&path, body).expect("write");
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

struct Fixture {
    _config: tempfile::TempDir,
    _runtime: tempfile::TempDir,
    api: MockApi,
    rt: Runtime,
}

/// Build the whole bridge around a temp config dir and a temp runtime dir.
fn fixture() -> Fixture {
    let config = tempfile::tempdir().expect("config tempdir");
    let runtime = tempfile::tempdir().expect("runtime tempdir");

    // The config-only extension surface: an engine, its binary, a command and
    // the prompt template that command renders.
    write(config.path(), "engines/fake-echo.toml", ENGINE_TOML);
    write(config.path(), "commands/greet.toml", COMMAND_TOML);
    write(config.path(), "prompts/greet.md", GREET_MD);
    let bin_dir = config.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("mkdir bin");
    let script = bin_dir.join("fake-echo");
    fs::write(&script, ENGINE_SH).expect("write script");
    make_executable(&script);

    // The coordinator config, in the shape the real loader parses. `extraPath`
    // becomes the child's whole PATH, which is how the engine binary is found.
    write(
        runtime.path(),
        "config.json",
        &json!({
            "token": TOKEN,
            "chatId": CHAT,
            "defaultTarget": "gcp",
            "targets": {
                "gcp": {
                    "label": "☁️ GCP",
                    "type": "local",
                    "cwd": runtime.path().to_str().unwrap(),
                    "extraPath": target_path(&bin_dir),
                },
                "mac": { "label": "🖥️ Mac", "type": "remote" },
            },
        })
        .to_string(),
    );
    // Park the conversation on the config-declared engine.
    write(
        runtime.path(),
        "state.json",
        &json!({ "offset": 0, "active": "gcp", "engine": "fake-echo" }).to_string(),
    );

    let api = MockApi::start();
    let paths = BridgePaths::from_runtime_dir(runtime.path());
    paths.ensure_dirs().expect("ensure dirs");
    let cfg = stackhour_bridge::config::load_coordinator_cfg(&paths.config_path).expect("config loads");

    let reg = stackhour_core::registry::load_with(
        config.path(),
        stackhour_core::registry::EnvSource::fixed(&[]),
    );
    assert!(
        reg.errors.is_empty(),
        "the fixture config must be valid: {:?}",
        reg.errors
    );

    let tg = Tg::with_config(
        TgConfig::new(TOKEN, CHAT).with_api_root(&api.base),
    );
    let rt = Runtime::new(cfg, paths, tg, RegistryCtx::from_registry(reg));

    Fixture {
        _config: config,
        _runtime: runtime,
        api,
        rt,
    }
}

/// One inbound text update, as getUpdates would deliver it.
fn text_update(update_id: i64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": 77,
            "chat": { "id": CHAT },
            "from": { "id": 1, "is_bot": false },
            "text": text,
        }
    })
}

/// Wait for the local lane to go idle — the lane drains on its own thread.
fn settle(rt: &Runtime) {
    for _ in 0..600 {
        if !rt.local.is_busy() && rt.local.queued() == 0 {
            // One more beat so the final delivery lands.
            std::thread::sleep(std::time::Duration::from_millis(50));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("the local lane never went idle");
}

/// The whole path, end to end.
#[test]
fn a_telegram_update_reaches_a_config_declared_command_and_comes_back_rendered() {
    let f = fixture();

    f.rt.handle_update(&text_update(1, "/greet world"));
    settle(&f.rt);

    let methods = f.api.methods();

    // 1. Exactly ONE status message is created, then EDITED in place. A
    //    second sendMessage for status would spam the chat — that single
    //    message being edited is the whole point of the status lifecycle.
    let sends = methods.iter().filter(|m| *m == "sendMessage").count();
    let edits = methods.iter().filter(|m| *m == "editMessageText").count();
    assert!(
        edits >= 1,
        "the status message was never edited; saw {methods:?}"
    );
    assert!(
        sends <= 2,
        "status should be edited, not resent: {sends} sendMessage calls in {methods:?}"
    );

    // 2. The engine actually ran and its output came back. The prompt
    //    template rendered `{{args}}` -> "world", the engine echoed it, and
    //    the final delivery carries the answer.
    let bodies: Vec<String> = f
        .api
        .requests()
        .iter()
        .filter(|r| matches!(r.method.as_str(), "sendMessage" | "editMessageText" | "sendRichMessage"))
        .map(|r| {
            let v = &r.body;
            v["text"]
                .as_str()
                .or_else(|| v.pointer("/rich_message/markdown").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert!(
        bodies.iter().any(|b| b.contains("Hello, world!")),
        "the engine's answer never reached the chat; bodies: {bodies:?}"
    );

    // 3. The status line named the config-declared engine and target, which
    //    means selection resolved through the registry and the config rather
    //    than a hardcoded default.
    assert!(
        bodies.iter().any(|b| b.contains("Fake Echo")),
        "the config-declared engine label never appeared: {bodies:?}"
    );
    assert!(
        bodies.iter().any(|b| b.contains("GCP")),
        "the config-declared target label never appeared: {bodies:?}"
    );
}

/// Plain typed text carries the user's message id all the way to
/// `routePrompt`, which reacts 👀 so the user knows the bridge heard them.
///
/// A config-declared command does NOT: the planner's `Action::RunCommand`
/// carries only the command and its raw args, so the message id is dropped
/// before the runner sees it. That is a real (small) inconsistency rather than
/// a deliberate parity choice — the JS had no declarative commands to compare
/// against — and fixing it means widening the Action, which belongs to the
/// command-surface area. Pinned here so the difference is visible.
#[test]
fn plain_text_is_reacted_to_but_a_declarative_command_is_not() {
    let f = fixture();
    f.rt.handle_update(&text_update(1, "just do the thing"));
    settle(&f.rt);
    assert!(
        f.api.methods().iter().any(|m| m == "setMessageReaction"),
        "plain text was never reacted to; saw {:?}",
        f.api.methods()
    );

    let g = fixture();
    g.rt.handle_update(&text_update(1, "/greet world"));
    settle(&g.rt);
    assert!(
        !g.api.methods().iter().any(|m| m == "setMessageReaction"),
        "a declarative command reacted; the Action must have grown a message id, \
         so update the comment above and assert the new behaviour"
    );
}

/// The intermediate status lines the engine emitted are shown while it runs.
/// This is what makes a long job bearable on a phone, and it only works if
/// the engine's stream reaches the lane's status editor.
#[test]
fn engine_progress_is_streamed_into_the_status_message_while_it_runs() {
    let f = fixture();

    f.rt.handle_update(&text_update(1, "/greet world"));
    settle(&f.rt);

    let edited: Vec<String> = f
        .api
        .requests()
        .iter()
        .filter(|r| r.method == "editMessageText")
        .map(|r| r.body["text"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        !edited.is_empty(),
        "nothing was ever streamed; methods: {:?}",
        f.api.methods()
    );

    // Every status edit targets the SAME message id.
    let ids: Vec<i64> = f
        .api
        .requests()
        .iter()
        .filter(|r| r.method == "editMessageText")
        .filter_map(|r| r.body["message_id"].as_i64())
        .collect();
    if let Some(first) = ids.first() {
        assert!(
            ids.iter().all(|id| id == first),
            "status edits jumped between messages: {ids:?}"
        );
    }
}

/// A message from another chat must not reach the command table at all. The
/// bridge can run shell commands, so this gate is the security boundary.
#[test]
fn an_update_from_another_chat_is_dropped_before_anything_runs() {
    let f = fixture();

    let mut u = text_update(1, "/greet world");
    u["message"]["chat"]["id"] = json!(CHAT + 1);
    f.rt.handle_update(&u);

    // Nothing was sent, nothing was reacted to, nothing was run.
    assert_eq!(
        f.api.request_count(),
        0,
        "a foreign chat produced Telegram traffic: {:?}",
        f.api.methods()
    );
    assert!(!f.rt.local.is_busy(), "a foreign chat started a job");
}

/// A bot's own message must not loop back into the bridge.
#[test]
fn an_update_from_a_bot_is_dropped() {
    let f = fixture();

    let mut u = text_update(1, "/greet world");
    u["message"]["from"]["is_bot"] = json!(true);
    f.rt.handle_update(&u);

    assert_eq!(f.api.request_count(), 0, "a bot message was handled");
}

/// An unknown slash command is answered rather than routed to an engine —
/// otherwise a typo silently burns a model call.
#[test]
fn an_unknown_command_is_answered_and_never_reaches_the_engine() {
    let f = fixture();
    f.api.set_default(Reply::ok(json!({ "message_id": 9 })));

    f.rt.handle_update(&text_update(1, "/definitely-not-a-command"));

    assert!(!f.rt.local.is_busy(), "an unknown command started a job");
    let bodies: Vec<String> = f
        .api
        .requests()
        .iter()
        .filter(|r| r.method == "sendMessage")
        .map(|r| r.body["text"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        !bodies.is_empty(),
        "the user got no answer; methods: {:?}",
        f.api.methods()
    );
}

/// A state-changing command mutates the shared state AND persists it, so a
/// restart does not forget which engine the conversation is on.
#[test]
fn a_target_switch_is_applied_and_persisted_through_the_real_state_store() {
    let f = fixture();

    f.rt.handle_update(&text_update(1, "/mac"));

    let on_disk: Value = serde_json::from_str(
        &fs::read_to_string(f._runtime.path().join("state.json")).expect("state.json exists"),
    )
    .expect("state.json parses");
    assert_eq!(
        on_disk["active"], "mac",
        "the switch never reached disk: {on_disk}"
    );
}
