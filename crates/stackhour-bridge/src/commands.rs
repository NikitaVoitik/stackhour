//! Telegram command handling.
//!
//! handle_text: full lowercased-command match for built-ins (/start /help
//! /menu /claude /codex /mac /local /gcp /remote /where /status /new /reset
//! /stop; unknown-slash -> help) with exact reply strings; status_text +
//! HELP via the PromptStore; control/stop inline keyboards with the '✅ '
//! active prefix; callback-query dispatch including the keyboard re-edit on
//! the emoji-regex match; THEN registry CommandDefs (prompt -> render +
//! route-as-text, agent/engine/target -> state switch, shell -> fixed argv
//! with args appended as ONE trailing element, confirm keyboards); /agent
//! <name> + /agents from souls.rs; the setMyCommands payload = built-ins +
//! user commands. /stop semantics: SIGTERM the current local child, delete
//! unclaimed mac jobs + edit their status messages, report claimed-running.

use crate::coordinator::Coordinator;
use crate::state::BridgeState;
use serde_json::Value;
use stackhour_core::registry::Registry;

/// Handle one text update (commands, then registry commands, then prompt
/// routing).
pub fn handle_text(ctx: &mut Coordinator, text: &str, msg_id: Option<i64>) {
    let _ = (ctx, text, msg_id);
    todo!()
}

/// Handle one callback query (inline keyboards).
pub fn handle_callback(ctx: &mut Coordinator, cb: &Value) {
    let _ = (ctx, cb);
    todo!()
}

/// The control inline keyboard ('✅ ' prefix on the active entries).
pub fn control_keyboard(state: &BridgeState, reg: &Registry) -> Value {
    let _ = (state, reg);
    todo!()
}

/// The /status reply text.
pub fn status_text(state: &BridgeState, worker_alive: bool, busy: bool) -> String {
    let _ = (state, worker_alive, busy);
    todo!()
}

/// The setMyCommands payload: built-ins + registry user commands.
pub fn my_commands_payload(reg: &Registry) -> Value {
    let _ = reg;
    todo!()
}
