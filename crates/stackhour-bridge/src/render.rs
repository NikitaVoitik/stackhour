//! Output shaping for Telegram, split from telegram.rs.
//!
//! esc(): 5-entity HTML escape. Fenced-code -> <pre> with the language line
//! stripped; inline code -> <code>; markdown-table detection + padded-code-
//! block rewrite; newline-preferring chunker (3800 chars for deliverFinal /
//! 4000 for tg-send, 50% hard-cut floor); fmt_dur ('1m30s' style); 80-char
//! activity-line builders for claude tool_use and codex item labels.

use serde_json::Value;

/// HTML-escape the 5 entities.
pub fn esc(s: &str) -> String {
    let _ = s;
    todo!()
}

/// Markdown -> Telegram-HTML chunks of at most `limit` chars, splitting at
/// newlines when possible (50% hard-cut floor).
pub fn html_chunks(text: &str, limit: usize) -> Vec<String> {
    let _ = (text, limit);
    todo!()
}

/// Rewrite detected markdown tables into padded code blocks.
pub fn rewrite_tables(text: &str) -> String {
    let _ = text;
    todo!()
}

/// '1m30s'-style duration formatting.
pub fn fmt_dur(ms: i64) -> String {
    let _ = ms;
    todo!()
}

/// 80-char activity line for a claude stream tool_use event, when renderable.
pub fn claude_activity(ev: &Value) -> Option<String> {
    let _ = ev;
    todo!()
}

/// 80-char activity line for a codex JSONL item, when renderable.
pub fn codex_activity(ev: &Value) -> Option<String> {
    let _ = ev;
    todo!()
}
