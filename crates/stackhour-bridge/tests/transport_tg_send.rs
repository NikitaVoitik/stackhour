//! End-to-end tests for `tg-send`, against the local mock Bot API only.
//!
//! The real token is never used here — see `common/mock_bot_api.rs` for why.

#[path = "common/mock_bot_api.rs"]
mod mock;

use mock::{MockApi, Reply};
use serde_json::json;
use stackhour_bridge::tgsend::{parse_args, run, TgSendArgs, TgSendEnv};

const CHAT: i64 = 4242;

fn config_file(dir: &std::path::Path, body: serde_json::Value) -> std::path::PathBuf {
    let path = dir.join("config.json");
    std::fs::write(&path, body.to_string()).expect("write config");
    path
}

/// A config file with the same SHAPE as the owner's, and obviously-fake
/// values. Never copy the real token, chat id or API key into a test.
fn fake_config(dir: &std::path::Path) -> std::path::PathBuf {
    config_file(
        dir,
        json!({
            "token": "000000000:FAKE-TOKEN-FOR-TESTS-ONLY",
            "chatId": CHAT,
            "defaultTarget": "gcp",
            "targets": { "gcp": { "label": "☁️ GCP", "type": "local" } },
            "elevenLabsApiKey": "fake-elevenlabs-key",
        }),
    )
}

fn env(api: &MockApi, cfg: std::path::PathBuf) -> TgSendEnv {
    TgSendEnv {
        config_path: Some(cfg),
        api_root: api.base.clone(),
        stdin: None,
        stderr: Vec::new(),
    }
}

fn args(v: &[&str]) -> TgSendArgs {
    parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

// ---- the happy paths ----

#[test]
fn the_default_path_tries_rich_first_and_stops_there_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::ok(json!({ "message_id": 1 })));
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["hello", "world"]), &mut e), 0);
    assert_eq!(api.methods(), vec!["sendRichMessage"]);
    assert_eq!(api.last().body["rich_message"]["markdown"], "hello world");
    assert_eq!(api.last().body["chat_id"], CHAT);
}

#[test]
fn rich_rejection_falls_through_to_a_plain_send() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::err(400, "Bad Request: method not found"));
    api.push(Reply::ok(json!({ "message_id": 2 })));
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["hi"]), &mut e), 0);
    assert_eq!(api.methods(), vec!["sendRichMessage", "sendMessage"]);
    let body = api.last().body;
    assert_eq!(body["text"], "hi");
    assert_eq!(body["disable_web_page_preview"], true);
    assert!(body.get("parse_mode").is_none());
}

#[test]
fn html_skips_the_rich_attempt_entirely() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::ok(json!({ "message_id": 3 })));
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["--html", "<b>hi</b>"]), &mut e), 0);
    assert_eq!(api.methods(), vec!["sendMessage"]);
    assert_eq!(api.last().body["parse_mode"], "HTML");
}

#[test]
fn from_prefixes_the_text_with_a_bracketed_label() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::ok(json!({ "message_id": 4 })));
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["--from", "GCP", "done"]), &mut e), 0);
    assert_eq!(api.last().body["rich_message"]["markdown"], "[GCP] done");
}

#[test]
fn text_can_come_from_stdin_and_is_trimmed() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::ok(json!({ "message_id": 5 })));
    let mut e = env(&api, fake_config(dir.path()));
    e.stdin = Some("  piped message\n".to_string());
    assert_eq!(run(&args(&[]), &mut e), 0);
    assert_eq!(api.last().body["rich_message"]["markdown"], "piped message");
}

#[test]
fn verbose_reports_the_path_on_stderr_not_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::ok(json!({ "message_id": 6 })));
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["-v", "hi"]), &mut e), 0);
    assert_eq!(e.stderr, vec!["tg-send: sent (rich)"]);

    let api2 = MockApi::start();
    api2.push(Reply::err(400, "no such method"));
    api2.push(Reply::ok(json!({ "message_id": 7 })));
    let mut e2 = env(&api2, fake_config(dir.path()));
    assert_eq!(run(&args(&["-v", "hi"]), &mut e2), 0);
    assert_eq!(e2.stderr, vec!["tg-send: sent (plain)"]);
}

// ---- chunking ----

