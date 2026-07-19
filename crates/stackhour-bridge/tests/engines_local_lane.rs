//! Parity tests for the ENGINES area: argv byte-order, the standing house
//! rules, and the local lane's status-message + deliverFinal lifecycle.
//!
//! SAFETY: every Telegram call here goes to the local mock in
//! `common/mock_bot_api.rs`. The owner's Node coordinator holds the only
//! legitimate poll on the real token and nothing in this file may ever be
//! pointed at it.
//!
//! The reference for every assertion is
//! `/home/nikita/.claude-remote/coordinator.mjs` lines 211-301 (spawnLocal,
//! runLocal, drainLocal) and 199-208 (deliverFinal).

#[path = "common/mock_bot_api.rs"]
mod mock;

use mock::{MockApi, Reply};
use serde_json::{json, Value};
use stackhour_bridge::engines::{self, RunRequest};
use stackhour_bridge::local_lane::{deliver_final, LaneContext, LocalJob, LocalLane, LocalTarget};
use stackhour_bridge::telegram::{Tg, TgConfig};
use stackhour_core::registry::{EngineDef, Registry};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

const CHAT: i64 = 4242;

fn registry() -> Registry {
    stackhour_core::registry::load_with(
        std::path::Path::new("/nonexistent-stackhour-config"),
        stackhour_core::registry::EnvSource::fixed(&[]),
    )
}

// ---------------------------------------------------------------------------
// argv byte-order
// ---------------------------------------------------------------------------

/// coordinator.mjs emits, in this order:
/// `-p --output-format stream-json --verbose --include-partial-messages
///  --permission-mode <m> --append-system-prompt <rules> --model <m>
///  --resume <sid>`.
///
/// The system prompt comes BEFORE the model. Both flags are only ever set
/// together on the default path (every claude turn carries ORWELL_RULES and a
/// configured target usually carries a model), so an order regression here is
/// invisible to any test that sets one at a time.
#[test]
fn claude_argv_puts_the_system_prompt_before_the_model_like_the_js() {
    let reg = registry();
    let claude = reg.engines.get("claude").expect("built-in claude");
    let argv = engines::build_argv(
        claude,
        &RunRequest {
            prompt: "hi".into(),
            session_id: Some("sess-1".into()),
            model: Some("opus".into()),
            permission_mode: Some("bypassPermissions".into()),
            system_prompt: Some("RULES".into()),
            live_status: true,
            ..RunRequest::default()
        },
    );
    assert_eq!(
        argv,
        vec![
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-mode",
            "bypassPermissions",
            "--append-system-prompt",
            "RULES",
            "--model",
            "opus",
            "--resume",
            "sess-1",
        ]
    );
}

/// Codex takes its session id as a subcommand argument before the `-` stdin
/// sentinel, and `bypassPermissions` replaces the sandbox flags wholesale.
#[test]
fn codex_argv_matches_the_js_in_both_permission_modes() {
    let reg = registry();
    let codex = reg.engines.get("codex").expect("built-in codex");

    let fresh = engines::build_argv(
        codex,
        &RunRequest {
            prompt: "hi".into(),
            ..RunRequest::default()
        },
    );
    assert_eq!(
        fresh,
        vec![
            "exec",
            "--json",
            "--skip-git-repo-check",
            "--sandbox",
            "workspace-write",
            "-"
        ]
    );

    let resumed = engines::build_argv(
        codex,
        &RunRequest {
            prompt: "hi".into(),
            session_id: Some("thread-9".into()),
            permission_mode: Some("bypassPermissions".into()),
            model: Some("gpt-x".into()),
            ..RunRequest::default()
        },
    );
    assert_eq!(
        resumed,
        vec![
            "exec",
            "resume",
            "--json",
            "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox",
            "--model",
            "gpt-x",
            "thread-9",
            "-",
        ]
    );
}

// ---------------------------------------------------------------------------
// the standing house rules (ORWELL_RULES)
// ---------------------------------------------------------------------------

