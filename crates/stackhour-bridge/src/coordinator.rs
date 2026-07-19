//! The coordinator daemon (blocking; threads for timers).
//!
//! Startup: loose validation + dir creation + state load/migration + online
//! banner + setMyCommands + 24h prune-timer thread + 1s results-poller
//! thread. Main getUpdates long-poll loop with per-update offset persistence
//! and chat-id/bot gate; update routing priority voice -> media -> text.
//! The gcp local lane: in-memory FIFO with the engine captured at enqueue,
//! busy single-flight, status message lifecycle (plain first send, then
//! 800ms-throttled exact-text-deduped HTML edits + typing), deliverFinal
//! rich -> delete-status -> chunked-HTML-fallback ordering with the
//! '— Engine · Label · dur' footer. Mac dispatch: job write, offline-queued
//! wording when the worker heartbeat is stale. Results consumption:
//! unlink-then-parse-skip, session persist, 'done' duration fallback on an
//! unknown pending id. `registry.reload_if_changed()` runs before each
//! update dispatch. Every loop body is wrapped and logged — the daemon NEVER
//! exits on error (it announces itself on restart and would spam Telegram).

use crate::config::CoordinatorCfg;
use crate::engines::RunningJob;
use crate::state::BridgeState;
use crate::telegram::Tg;
use crate::BridgePaths;
use stackhour_core::registry::Registry;
use std::collections::{HashMap, VecDeque};
use std::path::Path;

/// A prompt queued on the local (gcp) lane; the engine (and agent) are
/// captured at enqueue time.
#[derive(Debug, Clone)]
pub struct QueuedPrompt {
    pub prompt: String,
    pub engine: String,
    pub agent: Option<String>,
    /// The user's Telegram message id (for reactions/replies).
    pub message_id: Option<i64>,
    /// Enqueue timestamp (ms) for the duration footer.
    pub enqueued_ms: i64,
}

/// A job dispatched to the mac worker, awaiting its result file.
#[derive(Debug, Clone)]
pub struct PendingJob {
    pub job_id: String,
    pub engine: String,
    pub agent: Option<String>,
    /// The status message being edited while the job runs.
    pub status_message_id: Option<i64>,
    /// Dispatch timestamp (ms) for the duration footer.
    pub dispatched_ms: i64,
}

/// All coordinator runtime state (crate-visible so commands.rs can drive it).
#[allow(dead_code)] // scaffold: fields read only by the todo!() bodies
pub struct Coordinator {
    pub(crate) cfg: CoordinatorCfg,
    pub(crate) paths: BridgePaths,
    pub(crate) state: BridgeState,
    pub(crate) tg: Tg,
    pub(crate) reg: Registry,
    pub(crate) local_q: VecDeque<QueuedPrompt>,
    pub(crate) pending: HashMap<String, PendingJob>,
    pub(crate) current: Option<RunningJob>,
    pub(crate) busy: bool,
}

/// Run the coordinator daemon forever.
pub fn run_coordinator(runtime_dir: &Path) -> ! {
    let _ = runtime_dir;
    todo!()
}
