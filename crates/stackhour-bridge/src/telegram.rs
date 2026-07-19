//! Blocking Telegram Bot API client.
//!
//! `call` implements the exact retry ladder: 5 attempts; 429 -> sleep
//! retry_after+1; /not modified/i -> Ok(None); 400/404 -> log + None with NO
//! retry; otherwise 500*(n+1)ms backoff. getUpdates long-polls with
//! timeout=50 and a ~55s read timeout.

use serde_json::Value;

/// The Telegram client bound to one bot + chat.
#[derive(Debug)]
pub struct Tg {
    #[allow(dead_code)] // scaffold: read only by the todo!() bodies
    client: reqwest::blocking::Client,
    #[allow(dead_code)] // scaffold: read only by the todo!() bodies
    token: String,
    chat_id: i64,
}

impl Tg {
    /// Build a client (separate long-poll timeout handled per call).
    pub fn new(token: String, chat_id: i64) -> Tg {
        let _ = (&token, chat_id);
        todo!()
    }

    pub fn chat_id(&self) -> i64 {
        self.chat_id
    }

    /// Core call with the retry ladder. None = gave up / swallowed error.
    pub fn call(&self, method: &str, body: Value) -> Option<Value> {
        let _ = (method, body);
        todo!()
    }

    /// sendMessage (plain). Returns the message Value.
    pub fn send(&self, text: &str) -> Option<Value> {
        let _ = text;
        todo!()
    }

    /// editMessageText.
    pub fn edit(&self, message_id: i64, text: &str, extra: Option<Value>) -> Option<Value> {
        let _ = (message_id, text, extra);
        todo!()
    }

    /// deleteMessage.
    pub fn delete(&self, message_id: i64) -> Option<Value> {
        let _ = message_id;
        todo!()
    }

    /// setMessageReaction.
    pub fn react(&self, message_id: i64, emoji: &str) -> Option<Value> {
        let _ = (message_id, emoji);
        todo!()
    }

    /// sendChatAction typing.
    pub fn typing(&self) -> Option<Value> {
        todo!()
    }

    /// answerCallbackQuery.
    pub fn answer_cb(&self, callback_query_id: &str) -> Option<Value> {
        let _ = callback_query_id;
        todo!()
    }

    /// setMyCommands.
    pub fn set_my_commands(&self, commands: Value) -> Option<Value> {
        let _ = commands;
        todo!()
    }

    /// getFile.
    pub fn get_file(&self, file_id: &str) -> Option<Value> {
        let _ = file_id;
        todo!()
    }

    /// Download a file path returned by getFile.
    pub fn download(&self, file_path: &str) -> Option<Vec<u8>> {
        let _ = file_path;
        todo!()
    }

    /// The NONSTANDARD rich send: HTML render with a retry-without-extras
    /// fallback.
    pub fn send_rich(&self, markdown: &str, extra: Option<Value>) -> Option<Value> {
        let _ = (markdown, extra);
        todo!()
    }

    /// getUpdates long-poll (timeout 50, read timeout ~55s).
    pub fn get_updates(&self, offset: i64) -> Option<Value> {
        let _ = offset;
        todo!()
    }
}
