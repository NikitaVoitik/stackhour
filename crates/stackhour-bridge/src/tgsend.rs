//! `stackhour bridge tg-send` — the small, separate transport.
//!
//! A byte-for-byte port of `/home/nikita/.claude-remote/tg-send.mjs`. It is
//! deliberately NOT the coordinator's [`crate::telegram::Tg`]: the reference
//! is a second, simpler transport with its own retry rules, its own chunk
//! limit (4000, not deliverFinal's 3800) and its own exit codes, and the
//! owner's `tg-send "..."` habit depends on all three.
//!
//! Behaviour, in order:
//!
//! * Flags: `--html`, `--verbose` / `-v`, `--from <label>`. Everything else is
//!   message text, joined with a space.
//! * Text comes from the arguments, or from stdin when there are none and
//!   stdin is not a TTY. `--from X` prefixes the text with `[X] `.
//! * Default path: try `sendRichMessage`; on 429 with `retry_after`, sleep and
//!   RETRY. On anything else, fall through to the plain path. `--html` skips
//!   rich entirely.
//! * Plain path: chunk at 4000 chars, then up to 4 `sendMessage` attempts per
//!   part. A 429 with `retry_after` sleeps and consumes an attempt. When HTML
//!   is on and the description matches `/can't parse|parse entities/i`, the
//!   HTML flag is turned OFF and the SAME part is retried as plain text.
//! * Exit codes: 2 for config/usage problems, 1 when any part failed, 0 on
//!   success. `--verbose` prints to STDERR, never stdout.
//!
//! Two reference quirks are preserved: the `html` flag is process-global, so
//! once one chunk fails HTML parsing every SUBSEQUENT chunk is sent as plain
//! text (a long message ends up half-formatted); and stdin is only read when
//! it is not a TTY, so under systemd with stdin closed it reads "" and exits 2
//! rather than hanging.
//!
//! One deliberate DIVERGENCE, called out rather than mirrored: the reference's
//! `send()` does not wrap `fetch` in try/catch, so a DNS or TLS failure exits
//! with a Node stack trace instead of the clean `tg-send: Telegram error:`
//! line. This port reports the transport error through the normal error path.
//! Likewise the reference's rich-path 429 handler RECURSES with no depth cap,
//! so a persistently rate-limited chat can blow the stack; this port loops
//! with the same unbounded semantics but no stack growth.

use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::render::{chunk_text, TG_SEND_CHUNK_LIMIT};
use crate::telegram::{DEFAULT_API_ROOT, RETRY_AFTER_SLACK_SECS};

/// Attempts per part in the plain path (`attempt < 4`).
const PLAIN_ATTEMPTS: u32 = 4;

/// Parsed command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TgSendArgs {
    pub html: bool,
    pub verbose: bool,
    pub from: Option<String>,
    /// Non-flag arguments, in order.
    pub rest: Vec<String>,
}

/// Argument parsing, matching the reference's single forward pass. `--from`
/// consumes the next argument; a trailing `--from` with nothing after it
/// yields `None`, exactly as `args[++i]` gives `undefined`.
pub fn parse_args(args: &[String]) -> TgSendArgs {
    let mut out = TgSendArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--html" => out.html = true,
            "--verbose" | "-v" => out.verbose = true,
            "--from" => {
                i += 1;
                out.from = args.get(i).cloned();
            }
            other => out.rest.push(other.to_string()),
        }
        i += 1;
    }
    out
}

/// Everything the runner touches outside of pure argument handling, so the
/// whole flow can be exercised against a local mock Bot API.
pub struct TgSendEnv {
    /// Explicit config path. `None` = resolve from the environment.
    pub config_path: Option<PathBuf>,
    /// Bot API root; overridden in tests.
    pub api_root: String,
    /// stdin contents, when stdin should be read at all.
    pub stdin: Option<String>,
    /// Lines written to stderr.
    pub stderr: Vec<String>,
}

impl TgSendEnv {
    fn err(&mut self, line: impl Into<String>) {
        self.stderr.push(line.into());
    }
}