/// The JS hands ORWELL_RULES to EVERY claude turn, agent or not. The rules are
/// a registry template now, so this also pins that the shipped body is the
/// seven-line Orwell text byte-for-byte.
#[test]
fn the_house_rules_reach_claude_as_a_system_prompt_on_the_default_path() {
    let reg = registry();
    let claude = reg.engines.get("claude").expect("claude").clone();
    let rules = reg.prompts.render("house-rules", &[]);

    assert_eq!(
        rules,
        "Follow Orwell's writing rules in every reply:\n\
         1. Never use a metaphor, simile, or other figure of speech you are used to seeing in print.\n\
         2. Never use a long word where a short one will do.\n\
         3. If it is possible to cut a word out, always cut it out.\n\
         4. Never use the passive where the active will do.\n\
         5. Never use a foreign phrase, a scientific word, or jargon where plain English will do.\n\
         6. Break any of these rules sooner than say anything outright barbarous."
    );

    let mut req = RunRequest {
        prompt: "hello".into(),
        house_rules: Some(rules.clone()),
        ..RunRequest::default()
    };
    engines::apply_house_rules(&claude, &mut req);
    assert_eq!(req.system_prompt.as_deref(), Some(rules.as_str()));
    assert_eq!(req.prompt, "hello", "claude's prompt is left alone");
}

/// An active agent's composed soul is a deliberate override, so it wins.
#[test]
fn an_agents_system_prompt_wins_over_the_house_rules() {
    let reg = registry();
    let claude = reg.engines.get("claude").expect("claude").clone();
    let mut req = RunRequest {
        prompt: "hello".into(),
        system_prompt: Some("SOUL".into()),
        house_rules: Some("RULES".into()),
        ..RunRequest::default()
    };
    engines::apply_house_rules(&claude, &mut req);
    assert_eq!(req.system_prompt.as_deref(), Some("SOUL"));
}

/// Codex has no `--append-system-prompt`, so the JS prepends
/// '[Standing style rules]\n' + rules + '\n\n' — and ONLY when the attempt is
/// fresh. A resumed thread already carries them.
#[test]
fn codex_gets_the_rules_prepended_on_a_fresh_attempt_only() {
    let reg = registry();
    let codex = reg.engines.get("codex").expect("codex").clone();
    let turn = reg.prompts.render("house-rules-turn", &[]);

    let mut fresh = RunRequest {
        prompt: "do the thing".into(),
        house_rules: Some("RULES".into()),
        house_rules_turn: Some(turn.clone()),
        ..RunRequest::default()
    };
    engines::apply_house_rules(&codex, &mut fresh);
    assert_eq!(fresh.prompt, "[Standing style rules]\nRULES\n\ndo the thing");

    let mut resumed = RunRequest {
        prompt: "do the thing".into(),
        session_id: Some("thread-1".into()),
        house_rules: Some("RULES".into()),
        house_rules_turn: Some(turn),
        ..RunRequest::default()
    };
    engines::apply_house_rules(&codex, &mut resumed);
    assert_eq!(resumed.prompt, "do the thing");
}

/// Emptying `prompts/house-rules.md` is how a user turns them off, so an
/// empty body must be a no-op rather than an empty `--append-system-prompt`.
#[test]
fn empty_house_rules_are_a_no_op() {
    let reg = registry();
    let claude = reg.engines.get("claude").expect("claude").clone();
    let mut req = RunRequest {
        prompt: "hello".into(),
        house_rules: Some("   \n".into()),
        ..RunRequest::default()
    };
    engines::apply_house_rules(&claude, &mut req);
    assert_eq!(req.system_prompt, None);
}

// ---------------------------------------------------------------------------
// the two resume-retry log lines
// ---------------------------------------------------------------------------

/// Two lanes, two strings. The coordinator names the target and says
/// "retrying"; the worker does neither.
#[test]
fn the_two_lanes_have_two_different_resume_retry_log_lines() {
    assert_eq!(
        engines::resume_retry_log_line_local("codex", "gcp", Some(1)),
        "codex resume failed on gcp (1); retrying fresh"
    );
    assert_eq!(
        engines::resume_retry_log_line("codex", Some(1)),
        "codex resume failed (1); retry fresh"
    );
}

// ---------------------------------------------------------------------------
// the local lane against the mock Bot API
// ---------------------------------------------------------------------------

struct Ctx {
    reg: Registry,
    logs: Mutex<Vec<String>>,
    clock: AtomicI64,
}

impl Ctx {
    fn new() -> Arc<Ctx> {
        Arc::new(Ctx {
            reg: registry(),
            logs: Mutex::new(Vec::new()),
            clock: AtomicI64::new(0),
        })
    }
}

