//! Loopback-only Telegram Bot API simulator and tiny chat UI.

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use stackhour_core::{Error, Result};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

const DEFAULT_BIND: &str = "127.0.0.1:4060";
const DEFAULT_CHAT_ID: i64 = 1;
const DEFAULT_BOT_TOKEN: &str = "fake-token";
const MAX_HISTORY: usize = 1_000;

#[derive(Clone)]
struct FakeTelegram {
    chat_id: i64,
    bot_token: String,
    inner: Arc<Mutex<FakeState>>,
    updates_changed: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct FakeState {
    next_update_id: i64,
    next_message_id: i64,
    next_transcript_id: i64,
    updates: Vec<Value>,
    outgoing: Vec<Value>,
    transcript: Vec<Value>,
}

#[derive(Deserialize)]
struct UserMessage {
    text: String,
}

#[derive(Deserialize, Default)]
struct StateQuery {
    after: Option<i64>,
}

pub fn run(args: &[String]) -> Result<()> {
    let mut bind = DEFAULT_BIND.to_string();
    let mut chat_id = DEFAULT_CHAT_ID;
    let mut bot_token = DEFAULT_BOT_TOKEN.to_string();
    for arg in args {
        if let Some(value) = arg.strip_prefix("--bind=") {
            bind = value.to_string();
        } else if let Some(value) = arg.strip_prefix("--chat-id=") {
            chat_id = value
                .parse()
                .map_err(|error| Error::msg(format!("bad --chat-id: {error}")))?;
        } else if let Some(value) = arg.strip_prefix("--bot-token=") {
            if value.trim().is_empty() {
                return Err(Error::msg("--bot-token cannot be empty"));
            }
            bot_token = value.to_string();
        } else {
            return Err(Error::msg(
                "usage: stackhour control fake-telegram \
                 [--bind=127.0.0.1:4060] [--chat-id=1] [--bot-token=fake-token]",
            ));
        }
    }
    let address: SocketAddr = bind
        .parse()
        .map_err(|error| Error::msg(format!("bad --bind: {error}")))?;
    if !address.ip().is_loopback() {
        return Err(Error::msg("fake Telegram must bind to a loopback address"));
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| Error::msg(error.to_string()))?
        .block_on(async move {
            let listener = tokio::net::TcpListener::bind(address).await?;
            let bound = listener.local_addr()?;
            println!(
                "stackhour fake Telegram: http://{bound} \
                 (chat {chat_id}, token {bot_token})"
            );
            axum::serve(listener, router(chat_id, bot_token))
                .await
                .map_err(Error::from)
        })
}

fn router(chat_id: i64, bot_token: String) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/fake/send", post(fake_send))
        .route("/fake/state", get(fake_state))
        .route("/{bot}/{method}", post(bot_method))
        .with_state(FakeTelegram {
            chat_id,
            bot_token,
            inner: Arc::new(Mutex::new(FakeState {
                next_update_id: 1,
                next_message_id: 1,
                next_transcript_id: 1,
                ..FakeState::default()
            })),
            updates_changed: Arc::new(tokio::sync::Notify::new()),
        })
}

async fn index(headers: HeaderMap) -> Response {
    if !local_request(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    axum::response::Html(UI).into_response()
}

async fn fake_send(
    headers: HeaderMap,
    State(state): State<FakeTelegram>,
    Json(message): Json<UserMessage>,
) -> Response {
    if !local_request(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let text = message.text.trim();
    if text.is_empty() || text.encode_utf16().count() > 4_096 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "description": "text must contain 1..4096 UTF-16 units"})),
        )
            .into_response();
    }
    let mut inner = state.inner.lock().unwrap_or_else(|poison| poison.into_inner());
    let update_id = inner.next_update_id;
    inner.next_update_id += 1;
    let message_id = inner.next_message_id;
    inner.next_message_id += 1;
    inner.updates.push(json!({
        "update_id": update_id,
        "message": {
            "message_id": message_id,
            "date": unix_seconds(),
            "chat": {"id": state.chat_id, "type": "private"},
            "from": {"id": state.chat_id, "is_bot": false, "first_name": "Local user"},
            "text": text
        }
    }));
    push_transcript(&mut inner, "user", text);
    trim_history(&mut inner.updates);
    drop(inner);
    state.updates_changed.notify_waiters();
    Json(json!({"ok": true, "update_id": update_id})).into_response()
}

