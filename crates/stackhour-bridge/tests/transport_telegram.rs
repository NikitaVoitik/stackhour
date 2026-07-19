//! Transport parity tests for [`stackhour_bridge::telegram::Tg`].
//!
//! Every one of these runs against the local mock in `common/mock_bot_api.rs`.
//! No test in this file may ever be pointed at the real Bot API: the owner's
//! Node coordinator holds the only legitimate long poll on that token.
//!
//! The retry backoff is dialled down to 1ms so the full 5-attempt ladder runs
//! in milliseconds instead of five seconds; the SHAPE of the ladder is what is
//! under test, and the real values are asserted separately as constants in
//! telegram.rs.

#[path = "common/mock_bot_api.rs"]
mod mock;

use mock::{MockApi, Reply};
use serde_json::{json, Value};
use stackhour_bridge::telegram::{Tg, TgConfig};

const CHAT: i64 = 4242;

fn tg(api: &MockApi) -> Tg {
    let mut cfg = TgConfig::new("test-token", CHAT).with_api_root(api.base.clone());
    cfg.backoff_base_ms = 1;
    cfg.retry_after_slack_secs = 0;
    cfg.long_poll_secs = 1;
    cfg.long_poll_read_timeout_secs = 3;
    Tg::with_config(cfg)
}

// ---- the retry ladder ----

#[test]
fn ok_returns_the_result_not_the_envelope() {
    let api = MockApi::start();
    api.push(Reply::ok(json!({ "message_id": 77, "text": "hi" })));
    let out = tg(&api).send_message("hi", None, None).expect("result");
    // Callers do `m.message_id`, so `result` must be unwrapped for them.
    assert_eq!(out["message_id"], 77);
    assert!(out.get("ok").is_none());
    assert_eq!(api.request_count(), 1);
}

#[test]
fn a_400_is_terminal_and_never_retried() {
    let api = MockApi::start();
    api.push(Reply::err(400, "Bad Request: message text is empty"));
    assert!(tg(&api).send_message("", None, None).is_none());
    assert_eq!(api.request_count(), 1, "400 must not retry");
}

#[test]
fn a_404_is_terminal_and_never_retried() {
    let api = MockApi::start();
    api.push(Reply::err(404, "Not Found"));
    assert!(tg(&api).delete(5).is_none());
    assert_eq!(api.request_count(), 1);
}

#[test]
fn not_modified_is_swallowed_before_the_400_branch() {
    // This is what makes the status-message deduper and the callback keyboard
    // refresh free: no retry, no log line, just None.
    let api = MockApi::start();
    api.push(Reply::err(
        400,
        "Bad Request: message is not modified: specified new message content and reply markup are exactly the same",
    ));
    assert!(tg(&api).edit_message(9, "same", None, None).is_none());
    assert_eq!(api.request_count(), 1);
}

#[test]
fn a_500_is_retried_the_full_five_attempts_then_gives_up() {
    let api = MockApi::start();
    api.push_n(5, Reply::err(500, "Internal Server Error"));
    assert!(tg(&api).send_message("x", None, None).is_none());
    assert_eq!(api.request_count(), 5);
}

#[test]
fn a_409_from_a_duplicate_poller_retries_rather_than_exiting() {
    // A second getUpdates poller makes Telegram answer 409. The reference
    // treats it as an ordinary error: retry five times, log, carry on. It
    // does NOT exit, which is exactly how a duplicate poller silently
    // degrades the bridge.
    let api = MockApi::start();
    api.push_n(5, Reply::err(409, "Conflict: terminated by other getUpdates request"));
    assert!(tg(&api).get_updates(0).is_none());
    assert_eq!(api.request_count(), 5);
}

#[test]
fn a_transient_error_recovers_on_a_later_attempt() {
    let api = MockApi::start();
    api.push(Reply::err(500, "boom"));
    api.push(Reply::err(502, "bad gateway"));
    api.push(Reply::ok(json!({ "message_id": 3 })));
    let out = tg(&api).send_message("x", None, None).expect("recovered");
    assert_eq!(out["message_id"], 3);
    assert_eq!(api.request_count(), 3);
}

#[test]
fn a_non_json_body_is_an_error_and_is_retried() {
    let api = MockApi::start();
    api.push_n(5, Reply::garbage(502));
    assert!(tg(&api).send_message("x", None, None).is_none());
    assert_eq!(api.request_count(), 5);
}

#[test]
fn a_429_with_retry_after_sleeps_and_consumes_an_attempt() {
    let api = MockApi::start();
    api.push(Reply::rate_limited(0));
    api.push(Reply::ok(json!({ "message_id": 8 })));
    let out = tg(&api).send_message("x", None, None).expect("sent");
    assert_eq!(out["message_id"], 8);
    assert_eq!(api.request_count(), 2);
}

