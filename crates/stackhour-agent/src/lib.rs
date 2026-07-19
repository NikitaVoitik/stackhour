//! stackhour-agent — the fully synchronous local watcher loop.
//!
//! The exact 9-step tick: load state -> watchers in FIXED order (files,
//! claude, codex, macApps, ssh, zed) -> `{machine, …row}` stamping (machine
//! key first) -> queue append BEFORE state save (crash-safety ordering,
//! sacred) -> ≤4MiB batch send oldest-first -> queue rewrite/delete on
//! success only -> re-stat queue -> health report POST skipped when the
//! server already failed this tick. Chained sleep-after-tick drifting loop
//! with caught+logged tick errors; a first-tick error is fatal and releases
//! the lock; `--once` runs a single tick.

use serde_json::Value;
use stackhour_core::config::Config;
use stackhour_core::Result;

pub mod http;
pub mod lock;
pub mod queue;
pub mod state;
pub mod tail;
pub mod watch_claude;
pub mod watch_codex;
pub mod watch_files;
pub mod watch_mac;
pub mod watch_ssh;
pub mod watch_zed;

/// Why a watcher did (not) run this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Configured + available: run it.
    Run,
    /// Skipped: recorded in the health report with the exact reason string.
    Skipped {
        /// `agent.watch.<name>` toggle value.
        enabled: bool,
        /// Platform / input availability.
        available: bool,
        /// Exact reason string (e.g. `requires macOS`).
        reason: String,
    },
}

/// One watcher. Health bookkeeping (lastOk / lastDurationMs / lastCount /
/// lastInput / lastEvent / unmatchedInputRuns / consecutiveErrors /
/// error:null-on-success, input-marker diffing, 500-char error truncation)
/// is applied by the tick loop around these calls.
pub trait Watcher {
    /// Watcher name as it appears in config, health report and doctor
    /// (`files`, `claude`, `codex`, `macApps`, `ssh`, `zed`).
    fn name(&self) -> &str;

    /// Config + platform gating with exact reason strings.
    fn gate(&self, cfg: &Config) -> Gate;

    /// Cheap marker describing this tick's input (for unmatchedInputRuns
    /// diffing); None = watcher has no marker concept.
    fn input_marker(&self, state: &Value) -> Option<String>;

    /// Produce heartbeat rows (WITHOUT the machine stamp) and update state.
    fn run(&mut self, cfg: &Config, state: &mut Value, now: f64) -> Result<Vec<Value>>;
}

/// Run the agent loop (or a single tick with `once`). Returns the final
/// health report Value (used by `--once` and tests).
pub fn run_agent(cfg: &Config, once: bool) -> Result<Value> {
    let _ = (cfg, once);
    todo!()
}