/// Resolve the config path: `$CLAUDE_REMOTE_CONFIG`, else
/// `$STACKHOUR_BRIDGE_HOME/config.json`, else
/// `~/.claude-remote/config.json` (the reference's location).
pub fn resolve_config_path(env: &impl Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(p) = env("CLAUDE_REMOTE_CONFIG").filter(|v| !v.trim().is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(home) = env("STACKHOUR_BRIDGE_HOME").filter(|v| !v.trim().is_empty()) {
        return PathBuf::from(home).join("config.json");
    }
    let home = env("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude-remote").join("config.json")
}

/// `token` + `chatId` read out of the config file. `chatId` is accepted as a
/// number or a numeric string; the reference compares it with `!==` against a
/// JS number, so a string in the file would silently reject every update — the
/// port parses to i64 instead.
fn read_credentials(path: &Path) -> Result<(String, i64), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let cfg: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let token = cfg
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let chat_id = match cfg.get("chatId") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse::<i64>().ok(),
        _ => None,
    };
    match (token.is_empty(), chat_id) {
        (false, Some(id)) => Ok((token, id)),
        _ => Err(String::new()),
    }
}

/// Run tg-send against an injected environment. Returns the exit code; stderr
/// lines accumulate in `env.stderr`.
pub fn run(args: &TgSendArgs, env: &mut TgSendEnv) -> i32 {
    let cfg_path = match &env.config_path {
        Some(p) => p.clone(),
        None => resolve_config_path(&|k| std::env::var(k).ok()),
    };

    let (token, chat_id) = match read_credentials(&cfg_path) {
        Ok(v) => v,
        Err(message) if message.is_empty() => {
            env.err("tg-send: config missing token or chatId");
            return 2;
        }
        Err(message) => {
            env.err(format!(
                "tg-send: cannot read config at {}: {message}",
                cfg_path.display()
            ));
            return 2;
        }
    };

    let mut text = args.rest.join(" ").trim().to_string();
    if text.is_empty() {
        if let Some(stdin) = &env.stdin {
            text = stdin.trim().to_string();
        }
    }
    if text.is_empty() {
        env.err("tg-send: no message text provided (argument or stdin)");
        return 2;
    }
    if let Some(from) = &args.from {
        text = format!("[{from}] {text}");
    }

    let client = match reqwest::blocking::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            env.err(format!("tg-send: Telegram error: {e}"));
            return 1;
        }
    };
    let api = format!("{}/bot{token}", env.api_root);

    // `html` is mutated by the parse-error fallback and stays mutated for
    // every subsequent chunk — the reference's module-global behaviour.
    let mut html = args.html;

    if !html && send_rich(&client, &api, chat_id, &text) {
        if args.verbose {
            env.err("tg-send: sent (rich)");
        }
        return 0;
    }

    let mut ok = true;
    for part in chunk_text(&text, TG_SEND_CHUNK_LIMIT) {
        // `ok = (await send(part)) && ok` — every part is attempted.
        ok = send_part(&client, &api, chat_id, &part, &mut html, env) && ok;
    }
    if args.verbose && ok {
        env.err("tg-send: sent (plain)");
    }
    if ok {
        0
    } else {
        1
    }
}

/// One JSON POST. `Err` carries a transport-level message.
fn post(
    client: &reqwest::blocking::Client,
    url: &str,
    body: &Value,
) -> Result<(u16, Value), String> {
    let res = client
        .post(url)
        .header("content-type", "application/json")
        .body(serde_json::to_string(body).unwrap_or_else(|_| "{}".into()))
        .send()
        .map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    // The reference does `.catch(() => ({}))`: an unparseable body is an
    // empty object, NOT an error.
    let data = res.json::<Value>().unwrap_or_else(|_| json!({}));
    Ok((status, data))
}

fn retry_after(data: &Value) -> Option<u64> {
    data.get("parameters")
        .and_then(|p| p.get("retry_after"))
        .and_then(Value::as_u64)
}

/// The rich path: `true` on success, `false` to fall through to plain.
/// Unbounded 429 retries, matching the reference's unbounded recursion.
fn send_rich(client: &reqwest::blocking::Client, api: &str, chat_id: i64, markdown: &str) -> bool {
    let body = json!({ "chat_id": chat_id, "rich_message": { "markdown": markdown } });
    loop {
        let Ok((status, data)) = post(client, &format!("{api}/sendRichMessage"), &body) else {
            return false;
        };
        if data.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return true;
        }
        match (status, retry_after(&data)) {
            (429, Some(secs)) => {
                std::thread::sleep(Duration::from_secs(secs + RETRY_AFTER_SLACK_SECS));
            }
            _ => return false,
        }
    }
}