#[test]
fn a_429_on_every_attempt_falls_out_of_the_loop_with_no_extra_request() {
    // Five 429s = five attempts consumed. The reference returns `undefined`
    // here with no log line; the port returns None.
    let api = MockApi::start();
    api.push_n(5, Reply::rate_limited(0));
    assert!(tg(&api).send_message("x", None, None).is_none());
    assert_eq!(api.request_count(), 5);
}

#[test]
fn a_429_without_retry_after_takes_the_error_ladder_instead() {
    // Arguably a reference bug — it gets the aggressive 500/1000/1500/2000
    // backoff rather than Telegram's requested wait — but it is what the live
    // service does, so it is preserved.
    let api = MockApi::start();
    api.push_n(5, Reply::err(429, "Too Many Requests"));
    assert!(tg(&api).send_message("x", None, None).is_none());
    assert_eq!(api.request_count(), 5);
}

#[test]
fn the_transport_never_panics_when_the_server_is_simply_absent() {
    // A dead API must degrade to None, not take the daemon down.
    let mut cfg = TgConfig::new("t", CHAT).with_api_root("http://127.0.0.1:1".to_string());
    cfg.backoff_base_ms = 1;
    let tg = Tg::with_config(cfg);
    assert!(tg.send_message("x", None, None).is_none());
}

// ---- request shapes ----

#[test]
fn send_message_always_disables_the_web_page_preview_and_omits_a_null_parse_mode() {
    let api = MockApi::start();
    tg(&api).send_message("hello", None, None);
    let body = api.last().body;
    assert_eq!(body["chat_id"], CHAT);
    assert_eq!(body["text"], "hello");
    assert_eq!(body["disable_web_page_preview"], true);
    assert!(
        body.get("parse_mode").is_none(),
        "parse_mode must be absent, never null: {body}"
    );
}

#[test]
fn send_message_extras_are_merged_last_and_win() {
    let api = MockApi::start();
    tg(&api).send_message(
        "hello",
        Some("HTML"),
        Some(&json!({ "reply_markup": { "inline_keyboard": [] }, "disable_web_page_preview": false })),
    );
    let body = api.last().body;
    assert_eq!(body["parse_mode"], "HTML");
    assert_eq!(body["reply_markup"]["inline_keyboard"], json!([]));
    assert_eq!(
        body["disable_web_page_preview"], false,
        "extra is spread last, so it overrides earlier fields"
    );
}

#[test]
fn edit_message_mirrors_send_message() {
    let api = MockApi::start();
    tg(&api).edit_message(12, "text", Some("HTML"), Some(&json!({ "reply_markup": 1 })));
    let req = api.last();
    assert_eq!(req.method, "editMessageText");
    assert_eq!(req.body["message_id"], 12);
    assert_eq!(req.body["chat_id"], CHAT);
    assert_eq!(req.body["disable_web_page_preview"], true);
    assert_eq!(req.body["parse_mode"], "HTML");
    assert_eq!(req.body["reply_markup"], 1);
}

#[test]
fn the_fire_and_forget_calls_have_the_reference_shapes() {
    let api = MockApi::start();
    let tg = tg(&api);
    tg.delete(3);
    tg.typing();
    tg.react_eyes(9);
    tg.answer_cb_text("cb1", Some("Stopping…"));
    tg.answer_cb("cb2");

    let reqs = api.requests();
    assert_eq!(
        api.methods(),
        vec![
            "deleteMessage",
            "sendChatAction",
            "setMessageReaction",
            "answerCallbackQuery",
            "answerCallbackQuery"
        ]
    );
    assert_eq!(reqs[0].body, json!({ "chat_id": CHAT, "message_id": 3 }));
    assert_eq!(reqs[1].body, json!({ "chat_id": CHAT, "action": "typing" }));
    assert_eq!(
        reqs[2].body,
        json!({ "chat_id": CHAT, "message_id": 9, "reaction": [{ "type": "emoji", "emoji": "👀" }] })
    );
    assert_eq!(
        reqs[3].body,
        json!({ "callback_query_id": "cb1", "text": "Stopping…" })
    );
    // No text, no show_alert, no cache_time.
    assert_eq!(reqs[4].body, json!({ "callback_query_id": "cb2" }));
}

#[test]
fn clearing_a_reaction_sends_an_empty_array() {
    let api = MockApi::start();
    tg(&api).react(9, "");
    assert_eq!(api.last().body["reaction"], json!([]));
}

#[test]
fn get_updates_long_polls_with_the_reference_parameters() {
    let api = MockApi::start();
    api.push(Reply::ok(json!([])));
    let cfg = TgConfig::new("t", CHAT).with_api_root(api.base.clone());
    let tg = Tg::with_config(cfg);
    tg.get_updates(1234);
    let body = api.last().body;
    assert_eq!(body["offset"], 1234);
    assert_eq!(body["timeout"], 50);
    assert_eq!(
        body["allowed_updates"],
        json!(["message", "edited_message", "callback_query"]),
        "edited_message is accepted and treated exactly like message"
    );
}

