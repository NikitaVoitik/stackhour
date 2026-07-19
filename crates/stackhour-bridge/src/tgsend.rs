//! `stackhour bridge tg-send` (plus the installed node shim named
//! tg-send.mjs).
//!
//! --html/--verbose/-v/--from flag parsing; stdin fallback when no message
//! args; '[label] ' prefix; rich-first with 429 recursion; plain 4000-char
//! chunk fallback; parse-entities HTML retry; exit codes 0/1/2 with exact
//! stderr strings; config via $CLAUDE_REMOTE_CONFIG or the adjacent
//! config.json.

/// Run tg-send; returns the process exit code.
pub fn run_tg_send(args: &[String]) -> i32 {
    let _ = args;
    todo!()
}
