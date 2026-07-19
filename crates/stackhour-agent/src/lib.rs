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

use serde_json::{json, Value};
use stackhour_core::config::Config;
use stackhour_core::Result;
use std::path::Path;
use std::time::Duration;

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

/// The watcher set, in the FIXED order the tick runs them.
///
/// Only `files` is ported so far; the other five remain scaffolds and are
/// deliberately absent rather than silently reporting healthy.
fn default_watchers() -> Vec<Box<dyn Watcher>> {
    vec![Box::new(watch_files::FilesWatcher)]
}

/// Node's `setTimeout` ceiling, `TIMEOUT_MAX` = 2^31-1 milliseconds (~24.8
/// days). Anything above it is not a real interval, it is a typo.
const TIMEOUT_MAX_SECONDS: f64 = 2_147_483_647.0 / 1000.0;

/// Turn `agent.intervalSeconds` into a sleep duration that can never panic.
///
/// `intervalSeconds` reaches us straight out of a JS-semantics `Number()`
/// coercion with no range validation, so it can be negative, NaN, infinite or
/// absurdly large. `Duration::from_secs_f64` panics on NaN and on anything
/// past ~1.8e19 seconds, which used to abort the agent with exit 101 AFTER a
/// successful first tick — invisible to `--once` and a restart-loop under
/// systemd.
///
/// Floor at 1s (as before) and ceil at Node's own timer maximum. Node
/// technically wraps an over-large `setTimeout` delay down to 1ms, which
/// would busy-loop; clamping up to the ceiling is the same "keep running"
/// outcome without burning a core.
fn tick_interval(interval_seconds: f64) -> Duration {
    // NaN survives neither comparison, so name it explicitly.
    if interval_seconds.is_nan() {
        return Duration::from_secs(1);
    }
    Duration::from_secs_f64(interval_seconds.clamp(1.0, TIMEOUT_MAX_SECONDS))
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `Math.round(ms * 10) / 10` — one decimal place.
fn round1(ms: f64) -> f64 {
    stackhour_core::jsnum::js_round_f64(ms * 10.0) / 10.0
}

/// Ensure `state.watcherHealth[name]` exists and hand back the live map.
fn health_mut<'a>(state: &'a mut Value, name: &str) -> &'a mut serde_json::Map<String, Value> {
    let root = state.as_object_mut().expect("state is an object");
    if !root.get("watcherHealth").is_some_and(Value::is_object) {
        root.insert("watcherHealth".into(), json!({}));
    }
    let all = root
        .get_mut("watcherHealth")
        .and_then(Value::as_object_mut)
        .expect("just inserted");
    if !all.get(name).is_some_and(Value::is_object) {
        all.insert(name.to_string(), json!({}));
    }
    all.get_mut(name)
        .and_then(Value::as_object_mut)
        .expect("just inserted")
}