impl LaneContext for Ctx {
    fn engine(&self, name: &str) -> Option<EngineDef> {
        self.reg.engines.get(name).cloned()
    }
    fn target(&self, name: &str, _engine: &str) -> Option<LocalTarget> {
        (name == "gcp").then(|| LocalTarget {
            name: "gcp".into(),
            label: "☁️ GCP".into(),
            ..LocalTarget::default()
        })
    }
    fn prompt(&self, name: &str, vars: &[(&str, &str)]) -> String {
        self.reg.prompts.render(name, vars)
    }
    fn control_keyboard(&self) -> Value {
        json!({ "inline_keyboard": [[{ "text": "🆕 New session", "callback_data": "new" }]] })
    }
    fn session(&self, _t: &str, _e: &str, _a: Option<&str>) -> Option<String> {
        None
    }
    fn set_session(&self, _t: &str, _e: &str, _a: Option<&str>, _id: Option<String>) {}
    fn log(&self, line: &str) {
        self.logs.lock().unwrap().push(line.to_string());
    }
    fn now_ms(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
    fn default_target(&self) -> String {
        "gcp".into()
    }
}

fn tg(api: &MockApi) -> Tg {
    let mut cfg = TgConfig::new("test-token", CHAT).with_api_root(api.base.clone());
    cfg.backoff_base_ms = 1;
    cfg.retry_after_slack_secs = 0;
    Tg::with_config(cfg)
}

/// deliverFinal's happy path is not the rich path: `sendRichMessage` is not a
/// real Bot API method, so it 400s and the chunked-HTML fallback is what
/// delivers. The wasted 400 is reproduced deliberately.
///
/// Ordering under test: rich attempt -> deleteMessage -> fallback chunks.
#[test]
fn deliver_final_tries_rich_then_deletes_the_status_then_falls_back() {
    let api = MockApi::start();
    // sendRich retries ONCE without the reply_markup when the first attempt
    // fails and `extra` had keys, so a failed rich send is TWO calls.
    api.push_n(2, Reply::err(400, "Bad Request: method not found"));
    api.push(Reply::ok(json!(true))); // deleteMessage
    api.push(Reply::ok(json!({ "message_id": 9 }))); // sendMessage
    let ctx = Ctx::new();

    deliver_final(&tg(&api), &*ctx, "the answer", Some(77));

    let seen = api.requests();
    let methods: Vec<&str> = seen.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(
        methods,
        vec![
            "sendRichMessage",
            "sendRichMessage",
            "deleteMessage",
            "sendMessage"
        ]
    );
    assert_eq!(seen[0].body["rich_message"]["markdown"], "the answer");
    assert!(
        seen[0].body.get("reply_markup").is_some() && seen[1].body.get("reply_markup").is_none(),
        "the retry drops the keyboard"
    );
    assert_eq!(seen[2].body["message_id"], 77);
    assert_eq!(seen[3].body["text"], "the answer");
    assert_eq!(seen[3].body["parse_mode"], "HTML");
    assert!(
        seen[3].body.get("reply_markup").is_some(),
        "the last chunk carries the control keyboard"
    );
}

/// The status message is deleted even when the rich send SUCCEEDED, and no
/// fallback runs in that case.
#[test]
fn a_successful_rich_send_still_deletes_the_status_and_skips_the_fallback() {
    let api = MockApi::start();
    api.push(Reply::ok(json!({ "message_id": 5 }))); // sendRichMessage
    api.push(Reply::ok(json!(true))); // deleteMessage
    let ctx = Ctx::new();

    deliver_final(&tg(&api), &*ctx, "the answer", Some(77));

    let methods: Vec<String> = api.requests().iter().map(|r| r.method.clone()).collect();
    assert_eq!(methods, vec!["sendRichMessage", "deleteMessage"]);
}

/// Intermediate chunks carry NO keyboard; only the last one does. The split
/// newline stays at the HEAD of the next chunk, so continuations begin with a
/// blank line.
#[test]
fn the_fallback_keyboard_is_on_the_last_chunk_only() {
    let api = MockApi::start();
    api.push_n(2, Reply::err(400, "Bad Request")); // sendRichMessage + its retry
    api.set_default(Reply::ok(json!({ "message_id": 1 })));
    let ctx = Ctx::new();

    let long = format!("{}\n{}", "a".repeat(3799), "b".repeat(100));
    deliver_final(&tg(&api), &*ctx, &long, None);

    let sends: Vec<_> = api
        .requests()
        .into_iter()
        .filter(|r| r.method == "sendMessage")
        .collect();
    assert_eq!(sends.len(), 2, "expected one split");
    assert!(sends[0].body.get("reply_markup").is_none());
    assert!(sends[1].body.get("reply_markup").is_some());
    assert!(
        sends[1].body["text"].as_str().expect("text").starts_with('\n'),
        "the split newline leads the continuation chunk"
    );
}

/// With no status message id there is no deleteMessage at all — the mac lane
/// after a coordinator restart takes this path.
#[test]
fn no_status_id_means_no_delete() {
    let api = MockApi::start();
    api.push_n(2, Reply::err(400, "Bad Request"));
    api.set_default(Reply::ok(json!({ "message_id": 1 })));
    let ctx = Ctx::new();

    deliver_final(&tg(&api), &*ctx, "hi", None);

    assert!(api.requests().iter().all(|r| r.method != "deleteMessage"));
}

/// The whole local lane, end to end, with a real child process: the first
/// status message is PLAIN and unescaped, later renders of the same id are
/// HTML-escaped edits, and the final answer arrives with the footer.
#[test]
fn the_local_lane_runs_a_child_and_delivers_a_footered_answer() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A config-only engine whose plain-lines output is both text and status.
    std::fs::create_dir_all(dir.path().join("engines")).unwrap();
    std::fs::write(
        dir.path().join("engines/echo-lane.toml"),
        "label = \"Echo\"\nbin = \"/bin/cat\"\nkind = \"plain-lines\"\nargs = []\n",
    )
    .unwrap();

