//! Blocking Telegram Bot API client — the single transport primitive.
//!
//! [`Tg::call`] is a byte-for-byte port of `tg()` in
//! `/home/nikita/.claude-remote/coordinator.mjs` (lines 64-76), including its
//! quirks:
//!
//! 1. `data.ok` -> return `data.result` (the RESULT, never the envelope).
//! 2. HTTP 429 with `parameters.retry_after` -> sleep `(retry_after + 1)s` and
//!    `continue` — which CONSUMES an attempt.
//! 3. a description matching `/not modified/i` -> `None`, silently, with no
//!    log line. This is what makes idempotent `editMessageText` calls free and
//!    keeps the status-message deduper and the keyboard refresh quiet.
//! 4. HTTP 400 or 404 -> log `tg <method> 400/404: <description>` and `None`
//!    with NO retry.
//! 5. anything else -> treated as an error: sleep `500 * (attempt + 1)` ms
//!    (500/1000/1500/2000, no jitter) and retry; on the last attempt log
//!    `tg <method> failed: <message>` and return `None`.
//!
//! [`Tg::call`] NEVER returns an error and never exits the process. Every
//! failure degrades to `None`, and every call site treats `None` as "the
//! message did not happen" and carries on. That is deliberate: the coordinator
//! is an always-on daemon that announces itself on every start, so a transport
//! error must never take it down.
//!
//! Known reference quirks preserved on purpose:
//!
//! * A 429 on the FINAL attempt falls out of the loop and returns `None` with
//!   no log line at all.
//! * A 429 WITHOUT `parameters.retry_after` does not take the 429 branch — it
//!   falls through to the error ladder and gets the much more aggressive
//!   500/1000/1500/2000 backoff. Arguably a bug; it is what the live service
//!   does.
//! * 401/403/409/500 are NOT terminal. A 409 "terminated by other getUpdates"
//!   (a second poller) retry-loops and logs rather than exiting, which is
//!   exactly how a duplicate poller silently degrades the bridge.
//!
//! Timeouts: the JS sets none, which is what lets `getUpdates` hold a 50s long
//! poll. reqwest defaults to a 30s timeout, so the long-poll client gets its
//! own explicit read timeout strictly greater than 50s.

use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::time::Duration;

/// Attempts per call (`for (let attempt = 0; attempt < 5; attempt++)`).
pub const DEFAULT_ATTEMPTS: u32 = 5;
/// Linear backoff base: attempt N sleeps `base * (N + 1)` ms.
pub const DEFAULT_BACKOFF_BASE_MS: u64 = 500;
/// The hardcoded slack added to Telegram's `retry_after`, in seconds.
pub const RETRY_AFTER_SLACK_SECS: u64 = 1;
/// `getUpdates` long-poll seconds.
pub const DEFAULT_LONG_POLL_SECS: u64 = 50;
/// Client-side read timeout for the long poll. MUST exceed the long-poll
/// seconds or every poll aborts.
pub const DEFAULT_LONG_POLL_READ_TIMEOUT_SECS: u64 = 55;
/// Sleep after a `getUpdates` that returned nothing at all.
pub const DEFAULT_EMPTY_POLL_SLEEP_MS: u64 = 1000;
/// The public Bot API root. Overridable so tests can point at a local mock —
/// a second long-poller against the real token would steal the owner's
/// messages.
pub const DEFAULT_API_ROOT: &str = "https://api.telegram.org";
/// The only reaction emoji the bridge ever sets.
pub const REACTION_EMOJI: &str = "👀";
/// The nonstandard rich-message method tried before the HTML fallback.
pub const RICH_METHOD: &str = "sendRichMessage";

/// Transport tuning. Everything the JS hardcodes lives here so it can be
/// driven by config instead of being retyped at call sites.
#[derive(Debug, Clone)]
pub struct TgConfig {
    pub token: String,
    pub chat_id: i64,
    /// Scheme + host, no trailing slash (`https://api.telegram.org`).
    pub api_root: String,
    pub attempts: u32,
    pub backoff_base_ms: u64,
    pub retry_after_slack_secs: u64,
    pub long_poll_secs: u64,
    pub long_poll_read_timeout_secs: u64,
    pub empty_poll_sleep_ms: u64,
    /// Where `tg ... failed:` lines go. `None` disables file logging.
    pub log_path: Option<PathBuf>,
}