#[test]
fn a_long_message_is_chunked_at_4000_chars_and_every_part_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::err(400, "no rich")); // force the plain path
    api.set_default(Reply::ok(json!({ "message_id": 1 })));
    let text: String = (0..1200).map(|i| format!("line {i}\n")).collect();
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&[&text]), &mut e), 0);

    let sends: Vec<String> = api
        .requests()
        .into_iter()
        .filter(|r| r.method == "sendMessage")
        .map(|r| r.body["text"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(sends.len() > 1, "expected several chunks, got {}", sends.len());
    assert!(sends.iter().all(|s| s.chars().count() <= 4000));
    // Nothing is lost and nothing is duplicated.
    assert_eq!(sends.concat(), text.trim());
}

// ---- error handling ----

#[test]
fn a_parse_error_disables_html_for_this_and_every_later_chunk() {
    // The reference's `html` flag is module-global: once one chunk fails HTML
    // parsing the rest go out as plain text, so a long message ends up
    // half-formatted. Preserved deliberately.
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::err(400, "Bad Request: can't parse entities"));
    api.set_default(Reply::ok(json!({ "message_id": 1 })));
    let text: String = (0..1200).map(|i| format!("line {i}\n")).collect();
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["--html", &text]), &mut e), 0);

    let modes: Vec<bool> = api
        .requests()
        .into_iter()
        .map(|r| r.body.get("parse_mode").is_some())
        .collect();
    assert_eq!(modes[0], true, "the first attempt is HTML");
    assert!(
        modes[1..].iter().all(|m| !m),
        "every later send must be plain: {modes:?}"
    );
    assert!(e.stderr.is_empty());
}

#[test]
fn a_429_sleeps_and_consumes_an_attempt_on_the_plain_path() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::err(400, "no rich"));
    api.push(Reply::rate_limited(0));
    api.push(Reply::ok(json!({ "message_id": 1 })));
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["hi"]), &mut e), 0);
    assert_eq!(api.request_count(), 3);
}

#[test]
fn a_rich_429_retries_the_rich_call_rather_than_falling_through() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::rate_limited(0));
    api.push(Reply::ok(json!({ "message_id": 1 })));
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["hi"]), &mut e), 0);
    assert_eq!(api.methods(), vec!["sendRichMessage", "sendRichMessage"]);
}

#[test]
fn a_hard_failure_reports_the_description_and_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::err(400, "no rich"));
    api.set_default(Reply::err(403, "Forbidden: bot was blocked by the user"));
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["hi"]), &mut e), 1);
    assert_eq!(
        e.stderr,
        vec!["tg-send: Telegram error: Forbidden: bot was blocked by the user"]
    );
}

#[test]
fn a_failure_with_no_description_reports_the_status_code() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::err(400, "no rich"));
    api.set_default(Reply::garbage(503));
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["hi"]), &mut e), 1);
    assert_eq!(e.stderr, vec!["tg-send: Telegram error: 503"]);
}

// ---- exit codes and their exact stderr strings ----

#[test]
fn an_unreadable_config_exits_2_with_the_path_in_the_message() {
    let api = MockApi::start();
    let missing = std::path::PathBuf::from("/nonexistent/dir/config.json");
    let mut e = env(&api, missing.clone());
    assert_eq!(run(&args(&["hi"]), &mut e), 2);
    assert_eq!(e.stderr.len(), 1);
    assert!(
        e.stderr[0].starts_with(&format!("tg-send: cannot read config at {}: ", missing.display())),
        "got {:?}",
        e.stderr[0]
    );
    assert_eq!(api.request_count(), 0);
}

#[test]
fn a_config_missing_credentials_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let cfg = config_file(dir.path(), json!({ "targets": {} }));
    let mut e = env(&api, cfg);
    assert_eq!(run(&args(&["hi"]), &mut e), 2);
    assert_eq!(e.stderr, vec!["tg-send: config missing token or chatId"]);
}

#[test]
fn no_message_text_at_all_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let mut e = env(&api, fake_config(dir.path()));
    assert_eq!(run(&args(&["--verbose"]), &mut e), 2);
    assert_eq!(
        e.stderr,
        vec!["tg-send: no message text provided (argument or stdin)"]
    );
    assert_eq!(api.request_count(), 0);
}

#[test]
fn empty_stdin_exits_2_rather_than_hanging() {
    // Under systemd/cron with stdin closed this is the path taken.
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    let mut e = env(&api, fake_config(dir.path()));
    e.stdin = Some(String::new());
    assert_eq!(run(&args(&[]), &mut e), 2);
    assert_eq!(
        e.stderr,
        vec!["tg-send: no message text provided (argument or stdin)"]
    );
}

#[test]
fn a_string_chat_id_is_parsed_rather_than_rejected() {
    // The reference compares chatId with `!==` against a JS number, so a
    // quoted id in config.json would silently reject everything. The port
    // parses to i64 instead — a deliberate, reported divergence.
    let dir = tempfile::tempdir().unwrap();
    let api = MockApi::start();
    api.push(Reply::ok(json!({ "message_id": 1 })));
    let cfg = config_file(dir.path(), json!({ "token": "fake", "chatId": "4242" }));
    let mut e = env(&api, cfg);
    assert_eq!(run(&args(&["hi"]), &mut e), 0);
    assert_eq!(api.last().body["chat_id"], 4242);
}