/// Run every watcher once, updating health bookkeeping, and return the fresh
/// (unstamped) heartbeat rows.
fn run_watchers(cfg: &Config, state: &mut Value, watchers: &mut [Box<dyn Watcher>]) -> Vec<Value> {
    let mut fresh = Vec::new();
    for watcher in watchers.iter_mut() {
        let name = watcher.name().to_string();
        match watcher.gate(cfg) {
            Gate::Skipped {
                enabled,
                available,
                reason,
            } => {
                let health = health_mut(state, &name);
                health.insert("enabled".into(), json!(enabled));
                health.insert("available".into(), json!(available));
                health.insert("lastCount".into(), json!(0));
                health.insert("reason".into(), json!(reason));
                continue;
            }
            Gate::Run => {}
        }
        {
            let health = health_mut(state, &name);
            health.insert("enabled".into(), json!(true));
            health.insert("available".into(), json!(true));
            health.shift_remove("reason");
        }

        let before = watcher.input_marker(state);
        let started = std::time::Instant::now();
        let now = unix_now();
        let result = watcher.run(cfg, state, now);
        let duration_ms = round1(started.elapsed().as_secs_f64() * 1000.0);
        let finished = unix_now();

        match result {
            Ok(rows) => {
                let after = watcher.input_marker(state);
                let input_changed = before != after || !rows.is_empty();
                let count = rows.len();
                let max_time = rows
                    .iter()
                    .map(|r| r.get("time").and_then(Value::as_f64).unwrap_or(finished))
                    .fold(f64::NEG_INFINITY, f64::max);

                let health = health_mut(state, &name);
                health.insert("lastOk".into(), json!(finished));
                health.insert("lastDurationMs".into(), json!(duration_ms));
                health.insert("lastCount".into(), json!(count));
                health.insert("consecutiveErrors".into(), json!(0));
                health.insert("error".into(), Value::Null);
                if input_changed {
                    health.insert("lastInput".into(), json!(finished));
                }
                if count > 0 {
                    health.insert("lastEvent".into(), json!(max_time));
                    // A productive run clears the "input changed but nothing
                    // came out" streak doctor warns about.
                    health.insert("unmatchedInputRuns".into(), json!(0));
                } else if input_changed {
                    let prev = health
                        .get("unmatchedInputRuns")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    health.insert("unmatchedInputRuns".into(), json!(prev + 1.0));
                }
                fresh.extend(rows);
            }
            Err(err) => {
                // A failing watcher must not take the tick down with it: the
                // other watchers' rows still need to reach the server.
                let message: String = err.message().chars().take(500).collect();
                let health = health_mut(state, &name);
                health.insert("lastDurationMs".into(), json!(duration_ms));
                health.insert("lastError".into(), json!(finished));
                let prev = health
                    .get("consecutiveErrors")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                health.insert("consecutiveErrors".into(), json!(prev + 1.0));
                health.insert("error".into(), json!(message));
                eprintln!("[stackhour] watcher {name} failed: {}", err.message());
            }
        }
    }
    fresh
}

/// One full tick. Returns the health report that was (or would have been)
/// posted, so `--once` and tests can inspect it.
pub fn tick(cfg: &Config, data_dir: &Path, watchers: &mut [Box<dyn Watcher>]) -> Value {
    let machine = cfg.agent.machine.clone();
    let mut state = state::load_state(data_dir);

    let fresh: Vec<Value> = run_watchers(cfg, &mut state, watchers)
        .into_iter()
        .map(|row| {
            // `{ machine, ...row }` — the machine key goes FIRST, and a row
            // that already carries one wins (spread order).
            let mut out = serde_json::Map::new();
            out.insert("machine".to_string(), json!(machine));
            if let Some(obj) = row.as_object() {
                for (k, v) in obj {
                    out.insert(k.clone(), v.clone());
                }
            }
            Value::Object(out)
        })
        .collect();

    let queued = queue::read_queue(data_dir);

    // SACRED ORDERING: heartbeats are persisted BEFORE the watcher offsets
    // that produced them. A crash between these two writes replays a little
    // work (harmless — ingest is idempotent on the dedupe key); the reverse
    // order would advance the offsets past activity that was never stored,
    // losing it permanently.
    if !fresh.is_empty() {
        if let Err(e) = queue::append_queue(data_dir, &fresh) {
            eprintln!("[stackhour] cannot persist heartbeats: {}", e.message());
        }
    }
    if let Err(e) = state::save_state(data_dir, &state) {
        eprintln!("[stackhour] cannot save state: {}", e.message());
    }

    let mut pending = queued;
    pending.extend(fresh);

    let mut server_failed = false;
    if !pending.is_empty() {
        let take = queue::take_send_batch(&pending, queue::MAX_SEND_BYTES);
        let batch = &pending[..take];
        match http::post_ingest(&cfg.agent.server_url, &cfg.agent.token, batch) {
            Ok(inserted) => {
                let remaining = &pending[take..];
                // The queue is rewritten ONLY after the server confirms;
                // a failed send leaves every row on disk.
                if let Err(e) = queue::save_queue(data_dir, remaining) {
                    eprintln!("[stackhour] cannot rewrite the queue: {}", e.message());
                }
                let tail = if remaining.is_empty() {
                    String::new()
                } else {
                    format!(", {} queued", remaining.len())
                };
                println!(
                    "[stackhour] sent {take} heartbeats ({inserted} new{tail})"
                );
            }
            Err(e) => {
                server_failed = true;
                eprintln!(
                    "[stackhour] server unreachable ({}); queued {}",
                    e.message(),
                    pending.len()
                );
            }
        }
    }

    let queued_after = queue::read_queue(data_dir);
    let queue_bytes = std::fs::metadata(queue::queue_path(data_dir)).map_or(0, |m| m.len());
    let report = json!({
        "time": unix_now(),
        "machine": machine,
        "version": stackhour_core::VERSION,
        // PARITY: the key stays `nodeVersion` because the dashboard and the
        // agent-status table both read it; the VALUE now describes the Rust
        // build.
        "nodeVersion": stackhour_core::build_info(),
        "intervalSeconds": cfg.agent.interval_seconds,
        "queueDepth": queued_after.len(),
        "queueBytes": queue_bytes,
        "watchers": state.get("watcherHealth").cloned().unwrap_or_else(|| json!({})),
    });
    // Skipped when ingest already failed this tick — no point hammering a
    // server we just learned is down.
    if !server_failed {
        http::post_status(&cfg.agent.server_url, &cfg.agent.token, &report);
    }
    report
}