async fn fake_state(
    headers: HeaderMap,
    Query(query): Query<StateQuery>,
    State(state): State<FakeTelegram>,
) -> Response {
    if !local_request(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let inner = state.inner.lock().unwrap_or_else(|poison| poison.into_inner());
    let after = query.after.unwrap_or(0);
    let transcript = inner
        .transcript
        .iter()
        .filter(|message| message.get("id").and_then(Value::as_i64).unwrap_or(0) > after)
        .cloned()
        .collect::<Vec<_>>();
    drop(inner);
    let next_cursor = transcript
        .last()
        .and_then(|message| message.get("id"))
        .and_then(Value::as_i64)
        .unwrap_or(after);
    Json(json!({
        "chat_id": state.chat_id,
        "transcript": transcript,
        "next_cursor": next_cursor
    }))
    .into_response()
}

async fn bot_method(
    headers: HeaderMap,
    Path((bot, method)): Path<(String, String)>,
    State(state): State<FakeTelegram>,
    Json(body): Json<Value>,
) -> Response {
    if !local_request(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if bot != format!("bot{}", state.bot_token) {
        return telegram_error(StatusCode::NOT_FOUND, "unknown Bot API path");
    }
    match method.as_str() {
        "getUpdates" => {
            let offset = body.get("offset").and_then(Value::as_i64).unwrap_or(0);
            let timeout = body.get("timeout").and_then(Value::as_u64).unwrap_or(0).min(50);
            let mut notified = std::pin::pin!(state.updates_changed.notified());
            notified.as_mut().enable();
            let mut updates = confirmed_updates(&state, offset);
            if updates.is_empty() && timeout > 0 {
                let _ = tokio::time::timeout(Duration::from_secs(timeout), notified).await;
                updates = confirmed_updates(&state, offset);
            }
            Json(json!({"ok": true, "result": updates})).into_response()
        }
        "sendMessage" => {
            if body.get("chat_id").and_then(Value::as_i64) != Some(state.chat_id) {
                return telegram_error(StatusCode::BAD_REQUEST, "chat not found");
            }
            let Some(text) = body.get("text").and_then(Value::as_str) else {
                return telegram_error(StatusCode::BAD_REQUEST, "text is required");
            };
            if text.encode_utf16().count() > 4_096 {
                return telegram_error(StatusCode::BAD_REQUEST, "message is too long");
            }
            let mut inner = state.inner.lock().unwrap_or_else(|poison| poison.into_inner());
            let message_id = inner.next_message_id;
            inner.next_message_id += 1;
            let message = json!({
                "message_id": message_id,
                "date": unix_seconds(),
                "chat": {"id": state.chat_id, "type": "private"},
                "from": {"id": 0, "is_bot": true, "first_name": "Claire"},
                "text": text
            });
            inner.outgoing.push(message.clone());
            push_transcript(&mut inner, "bot", text);
            trim_history(&mut inner.outgoing);
            drop(inner);
            Json(json!({"ok": true, "result": message})).into_response()
        }
        _ => telegram_error(StatusCode::NOT_FOUND, "unknown Bot API method"),
    }
}

fn confirmed_updates(state: &FakeTelegram, offset: i64) -> Vec<Value> {
    let mut inner = state.inner.lock().unwrap_or_else(|poison| poison.into_inner());
    if offset > 0 {
        inner
            .updates
            .retain(|update| update.get("update_id").and_then(Value::as_i64).unwrap_or(0) >= offset);
    }
    inner.updates.clone()
}

fn push_transcript(state: &mut FakeState, direction: &str, text: &str) {
    let id = state.next_transcript_id;
    state.next_transcript_id += 1;
    state
        .transcript
        .push(json!({"id": id, "direction": direction, "text": text}));
    trim_history(&mut state.transcript);
}

fn trim_history(values: &mut Vec<Value>) {
    if values.len() > MAX_HISTORY {
        values.drain(..values.len() - MAX_HISTORY);
    }
}

fn local_request(headers: &HeaderMap) -> bool {
    let host_is_loopback = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| reqwest::Url::parse(&format!("http://{host}")).ok())
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| loopback_host(&host));
    if !host_is_loopback {
        return false;
    }
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|origin| {
            reqwest::Url::parse(origin)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
                .is_some_and(|host| loopback_host(&host))
        })
}