    let api = MockApi::start();
    api.set_default(Reply::ok(json!({ "message_id": 31 })));

    struct EchoCtx {
        reg: Registry,
        logs: Mutex<Vec<String>>,
    }
    impl LaneContext for EchoCtx {
        fn engine(&self, name: &str) -> Option<EngineDef> {
            self.reg.engines.get(name).cloned()
        }
        fn target(&self, name: &str, _e: &str) -> Option<LocalTarget> {
            (name == "gcp").then(|| LocalTarget {
                name: "gcp".into(),
                label: "☁️ GCP".into(),
                ..LocalTarget::default()
            })
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
        fn set_session(&self, _t: &str, _e: &str, _a: Option<&str>, _id: Option<String>) {}
        fn log(&self, line: &str) {
            self.logs.lock().unwrap().push(line.into());
        }
        fn now_ms(&self) -> i64 {
            0
        }
        fn default_target(&self) -> String {
            "gcp".into()
        }
    }

    let ctx = Arc::new(EchoCtx {
        reg: stackhour_core::registry::load_with(dir.path(), stackhour_core::registry::EnvSource::fixed(&[])),
        logs: Mutex::new(Vec::new()),
    });
    assert!(ctx.reg.engines.contains_key("echo-lane"), "{:?}", ctx.reg.errors);

    let lane = LocalLane::new(Arc::new(tg(&api)), Arc::clone(&ctx) as Arc<dyn LaneContext>);
    lane.enqueue(LocalJob {
        prompt: "hello from the lane".into(),
        engine: "echo-lane".into(),
        target: "gcp".into(),
        agent: None,
    });
    for _ in 0..300 {
        if !lane.is_busy() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!lane.is_busy(), "the lane never finished");

    let seen = api.requests();
    let first_status = seen
        .iter()
        .find(|r| r.method == "sendMessage")
        .expect("a status message was sent");
    assert_eq!(
        first_status.body["text"], "▹ Echo · ☁️ GCP · working…",
        "the first status render is plain"
    );
    assert!(
        first_status.body.get("parse_mode").is_none(),
        "the FIRST status message carries no parse_mode"
    );
    assert_eq!(
        first_status.body["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
        "stop"
    );

    // Later renders of the SAME message id are HTML edits.
    if let Some(edit) = seen.iter().find(|r| r.method == "editMessageText") {
        assert_eq!(edit.body["message_id"], 31);
        assert_eq!(edit.body["parse_mode"], "HTML");
    }

    // The answer is delivered with the provenance footer.
    let rich = seen
        .iter()
        .find(|r| r.method == "sendRichMessage")
        .expect("deliverFinal ran");
    let delivered = rich.body["rich_message"]["markdown"].as_str().expect("markdown");
    assert!(
        delivered.ends_with("hello from the lane\n\n— Echo · ☁️ GCP · 0s"),
        "the answer must carry the provenance footer:\n{delivered}"
    );
    // `echo-lane` declares no `system_prompt_args`, so — like codex — it gets
    // the standing house rules prepended to the prompt, and `cat` echoes them
    // straight back. That is the config-driven default path working.
    assert!(
        delivered.starts_with("[Standing style rules]\nFollow Orwell's writing rules"),
        "the house rules did not reach a system-prompt-less engine:\n{delivered}"
    );
}