/// Run the agent loop (or a single tick with `once`). Returns the final
/// health report Value (used by `--once` and tests).
pub fn run_agent(cfg: &Config, once: bool) -> Result<Value> {
    let data_dir = cfg.paths.data_dir.clone();
    // Held for the whole run; dropped (and so released) on every exit path.
    let _lock = lock::acquire(&data_dir)?;
    println!(
        "[stackhour] agent starting on {} -> {} (every {}s)",
        cfg.agent.machine, cfg.agent.server_url, cfg.agent.interval_seconds
    );

    let mut watchers = default_watchers();
    let report = tick(cfg, &data_dir, &mut watchers);
    if once {
        return Ok(report);
    }

    // Sleep AFTER the tick (a drifting loop, matching the JS `setTimeout`
    // chain) so a slow tick can never queue up overlapping runs.
    let interval = tick_interval(cfg.agent.interval_seconds);
    loop {
        std::thread::sleep(interval);
        tick(cfg, &data_dir, &mut watchers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `agent.intervalSeconds` is an unvalidated JS `Number()`
    /// coercion, so a config typo like `1e20` used to reach
    /// `Duration::from_secs_f64` and panic (exit 101) after the first
    /// successful tick. Every hostile value must produce a finite Duration.
    #[test]
    fn tick_interval_never_panics_on_hostile_config_values() {
        for hostile in [
            1e20,
            f64::MAX,
            f64::INFINITY,
            f64::NAN,
            f64::NEG_INFINITY,
            -1.0,
            0.0,
        ] {
            let d = tick_interval(hostile);
            assert!(d >= Duration::from_secs(1), "{hostile} floored below 1s");
            assert!(
                d <= Duration::from_secs_f64(TIMEOUT_MAX_SECONDS),
                "{hostile} exceeded the timer ceiling"
            );
        }
    }

    /// Sane values pass through untouched, including sub-second ones being
    /// floored to exactly 1s the way the old `.max(1.0)` did.
    #[test]
    fn tick_interval_preserves_ordinary_values() {
        assert_eq!(tick_interval(20.0), Duration::from_secs(20));
        assert_eq!(tick_interval(0.5), Duration::from_secs(1));
        assert_eq!(tick_interval(1.5), Duration::from_secs_f64(1.5));
    }
}