/// The plain path for one chunk: up to 4 attempts.
fn send_part(
    client: &reqwest::blocking::Client,
    api: &str,
    chat_id: i64,
    part: &str,
    html: &mut bool,
    env: &mut TgSendEnv,
) -> bool {
    for _ in 0..PLAIN_ATTEMPTS {
        let mut body = json!({
            "chat_id": chat_id,
            "text": part,
            "disable_web_page_preview": true,
        });
        if *html {
            body["parse_mode"] = json!("HTML");
        }
        let (status, data) = match post(client, &format!("{api}/sendMessage"), &body) {
            Ok(v) => v,
            Err(message) => {
                env.err(format!("tg-send: Telegram error: {message}"));
                return false;
            }
        };
        if data.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return true;
        }
        let description = data
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if status == 429 {
            if let Some(secs) = retry_after(&data) {
                std::thread::sleep(Duration::from_secs(secs + RETRY_AFTER_SLACK_SECS));
                continue;
            }
        }
        if *html && is_parse_error(&description) {
            *html = false;
            continue;
        }
        env.err(format!(
            "tg-send: Telegram error: {}",
            if description.is_empty() {
                status.to_string()
            } else {
                description
            }
        ));
        return false;
    }
    false
}

/// `/can't parse|parse entities/i`
fn is_parse_error(description: &str) -> bool {
    let lower = description.to_lowercase();
    lower.contains("can't parse") || lower.contains("parse entities")
}

/// Run tg-send against the real process environment; returns the exit code.
pub fn run_tg_send(args: &[String]) -> i32 {
    let parsed = parse_args(args);
    let stdin = if parsed.rest.is_empty() && !stdin_is_tty() {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).ok();
        Some(buf)
    } else {
        None
    };
    let mut env = TgSendEnv {
        config_path: None,
        api_root: DEFAULT_API_ROOT.to_string(),
        stdin,
        stderr: Vec::new(),
    };
    let code = run(&parsed, &mut env);
    for line in &env.stderr {
        eprintln!("{line}");
    }
    code
}

#[cfg(unix)]
fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

#[cfg(not(unix))]
fn stdin_is_tty() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_collects_flags_and_message_words() {
        let p = parse_args(&args(&["--from", "Mac", "hello", "--html", "world", "-v"]));
        assert!(p.html);
        assert!(p.verbose);
        assert_eq!(p.from.as_deref(), Some("Mac"));
        assert_eq!(p.rest, vec!["hello", "world"]);
    }

    #[test]
    fn a_trailing_from_with_no_value_yields_none() {
        let p = parse_args(&args(&["hi", "--from"]));
        assert_eq!(p.from, None);
        assert_eq!(p.rest, vec!["hi"]);
    }

    #[test]
    fn is_parse_error_matches_both_telegram_phrasings_case_insensitively() {
        assert!(is_parse_error("Bad Request: can't parse entities"));
        assert!(is_parse_error("CAN'T PARSE"));
        assert!(is_parse_error("Can not parse entities in message"));
        assert!(!is_parse_error("chat not found"));
    }

    #[test]
    fn resolve_config_path_prefers_the_explicit_env_override() {
        let env = |k: &str| match k {
            "CLAUDE_REMOTE_CONFIG" => Some("/tmp/a.json".to_string()),
            "HOME" => Some("/home/x".to_string()),
            _ => None,
        };
        assert_eq!(resolve_config_path(&env), PathBuf::from("/tmp/a.json"));
    }

    #[test]
    fn resolve_config_path_falls_back_through_bridge_home_to_claude_remote() {
        let bridge = |k: &str| match k {
            "STACKHOUR_BRIDGE_HOME" => Some("/srv/bridge".to_string()),
            "HOME" => Some("/home/x".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_config_path(&bridge),
            PathBuf::from("/srv/bridge/config.json")
        );
        let bare = |k: &str| (k == "HOME").then(|| "/home/x".to_string());
        assert_eq!(
            resolve_config_path(&bare),
            PathBuf::from("/home/x/.claude-remote/config.json")
        );
    }

    #[test]
    fn an_empty_env_var_is_treated_as_unset() {
        let env = |k: &str| match k {
            "CLAUDE_REMOTE_CONFIG" => Some("  ".to_string()),
            "HOME" => Some("/home/x".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_config_path(&env),
            PathBuf::from("/home/x/.claude-remote/config.json")
        );
    }
}