fn loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn telegram_error(status: StatusCode, description: &str) -> Response {
    (status, Json(json!({"ok": false, "description": description}))).into_response()
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

const UI: &str = r#"<!doctype html>
<html lang="en">
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Stackhour · Fake Telegram</title>
<style>
:root{color-scheme:dark;font:15px/1.45 ui-sans-serif,system-ui;background:#0e1621;color:#e8f1f8}
*{box-sizing:border-box}body{margin:0;display:grid;place-items:center;min-height:100vh}
main{width:min(760px,100%);height:min(900px,100vh);display:grid;grid-template-rows:auto 1fr auto;background:#17212b}
header{padding:16px 20px;background:#202b36;border-bottom:1px solid #2b3947}
h1{font-size:17px;margin:0}.hint{color:#91a3b5;font-size:12px}
#chat{overflow:auto;padding:20px;display:flex;flex-direction:column;gap:10px}
.bubble{max-width:82%;padding:9px 12px;border-radius:13px;white-space:pre-wrap;overflow-wrap:anywhere}
.user{align-self:flex-end;background:#2b5278}.bot{align-self:flex-start;background:#182f3d}
form{display:flex;gap:8px;padding:12px;background:#202b36}textarea{flex:1;resize:none;border:0;border-radius:10px;padding:10px;background:#0e1621;color:inherit}
button{border:0;border-radius:10px;padding:0 18px;background:#4b9ed8;color:white;font-weight:700;cursor:pointer}
</style>
<main><header><h1>Claire · local Telegram</h1><div class="hint">Loopback simulator — real Bot API traffic</div></header>
<section id="chat"></section>
<form id="form"><textarea id="text" rows="2" autofocus placeholder="Message Claire"></textarea><button>Send</button></form></main>
<script>
const chat=document.querySelector('#chat'),text=document.querySelector('#text');let fingerprint='';
function bubble(kind,value){const el=document.createElement('div');el.className='bubble '+kind;el.textContent=value;chat.append(el)}
async function refresh(){try{const s=await fetch('/fake/state').then(r=>r.json());const next=JSON.stringify(s);if(next===fingerprint)return;fingerprint=next;chat.replaceChildren();for(const m of s.transcript)bubble(m.direction,m.text||'');chat.scrollTop=chat.scrollHeight}catch(_){}}
document.querySelector('#form').addEventListener('submit',async e=>{e.preventDefault();const value=text.value.trim();if(!value)return;await fetch('/fake/send',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({text:value})});text.value='';refresh()});
setInterval(refresh,500);refresh();
</script></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_telegram_client_and_fake_ui_share_one_bounded_transcript() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router(42, "fake-token".to_string()))
                .await
                .unwrap();
        });
        let client = reqwest::Client::new();
        let root = format!("http://{address}");
        let injected: Value = client
            .post(format!("{root}/fake/send"))
            .json(&json!({"text": "hello"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(injected["ok"], true);

        let telegram_root = root.clone();
        tokio::task::spawn_blocking(move || {
            let telegram = crate::telegram::Telegram::with_config(
                crate::telegram::TelegramConfig::new("fake-token", 42).with_api_root(telegram_root),
            );
            let updates = telegram.get_updates(0).unwrap();
            assert_eq!(updates[0]["message"]["text"], "hello");
            assert!(telegram.send_text("hi back"));
        })
        .await
        .unwrap();

        let transcript: Value = client
            .get(format!("{root}/fake/state"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(transcript["transcript"][1]["text"], "hi back");

        let forbidden = client
            .post(format!("{root}/fake/send"))
            .header("origin", "https://attacker.example")
            .json(&json!({"text": "run something"}))
            .send()
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        task.abort();
    }

    #[test]
    fn non_loopback_bind_is_rejected_before_serving() {
        let error = run(&["--bind=0.0.0.0:4060".to_string()]).unwrap_err();
        assert!(error.message().contains("loopback"));
    }

    #[test]
    fn local_gate_accepts_named_and_ipv6_loopback_hosts() {
        for host in ["localhost:4060", "[::1]:4060", "127.0.0.1:4060"] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            assert!(local_request(&headers), "{host}");
        }
    }
}
