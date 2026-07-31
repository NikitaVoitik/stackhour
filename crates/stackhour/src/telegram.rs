//! Telegram Bot API client for the control-plane assistant.

use serde_json::{json, Value};
use std::time::Duration;

const DEFAULT_ATTEMPTS: u32 = 5;
const DEFAULT_BACKOFF_BASE_MS: u64 = 500;
const DEFAULT_RETRY_AFTER_SLACK_SECS: u64 = 1;
const DEFAULT_LONG_POLL_SECS: u64 = 50;
const DEFAULT_LONG_POLL_READ_TIMEOUT_SECS: u64 = 55;
const DEFAULT_EMPTY_POLL_SLEEP_MS: u64 = 1000;
const DEFAULT_API_ROOT: &str = "https://api.telegram.org";
const TELEGRAM_CHUNK_UTF16_UNITS: usize = 4_000;

#[derive(Debug, Clone)]
pub struct TelegramConfig {
    token: String,
    chat_id: i64,
    api_root: String,
    attempts: u32,
    backoff_base_ms: u64,
    retry_after_slack_secs: u64,
    long_poll_secs: u64,
    long_poll_read_timeout_secs: u64,
    empty_poll_sleep_ms: u64,
}

impl TelegramConfig {
    pub fn new(token: impl Into<String>, chat_id: i64) -> Self {
        Self {
            token: token.into(),
            chat_id,
            api_root: DEFAULT_API_ROOT.to_string(),
            attempts: DEFAULT_ATTEMPTS,
            backoff_base_ms: DEFAULT_BACKOFF_BASE_MS,
            retry_after_slack_secs: DEFAULT_RETRY_AFTER_SLACK_SECS,
            long_poll_secs: DEFAULT_LONG_POLL_SECS,
            long_poll_read_timeout_secs: DEFAULT_LONG_POLL_READ_TIMEOUT_SECS,
            empty_poll_sleep_ms: DEFAULT_EMPTY_POLL_SLEEP_MS,
        }
    }

    pub fn with_api_root(mut self, root: impl Into<String>) -> Self {
        self.api_root = root.into().trim_end_matches('/').to_string();
        self
    }
}

#[derive(Debug)]
pub struct Telegram {
    client: reqwest::blocking::Client,
    poll_client: reqwest::blocking::Client,
    config: TelegramConfig,
}

enum Step {
    Done(Value),
    Swallow,
    Terminal(String),
    RateLimited(Duration),
    Failed(String),
}

impl Telegram {
    pub fn with_config(config: TelegramConfig) -> Self {
        let client = reqwest::blocking::Client::builder()
            .build()
            .expect("blocking HTTP client");
        let poll_client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(config.long_poll_read_timeout_secs))
            .build()
            .expect("blocking Telegram long-poll client");
        Self {
            client,
            poll_client,
            config,
        }
    }

    pub fn send(&self, text: &str) -> Option<Value> {
        self.call(
            &self.client,
            "sendMessage",
            json!({
                "chat_id": self.config.chat_id,
                "text": text,
                "disable_web_page_preview": true,
            }),
            None,
        )
    }

    pub fn send_text(&self, text: &str) -> bool {
        telegram_chunks(text, TELEGRAM_CHUNK_UTF16_UNITS)
            .into_iter()
            .all(|chunk| self.send(&chunk).is_some())
    }

    pub fn get_updates(&self, offset: i64) -> Option<Value> {
        self.call(
            &self.poll_client,
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": self.config.long_poll_secs,
                "allowed_updates": ["message", "edited_message"],
            }),
            Some(Duration::from_secs(self.config.long_poll_read_timeout_secs)),
        )
    }

    pub fn empty_poll_sleep(&self) -> Duration {
        Duration::from_millis(self.config.empty_poll_sleep_ms)
    }

    fn call(
        &self,
        client: &reqwest::blocking::Client,
        method: &str,
        body: Value,
        timeout: Option<Duration>,
    ) -> Option<Value> {
        let url = format!("{}/bot{}/{method}", self.config.api_root, self.config.token);
        for attempt in 0..self.config.attempts {
            match self.attempt(client, &url, &body, timeout) {
                Step::Done(result) => return Some(result),
                Step::Swallow => return None,
                Step::Terminal(description) => {
                    eprintln!("telegram {method}: {description}");
                    return None;
                }
                Step::RateLimited(wait) => std::thread::sleep(wait),
                Step::Failed(message) => {
                    if attempt + 1 == self.config.attempts {
                        eprintln!("telegram {method} failed: {message}");
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(
                        self.config.backoff_base_ms * u64::from(attempt + 1),
                    ));
                }
            }
        }
        None
    }

    fn attempt(
        &self,
        client: &reqwest::blocking::Client,
        url: &str,
        body: &Value,
        timeout: Option<Duration>,
    ) -> Step {
        let mut request = client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = match request.send() {
            Ok(response) => response,
            Err(error) => return Step::Failed(safe_request_error(&error)),
        };
        let status = response.status().as_u16();
        let data: Value = match response.json() {
            Ok(value) => value,
            Err(error) => return Step::Failed(safe_request_error(&error)),
        };
        if data
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Step::Done(data.get("result").cloned().unwrap_or(Value::Null));
        }
        let description = data
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if status == 429 {
            if let Some(seconds) = data
                .get("parameters")
                .and_then(|parameters| parameters.get("retry_after"))
                .and_then(serde_json::Value::as_u64)
            {
                return Step::RateLimited(Duration::from_secs(seconds + self.config.retry_after_slack_secs));
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
}

fn safe_request_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "request timed out".to_string()
    } else if error.is_connect() {
        "connection failed".to_string()
    } else if error.is_decode() {
        "response decoding failed".to_string()
    } else {
        "request failed".to_string()
    }
}

fn telegram_chunks(text: &str, max_utf16_units: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut units = 0;
    for character in text.chars() {
        let width = character.len_utf16();
        if !chunk.is_empty() && units + width > max_utf16_units {
            chunks.push(std::mem::take(&mut chunk));
            units = 0;
        }
        chunk.push(character);
        units += width;
    }
    chunks.push(chunk);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_poll_read_timeout_exceeds_the_server_poll() {
        let config = TelegramConfig::new("token", 1);
        assert!(config.long_poll_read_timeout_secs > config.long_poll_secs);
    }

    #[test]
    fn api_root_is_replaceable_for_local_transport_tests() {
        let config = TelegramConfig::new("token", 1).with_api_root("http://127.0.0.1:1/");
        assert_eq!(config.api_root, "http://127.0.0.1:1");
    }

    #[test]
    fn long_unicode_messages_are_split_below_telegrams_limit_without_loss() {
        let original = format!("{}{}", "a".repeat(3_999), "🌙".repeat(100));
        let chunks = telegram_chunks(&original, TELEGRAM_CHUNK_UTF16_UNITS);
        assert!(chunks.len() > 1);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.encode_utf16().count() <= TELEGRAM_CHUNK_UTF16_UNITS));
        assert_eq!(chunks.concat(), original);
    }
}