#[test]
fn set_my_commands_wraps_the_generated_table() {
    let api = MockApi::start();
    let table = json!([{ "command": "help", "description": "Show command list" }]);
    tg(&api).set_my_commands(table.clone());
    let req = api.last();
    assert_eq!(req.method, "setMyCommands");
    assert_eq!(req.body, json!({ "commands": table }));
}

// ---- sendRichMessage ----

#[test]
fn send_rich_returns_immediately_when_the_rich_method_is_accepted() {
    let api = MockApi::start();
    api.push(Reply::ok(json!({ "message_id": 55 })));
    let out = tg(&api)
        .send_rich("# hi", Some(json!({ "reply_markup": 1 })))
        .expect("rich sent");
    assert_eq!(out["message_id"], 55);
    assert_eq!(api.request_count(), 1);
    let body = api.last().body;
    assert_eq!(body["rich_message"]["markdown"], "# hi");
    assert_eq!(body["reply_markup"], 1);
}

#[test]
fn send_rich_retries_once_without_the_markup_when_extras_were_supplied() {
    let api = MockApi::start();
    api.push(Reply::err(400, "Bad Request: reply_markup not supported"));
    api.push(Reply::ok(json!({ "message_id": 56 })));
    let out = tg(&api)
        .send_rich("body", Some(json!({ "reply_markup": 1 })))
        .expect("second try");
    assert_eq!(out["message_id"], 56);
    assert_eq!(api.request_count(), 2);
    // The consequence: this message arrives with NO control keyboard.
    assert!(
        api.nth(1).body.get("reply_markup").is_none(),
        "the retry must drop the markup"
    );
}

#[test]
fn send_rich_does_not_retry_when_there_were_no_extras() {
    let api = MockApi::start();
    api.push(Reply::err(400, "Bad Request: unknown method"));
    assert!(tg(&api).send_rich("body", None).is_none());
    assert_eq!(api.request_count(), 1);
}

#[test]
fn send_rich_against_a_standard_api_costs_exactly_two_400s_and_then_gives_up() {
    // The real-world path today: sendRichMessage does not exist, both calls
    // 400, and the CALLER falls back to chunked HTML. The wasted 400s are
    // reproduced on purpose so the rich path lights up by itself the day the
    // account gets the method.
    let api = MockApi::start();
    api.set_default(Reply::err(400, "Bad Request: method not found"));
    assert!(tg(&api)
        .send_rich("body", Some(json!({ "reply_markup": 1 })))
        .is_none());
    assert_eq!(api.request_count(), 2);
    assert_eq!(api.methods(), vec!["sendRichMessage", "sendRichMessage"]);
}

// ---- file endpoints ----

#[test]
fn get_file_and_download_use_the_two_separate_api_bases() {
    let api = MockApi::start();
    api.push(Reply::ok(
        json!({ "file_id": "abc", "file_path": "photos/file_1.jpg", "file_size": 12 }),
    ));
    api.push(Reply::raw(200, "PAYLOAD"));
    let tg = tg(&api);
    let meta = tg.get_file("abc").expect("meta");
    let path = meta["file_path"].as_str().unwrap();
    let bytes = tg.download(path).expect("bytes");
    assert_eq!(bytes, b"PAYLOAD");
    assert_eq!(api.nth(0).path, "/bottest-token/getFile");
    assert_eq!(api.nth(1).path, "/file/bottest-token/photos/file_1.jpg");
}

#[test]
fn a_download_of_an_oversized_file_surfaces_as_a_getfile_400() {
    // Telegram's own 20MB getFile cap dominates any configured byte limit:
    // the 400 is swallowed by the retry ladder and the caller sees None, then
    // reports the generic "did not return a downloadable file path" message.
    let api = MockApi::start();
    api.push(Reply::err(400, "Bad Request: file is too big"));
    assert!(tg(&api).get_file("huge").is_none());
    assert_eq!(api.request_count(), 1);
}

#[test]
fn a_non_2xx_download_yields_none_rather_than_a_body() {
    let api = MockApi::start();
    api.push(Reply::raw(500, "nope"));
    assert!(tg(&api).download("photos/x.jpg").is_none());
}

// ---- the whole thing degrades to None, never a panic ----

#[test]
fn every_public_call_returns_none_against_a_failing_api_without_panicking() {
    let api = MockApi::start();
    api.set_default(Reply::err(500, "down"));
    let tg = tg(&api);
    let outcomes: Vec<Option<Value>> = vec![
        tg.send_message("a", None, None),
        tg.edit_message(1, "a", None, None),
        tg.delete(1),
        tg.react_eyes(1),
        tg.typing(),
        tg.answer_cb("x"),
        tg.set_my_commands(json!([])),
        tg.get_file("f"),
        tg.send_rich("m", None),
        tg.get_updates(0),
    ];
    assert!(outcomes.iter().all(Option::is_none));
}