impl TgConfig {
    pub fn new(token: impl Into<String>, chat_id: i64) -> TgConfig {
        TgConfig {
            token: token.into(),
            chat_id,
            api_root: DEFAULT_API_ROOT.to_string(),
            attempts: DEFAULT_ATTEMPTS,
            backoff_base_ms: DEFAULT_BACKOFF_BASE_MS,
            retry_after_slack_secs: RETRY_AFTER_SLACK_SECS,
            long_poll_secs: DEFAULT_LONG_POLL_SECS,
            long_poll_read_timeout_secs: DEFAULT_LONG_POLL_READ_TIMEOUT_SECS,
            empty_poll_sleep_ms: DEFAULT_EMPTY_POLL_SLEEP_MS,
            log_path: None,
        }
    }

    pub fn with_api_root(mut self, root: impl Into<String>) -> TgConfig {
        self.api_root = root.into();
        self
    }

    pub fn with_log_path(mut self, path: Option<PathBuf>) -> TgConfig {
        self.log_path = path;
        self
    }
}

/// The Telegram client bound to one bot + chat.
#[derive(Debug)]
pub struct Tg {
    client: reqwest::blocking::Client,
    /// Separate client whose read timeout survives a 50s long poll.
    poll_client: reqwest::blocking::Client,
    cfg: TgConfig,
}

/// What one HTTP attempt decided.
enum Step {
    /// `data.ok` — hand back `data.result`.
    Done(Value),
    /// Swallow silently (`/not modified/i`).
    Swallow,
    /// Terminal 400/404: log once, no retry.
    Terminal(String),
    /// 429 with retry_after: sleep this long, then consume an attempt.
    RateLimited(Duration),
    /// Anything else, including transport and JSON failures.
    Failed(String),
}

impl Tg {
    /// Build a client for the real Bot API.
    pub fn new(token: String, chat_id: i64) -> Tg {
        Tg::with_config(TgConfig::new(token, chat_id))
    }

    /// Build a client from an explicit [`TgConfig`].
    pub fn with_config(cfg: TgConfig) -> Tg {
        let client = reqwest::blocking::Client::builder()
            .build()
            .expect("blocking http client");
        let poll_client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(cfg.long_poll_read_timeout_secs))
            .build()
            .expect("blocking long-poll http client");
        Tg {
            client,
            poll_client,
            cfg,
        }
    }

    pub fn chat_id(&self) -> i64 {
        self.cfg.chat_id
    }

    pub fn config(&self) -> &TgConfig {
        &self.cfg
    }

    /// `https://api.telegram.org/bot<token>` — never logged.
    fn api_base(&self) -> String {
        format!("{}/bot{}", self.cfg.api_root, self.cfg.token)
    }

    /// `https://api.telegram.org/file/bot<token>` — the second derived base a
    /// reimplementation must not forget.
    fn file_api_base(&self) -> String {
        format!("{}/file/bot{}", self.cfg.api_root, self.cfg.token)
    }

    /// The download URL for a `file_path` returned by `getFile`.
    pub fn file_url(&self, file_path: &str) -> String {
        format!("{}/{file_path}", self.file_api_base())
    }

    fn log(&self, msg: &str) {
        match &self.cfg.log_path {
            Some(p) => crate::log_line(p, msg),
            None => {
                println!("{msg}");
            }
        }
    }

    /// Core call with the retry ladder. `None` = gave up, or an error the
    /// reference deliberately swallows.
    pub fn call(&self, method: &str, body: Value) -> Option<Value> {
        self.call_with(&self.client, method, body, None)
    }

    fn call_with(
        &self,
        client: &reqwest::blocking::Client,
        method: &str,
        body: Value,
        timeout: Option<Duration>,
    ) -> Option<Value> {
        let url = format!("{}/{method}", self.api_base());
        for attempt in 0..self.cfg.attempts {
            let step = self.attempt(client, &url, &body, timeout);
            match step {
                Step::Done(result) => return Some(result),
                Step::Swallow => return None,
                Step::Terminal(description) => {
                    self.log(&format!("tg {method} 400/404: {description}"));
                    return None;
                }
                Step::RateLimited(wait) => {
                    // Consumes an attempt, exactly like the JS `continue`.
                    std::thread::sleep(wait);
                }
                Step::Failed(message) => {
                    if attempt + 1 == self.cfg.attempts {
                        self.log(&format!("tg {method} failed: {message}"));
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(
                        self.cfg.backoff_base_ms * u64::from(attempt + 1),
                    ));
                }
            }
        }
        // A 429 on the final attempt lands here: no log line, no value. The
        // JS returns `undefined` at exactly this point.
        None
    }

    fn attempt(
        &self,
        client: &reqwest::blocking::Client,
        url: &str,
        body: &Value,
        timeout: Option<Duration>,
    ) -> Step {
        let mut req = client
            .post(url)
            .header("content-type", "application/json")
            .body(serde_json::to_string(body).unwrap_or_else(|_| "{}".into()));
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let res = match req.send() {
            Ok(res) => res,
            Err(e) => return Step::Failed(transport_message(&e)),
        };
        let status = res.status().as_u16();
        let data: Value = match res.json() {
            Ok(v) => v,
            Err(e) => return Step::Failed(transport_message(&e)),
        };
        if data.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return Step::Done(data.get("result").cloned().unwrap_or(Value::Null));
        }
        let description = data
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if status == 429 {
            if let Some(secs) = data
                .get("parameters")
                .and_then(|p| p.get("retry_after"))
                .and_then(Value::as_u64)
            {
                return Step::RateLimited(Duration::from_secs(
                    secs + self.cfg.retry_after_slack_secs,
                ));
            }
        }
        if description.to_lowercase().contains("not modified") {
            return Step::Swallow;
        }
        if status == 400 || status == 404 {
            return Step::Terminal(description);
        }
        Step::Failed(if description.is_empty() {
            format!("HTTP {status}")
        } else {
            description
        })
    }

    // ---- outbound message primitives ----

    /// `sendMessage`. `disable_web_page_preview: true` on EVERY outbound
    /// message; `parse_mode` is omitted entirely when `None` (never sent as
    /// null); `extra` is merged LAST and can override any earlier field.
    pub fn send_message(
        &self,
        text: &str,
        parse_mode: Option<&str>,
        extra: Option<&Value>,
    ) -> Option<Value> {
        let mut body = json!({
            "chat_id": self.cfg.chat_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        if let Some(mode) = parse_mode {
            body["parse_mode"] = json!(mode);
        }
        merge_extra(&mut body, extra);
        self.call("sendMessage", body)
    }

    /// `editMessageText`. Same shape and same override precedence as
    /// [`Tg::send_message`].
    pub fn edit_message(
        &self,
        message_id: i64,
        text: &str,
        parse_mode: Option<&str>,
        extra: Option<&Value>,
    ) -> Option<Value> {
        let mut body = json!({
            "chat_id": self.cfg.chat_id,
            "message_id": message_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        if let Some(mode) = parse_mode {
            body["parse_mode"] = json!(mode);
        }
        merge_extra(&mut body, extra);
        self.call("editMessageText", body)
    }

    /// `sendMessage` with no parse mode and no extras.
    pub fn send(&self, text: &str) -> Option<Value> {
        self.send_message(text, None, None)
    }

    /// `editMessageText` with no parse mode.
    pub fn edit(&self, message_id: i64, text: &str, extra: Option<Value>) -> Option<Value> {
        self.edit_message(message_id, text, None, extra.as_ref())
    }

    /// `deleteMessage`. Fire-and-forget in the reference: the JS declares it
    /// async but never awaits the inner `tg()`.
    pub fn delete(&self, message_id: i64) -> Option<Value> {
        self.call(
            "deleteMessage",
            json!({ "chat_id": self.cfg.chat_id, "message_id": message_id }),
        )
    }

    /// `setMessageReaction`. An empty `emoji` clears the reaction — the
    /// coordinator never does that, but the shape is the JS's.
    pub fn react(&self, message_id: i64, emoji: &str) -> Option<Value> {
        let reaction = if emoji.is_empty() {
            json!([])
        } else {
            json!([{ "type": "emoji", "emoji": emoji }])
        };
        self.call(
            "setMessageReaction",
            json!({
                "chat_id": self.cfg.chat_id,
                "message_id": message_id,
                "reaction": reaction,
            }),
        )
    }

    /// `setMessageReaction` with the only emoji the bridge ever uses.
    pub fn react_eyes(&self, message_id: i64) -> Option<Value> {
        self.react(message_id, REACTION_EMOJI)
    }

    /// `sendChatAction` typing.
    pub fn typing(&self) -> Option<Value> {
        self.call(
            "sendChatAction",
            json!({ "chat_id": self.cfg.chat_id, "action": "typing" }),
        )
    }

    /// `answerCallbackQuery`. Never sets `show_alert` or `cache_time`, so
    /// every answer is the short grey toast. `text` is omitted when empty.
    pub fn answer_cb_text(&self, callback_query_id: &str, text: Option<&str>) -> Option<Value> {
        let mut body = json!({ "callback_query_id": callback_query_id });
        if let Some(t) = text.filter(|t| !t.is_empty()) {
            body["text"] = json!(t);
        }
        self.call("answerCallbackQuery", body)
    }

    /// `answerCallbackQuery` with no toast text — dismisses the client-side
    /// spinner and nothing more.
    pub fn answer_cb(&self, callback_query_id: &str) -> Option<Value> {
        self.answer_cb_text(callback_query_id, None)
    }

    /// `setMyCommands`. The payload is GENERATED from the command table (see
    /// [`crate::commands::my_commands_payload`]) — there is no second copy of
    /// the command list in this crate.
    pub fn set_my_commands(&self, commands: Value) -> Option<Value> {
        self.call("setMyCommands", json!({ "commands": commands }))
    }

    /// `getFile`.
    pub fn get_file(&self, file_id: &str) -> Option<Value> {
        self.call("getFile", json!({ "file_id": file_id }))
    }

    /// Start a download of a `file_path` returned by `getFile`.
    ///
    /// One attempt, no retry ladder, no timeout — the reference does a bare
    /// `fetch` here and does NOT route it through `tg()`. Callers that need to
    /// stream to disk (media.rs) take the response; [`Tg::download`] buffers.
    pub fn download_response(
        &self,
        file_path: &str,
    ) -> reqwest::Result<reqwest::blocking::Response> {
        self.client.get(self.file_url(file_path)).send()
    }

    /// Download a file path returned by `getFile` into memory. `None` on a
    /// transport error or a non-2xx response.
    pub fn download(&self, file_path: &str) -> Option<Vec<u8>> {
        let res = self.download_response(file_path).ok()?;
        if !res.status().is_success() {
            return None;
        }
        res.bytes().ok().map(|b| b.to_vec())
    }

    /// The NONSTANDARD rich send.
    ///
    /// First tries `sendRichMessage` with `extra` merged in. If that fails and
    /// `extra` was non-empty, it retries ONCE without `extra` — dropping the
    /// `reply_markup` and trying again. If `extra` was empty there is no
    /// retry.
    ///
    /// `sendRichMessage` is not (yet) a real Bot API method, so against the
    /// standard API this 400s, [`Tg::call`] logs once and returns `None`, and
    /// the caller's chunked-HTML fallback is what actually delivers. That
    /// wasted 400 per final answer is reproduced deliberately: the day the
    /// account gets the method, the rich path starts working with no code
    /// change. Note the consequence of the retry: a rich send can arrive
    /// WITHOUT the control keyboard when the API accepts the message but
    /// rejects the markup.
    pub fn send_rich(&self, markdown: &str, extra: Option<Value>) -> Option<Value> {
        let base = || {
            json!({
                "chat_id": self.cfg.chat_id,
                "rich_message": { "markdown": markdown },
            })
        };
        let extra_keys = extra
            .as_ref()
            .and_then(Value::as_object)
            .map(Map::len)
            .unwrap_or(0);
        let mut body = base();
        merge_extra(&mut body, extra.as_ref());
        if let Some(result) = self.call(RICH_METHOD, body) {
            return Some(result);
        }
        if extra_keys > 0 {
            return self.call(RICH_METHOD, base());
        }
        None
    }

    /// `getUpdates` long-poll. Returns the raw `result` array.
    ///
    /// `edited_message` IS accepted and is treated identically to `message` by
    /// the coordinator: editing a past message re-runs it.
    pub fn get_updates(&self, offset: i64) -> Option<Value> {
        self.call_with(
            &self.poll_client,
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": self.cfg.long_poll_secs,
                "allowed_updates": ["message", "edited_message", "callback_query"],
            }),
            Some(Duration::from_secs(self.cfg.long_poll_read_timeout_secs)),
        )
    }

    /// How long to sleep after a `getUpdates` that produced nothing.
    pub fn empty_poll_sleep(&self) -> Duration {
        Duration::from_millis(self.cfg.empty_poll_sleep_ms)
    }
}

/// Merge `extra` over `body`, last-write-wins — the JS object-spread order.
fn merge_extra(body: &mut Value, extra: Option<&Value>) {
    let (Some(target), Some(source)) = (body.as_object_mut(), extra.and_then(Value::as_object))
    else {
        return;
    };
    for (k, v) in source {
        target.insert(k.clone(), v.clone());
    }
}

/// A short message for a transport/JSON failure, standing in for the JS
/// `e.message`.
fn transport_message(e: &reqwest::Error) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tg() -> Tg {
        Tg::with_config(TgConfig::new("fake-token", 4242))
    }

    #[test]
    fn merge_extra_lets_extra_override_earlier_fields() {
        let mut body = json!({ "text": "a", "disable_web_page_preview": true });
        merge_extra(&mut body, Some(&json!({ "text": "b", "reply_markup": 1 })));
        assert_eq!(body["text"], "b");
        assert_eq!(body["reply_markup"], 1);
        assert_eq!(body["disable_web_page_preview"], true);
    }

    #[test]
    fn merge_extra_ignores_a_non_object() {
        let mut body = json!({ "text": "a" });
        merge_extra(&mut body, Some(&json!("nope")));
        assert_eq!(body, json!({ "text": "a" }));
    }

    #[test]
    fn the_file_api_base_is_derived_separately_from_the_api_base() {
        let tg = tg();
        assert_eq!(
            tg.file_url("photos/file_1.jpg"),
            "https://api.telegram.org/file/botfake-token/photos/file_1.jpg"
        );
        assert_eq!(
            tg.api_base(),
            "https://api.telegram.org/botfake-token"
        );
    }

    /// Otherwise reqwest aborts every single poll. Checked at compile time so
    /// it cannot be broken by editing the constants.
    const _: () = assert!(DEFAULT_LONG_POLL_READ_TIMEOUT_SECS > DEFAULT_LONG_POLL_SECS);

    #[test]
    fn a_configured_read_timeout_must_still_outlast_its_long_poll() {
        let cfg = TgConfig::new("t", 1);
        assert!(cfg.long_poll_read_timeout_secs > cfg.long_poll_secs);
    }

    #[test]
    fn defaults_match_the_hardcoded_reference_values() {
        let cfg = TgConfig::new("t", 1);
        assert_eq!(cfg.attempts, 5);
        assert_eq!(cfg.backoff_base_ms, 500);
        assert_eq!(cfg.retry_after_slack_secs, 1);
        assert_eq!(cfg.long_poll_secs, 50);
        assert_eq!(cfg.empty_poll_sleep_ms, 1000);
    }
}
