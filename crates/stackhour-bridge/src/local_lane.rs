//! The coordinator's LOCAL (gcp) lane: `drainLocal` / `busyLocal` / the
//! single-status-message live progress stream / `deliverFinal`.
//!
//! This is coordinator.mjs lines 288-301 plus 199-208, lifted into a type the
//! daemon can own. Everything user-visible is a registry template and
//! everything spawn-related is an [`EngineDef`] plus a [`LocalTarget`]; the
//! lane itself contains no command list, no button caption and no engine name.
//!
//! ## The behaviours that are easy to get wrong
//!
//! * **Single-flight FIFO.** The JS `if (busyLocal) return; busyLocal = true`
//!   is safe only because JS is single-threaded. With real threads the check,
//!   the flag and the queue must live under ONE mutex, or an enqueue landing
//!   between "queue is empty" and "clear busy" strands the item forever. See
//!   [`Inner`] — `busy` and `queue` are never locked separately.
//! * **The send/edit asymmetry.** The FIRST status message is sent plain and
//!   UNESCAPED with no `parse_mode`; every later render of the same message id
//!   is `esc()`aped and sent as HTML. A label containing `<` therefore renders
//!   literally at first and as an entity afterwards. That is what the owner
//!   sees today, so it is preserved rather than tidied.
//! * **Dedupe by intent.** `last_shown` is set BEFORE the API call, so a
//!   failed edit is never retried with the same text — matching the JS.
//! * **deliverFinal ordering.** rich send -> delete the status message
//!   (whether or not the rich send worked) -> only then the chunked-HTML
//!   fallback. The status message is always gone before any fallback chunk
//!   lands, and on total transport failure the user sees it vanish and gets
//!   nothing back. Preserved deliberately.
//! * **The retry log line goes to the LOG.** Not to the activity channel: the
//!   activity channel is rendered as a Telegram status edit.

use crate::engines::{self, RunRequest, RunResult, RunningJob};
use crate::render;
use crate::telegram::Tg;
use serde_json::Value;
use stackhour_core::registry::EngineDef;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// A target the local lane can run on (`targets.<name>` with `type: local`).
/// Built by the coordinator from its `TargetCfg`; the lane never reads
/// config.json itself.
#[derive(Debug, Clone, Default)]
pub struct LocalTarget {
    /// The target key, e.g. `gcp`.
    pub name: String,
    /// Display label; falls back to `name` when the config has none.
    pub label: String,
    pub cwd: Option<PathBuf>,
    /// `claudeBin` / `codexBin` for the engine being run, already resolved by
    /// the caller (it knows which engine the job picked).
    pub bin: Option<String>,
    /// Becomes the child's WHOLE `PATH` when set — see [`RunRequest`].
    pub extra_path: Option<String>,
    /// `tgt.permissionMode`; `None` renders as `default`.
    pub permission_mode: Option<String>,
    /// `tgt.model` for claude, `tgt.codexModel` for codex — again resolved by
    /// the caller against the job's engine.
    pub model: Option<String>,
}

/// One prompt queued on the local lane. The engine and target are captured at
/// ENQUEUE time, so switching engine while a job waits does not retarget it.
#[derive(Debug, Clone, Default)]
pub struct LocalJob {
    /// The prompt as the engine will see it — media prompts are already
    /// rewritten by the media lane before they get here.
    pub prompt: String,
    /// Engine name, resolved against the registry when the job runs.
    pub engine: String,
    /// Target key. A target that is not declared `local` falls back to the
    /// lane's `default_target` (JS: `targets[target]?.type === 'local' ?
    /// target : 'gcp'`).
    pub target: String,
    /// Active agent at enqueue time, for the session key.
    pub agent: Option<String>,
}

/// Everything the lane needs from the rest of the bridge, behind one trait so
/// the lane can be driven by a test double and so the other areas' internals
/// stay theirs.
pub trait LaneContext: Send + Sync {
    /// The engine definition for a job's engine name, or `None` if unknown.
    fn engine(&self, name: &str) -> Option<EngineDef>;
    /// The resolved target for a job's engine, or `None` if it is not a local
    /// target.
    fn target(&self, name: &str, engine: &str) -> Option<LocalTarget>;
    /// Render a registry prompt template.
    fn prompt(&self, name: &str, vars: &[(&str, &str)]) -> String;
    /// The control keyboard, built fresh from live state.
    fn control_keyboard(&self) -> Value;
    /// Stored session id for this (target, engine, agent).
    fn session(&self, target: &str, engine: &str, agent: Option<&str>) -> Option<String>;
    /// Persist a session id. The JS `setSession` writes state.json
    /// immediately; implementations must do the same or a restart loses it.
    fn set_session(&self, target: &str, engine: &str, agent: Option<&str>, id: Option<String>);
    /// Append a line to the coordinator log. NEVER shown in Telegram.
    fn log(&self, line: &str);
    /// Wall-clock milliseconds. Injectable so duration footers are testable.
    fn now_ms(&self) -> i64;
    /// The target key an undeclared/unknown target falls back to.
    fn default_target(&self) -> String;
}

/// Registry template names this lane renders. Named, not inlined, so the set
/// of strings the lane can produce is greppable in one place.
mod tpl {
    pub const STATUS_LINE: &str = "status-line";
    pub const STATUS_WORKING: &str = "status-working";
    pub const FINAL: &str = "final";
    pub const NO_OUTPUT: &str = "no-output";
    pub const EMPTY_CHUNK: &str = "empty-chunk";
    pub const HOUSE_RULES: &str = "house-rules";
    pub const HOUSE_RULES_TURN: &str = "house-rules-turn";
    pub const ERROR_RUN: &str = "error-run";
    pub const ERROR_EXIT: &str = "error-exit";
    pub const ERROR_GENERIC: &str = "error-generic";
}

/// The queue and the busy flag, under ONE lock. Splitting them reintroduces
/// the TOCTOU the JS cannot have.
#[derive(Default)]
struct Inner {
    queue: VecDeque<LocalJob>,
    busy: bool,
    /// The child of the job currently running, for `/stop`.
    current: Option<RunningJob>,
}

/// The local lane. Clone-cheap; every clone shares one queue.
#[derive(Clone)]
pub struct LocalLane {
    tg: Arc<Tg>,
    ctx: Arc<dyn LaneContext>,
    inner: Arc<Mutex<Inner>>,
}

impl LocalLane {
    pub fn new(tg: Arc<Tg>, ctx: Arc<dyn LaneContext>) -> LocalLane {
        LocalLane {
            tg,
            ctx,
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// `busyLocal` — what `/where` reports as `GCP busy`.
    pub fn is_busy(&self) -> bool {
        self.inner.lock().map(|g| g.busy).unwrap_or(false)
    }

    /// Queue length, for tests and diagnostics.
    pub fn queued(&self) -> usize {
        self.inner.lock().map(|g| g.queue.len()).unwrap_or(0)
    }

    /// SIGTERM the running child, if any. Returns whether there was one.
    ///
    /// `/stop` deliberately does NOT clear the queue — the next queued prompt
    /// starts immediately, exactly as in the JS. A SIGTERM'd child exits with
    /// no code, so the resume-retry rule does not fire and the job still
    /// delivers a final message ("(no output)" plus the footer).
    pub fn stop_current(&self) -> bool {
        let job = self.inner.lock().ok().and_then(|g| g.current.clone());
        match job {
            Some(job) => {
                job.terminate();
                true
            }
            None => false,
        }
    }

    /// `routePrompt`'s local branch: push and drain. Returns immediately; the
    /// drain runs on its own thread when the lane is idle.
    pub fn enqueue(&self, job: LocalJob) {
        {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            inner.queue.push_back(job);
            if inner.busy {
                return; // an in-flight drain will pick it up
            }
            inner.busy = true;
        }
        let lane = self.clone();
        std::thread::spawn(move || lane.drain());
    }

    /// Drain the queue to empty, then clear `busy` — the pop and the clear
    /// happen under the same lock, so an enqueue can never be stranded.
    fn drain(&self) {
        loop {
            let next = {
                let Ok(mut inner) = self.inner.lock() else {
                    return;
                };
                match inner.queue.pop_front() {
                    Some(job) => job,
                    None => {
                        inner.busy = false;
                        return;
                    }
                }
            };
            self.run_one(next);
        }
    }

    /// One queue item, start to delivered message. Every failure is contained:
    /// the loop must continue and `busy` must always be cleared.
    fn run_one(&self, job: LocalJob) {
        let t0 = self.ctx.now_ms();
        let engine_label = self
            .ctx
            .engine(&job.engine)
            .map(|d| d.label)
            .unwrap_or_else(|| job.engine.clone());

        // `targets[target]?.type === 'local' ? target : 'gcp'` — anything not
        // declared local silently runs on the default target.
        let (name, target) = match self.ctx.target(&job.target, &job.engine) {
            Some(t) => (job.target.clone(), t),
            None => {
                let fallback = self.ctx.default_target();
                match self.ctx.target(&fallback, &job.engine) {
                    Some(t) => (fallback, t),
                    None => {
                        self.ctx
                            .log(&format!("drainLocal err unknown target {}", job.target));
                        return;
                    }
                }
            }
        };
        let target_label = if target.label.is_empty() {
            name.clone()
        } else {
            target.label.clone()
        };

        let mut status = StatusMessage::new(
            Arc::clone(&self.tg),
            Arc::clone(&self.ctx),
            engine_label.clone(),
            target_label.clone(),
        );
        let working = self.ctx.prompt(tpl::STATUS_WORKING, &[]);
        status.show(&working);
        let _ = self.tg.typing();

        let Some(def) = self.ctx.engine(&job.engine) else {
            let msg = self.ctx.prompt(
                tpl::ERROR_GENERIC,
                &[("error", &format!("unknown engine {}", job.engine))],
            );
            self.ctx
                .log(&format!("drainLocal err unknown engine {}", job.engine));
            status.fail(&msg);
            return;
        };

        let result = self.run_engine(&def, &job, &name, &target, &mut status);
        let body = self.final_text(&result, &engine_label);
        let final_text = self.ctx.prompt(
            tpl::FINAL,
            &[
                ("text", &body),
                ("engine", &engine_label),
                ("target", &target_label),
                ("duration", &render::fmt_dur(self.ctx.now_ms() - t0)),
            ],
        );
        deliver_final(&self.tg, &*self.ctx, &final_text, status.message_id());
    }

    /// Spawn the engine, stream its activity into the status message, persist
    /// the captured session.
    fn run_engine(
        &self,
        def: &EngineDef,
        job: &LocalJob,
        target_name: &str,
        target: &LocalTarget,
        status: &mut StatusMessage,
    ) -> RunResult {
        let session = self.ctx.session(target_name, &job.engine, job.agent.as_deref());
        let house_rules = match self.ctx.prompt(tpl::HOUSE_RULES, &[]) {
            s if s.trim().is_empty() => None,
            s => Some(s),
        };
        let req = RunRequest {
            prompt: job.prompt.clone(),
            session_id: session.clone(),
            model: target.model.clone(),
            permission_mode: target.permission_mode.clone(),
            cwd: target.cwd.clone(),
            live_status: true,
            extra_path: target.extra_path.clone(),
            bin: target.bin.clone(),
            house_rules,
            house_rules_turn: Some(self.ctx.prompt(tpl::HOUSE_RULES_TURN, &[])),
            ..RunRequest::default()
        };

        // The activity channel feeds status edits on this thread's behalf.
        let (tx, rx) = mpsc::channel::<String>();
        let pump = {
            let tg = Arc::clone(&self.tg);
            let mut status = status.clone();
            std::thread::spawn(move || {
                for line in rx {
                    status.show(&line);
                    let _ = tg.typing();
                }
                status
            })
        };

        let (running, handle) = engines::spawn_engine(def, req.clone(), Some(tx.clone()));
        self.set_current(Some(running));
        let mut result = handle.join().unwrap_or_else(|_| RunResult {
            error: Some("engine reader thread panicked".to_string()),
            ..RunResult::default()
        });

        // The resume-retry rule, with the coordinator's own log line and the
        // log FILE as its sink.
        let failed = result.code.is_some_and(|c| c != 0);
        if failed && session.is_some() && result.text.is_empty() {
            self.ctx.log(&engines::resume_retry_log_line_local(
                &def.name,
                target_name,
                result.code,
            ));
            let mut fresh = req;
            fresh.session_id = None;
            let (running, handle) = engines::spawn_engine(def, fresh, Some(tx.clone()));
            self.set_current(Some(running));
            result = handle.join().unwrap_or_else(|_| RunResult {
                error: Some("engine reader thread panicked".to_string()),
                ..RunResult::default()
            });
            result.retried_fresh = true;
        }
        self.set_current(None);

        drop(tx);
        if let Ok(pumped) = pump.join() {
            status.adopt(pumped);
        }

        // A falsy captured session leaves the stored one untouched — it is
        // never cleared by a failed run (JS: `if (r.sessionId) setSession(…)`).
        if let Some(id) = result.session_id.clone() {
            if !id.is_empty() {
                self.ctx
                    .set_session(target_name, &job.engine, job.agent.as_deref(), Some(id));
            }
        }
        result
    }

    fn set_current(&self, job: Option<RunningJob>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.current = job;
        }
    }

    /// `r.text || (error ? … : code ? … : '(no output)')` — ANY non-empty text
    /// wins over an error, and a clean exit that said nothing is `(no output)`,
    /// not an error.
    fn final_text(&self, r: &RunResult, engine_label: &str) -> String {
        if !r.text.is_empty() {
            return r.text.clone();
        }
        if let Some(err) = &r.error {
            return self.ctx.prompt(tpl::ERROR_RUN, &[("error", err)]);
        }
        match r.code {
            Some(code) if code != 0 => self.ctx.prompt(
                tpl::ERROR_EXIT,
                &[
                    ("engine", engine_label),
                    ("code", &code.to_string()),
                    ("stderr", &stderr_tail(&r.stderr)),
                ],
            ),
            // Includes code == None, i.e. a /stop-killed child: the JS falls
            // through to '(no output)' there too, so a stopped job still
            // delivers a message.
            _ => self.ctx.prompt(tpl::NO_OUTPUT, &[]),
        }
    }
}

/// Last 500 chars of stderr, as the JS `.slice(-500)` does (UTF-16 units).
fn stderr_tail(stderr: &str) -> String {
    const TAIL: usize = 500;
    let units: Vec<u16> = stderr.encode_utf16().collect();
    if units.len() <= TAIL {
        return stderr.to_string();
    }
    String::from_utf16_lossy(&units[units.len() - TAIL..])
}

/// The ONE status message a job owns for its whole life.
#[derive(Clone)]
struct StatusMessage {
    tg: Arc<Tg>,
    ctx: Arc<dyn LaneContext>,
    engine_label: String,
    target_label: String,
    /// Shared so the activity pump thread and the lane thread agree on which
    /// message they are editing and what was last rendered.
    shared: Arc<Mutex<StatusShared>>,
}

#[derive(Default)]
struct StatusShared {
    message_id: Option<i64>,
    last_shown: String,
}

impl StatusMessage {
    fn new(
        tg: Arc<Tg>,
        ctx: Arc<dyn LaneContext>,
        engine_label: String,
        target_label: String,
    ) -> StatusMessage {
        StatusMessage {
            tg,
            ctx,
            engine_label,
            target_label,
            shared: Arc::new(Mutex::new(StatusShared::default())),
        }
    }

    fn message_id(&self) -> Option<i64> {
        self.shared.lock().ok().and_then(|s| s.message_id)
    }

    /// Adopt another handle's state. The two share an `Arc` already, so this
    /// exists only to make the hand-back from the pump thread explicit.
    fn adopt(&mut self, other: StatusMessage) {
        self.shared = other.shared;
    }

    /// Render one activity line into the status message.
    ///
    /// First emission: a PLAIN, unescaped `sendMessage`. Every later one: an
    /// HTML-escaped `editMessageText` of the same id. Both carry the stop
    /// keyboard. Identical text is a no-op with no API call at all.
    fn show(&mut self, activity: &str) {
        let activity = if activity.is_empty() {
            self.ctx.prompt(tpl::STATUS_WORKING, &[])
        } else {
            activity.to_string()
        };
        let txt = self.ctx.prompt(
            tpl::STATUS_LINE,
            &[
                ("engine", &self.engine_label),
                ("target", &self.target_label),
                ("activity", &activity),
            ],
        );

        // last_shown is set BEFORE the call: dedupe is by intent, so a failed
        // edit is never retried with the same text (JS parity).
        let existing = {
            let Ok(mut shared) = self.shared.lock() else {
                return;
            };
            if shared.last_shown == txt {
                return;
            }
            shared.last_shown = txt.clone();
            shared.message_id
        };

        let kb = crate::keyboard::stop_keyboard();
        let extra = serde_json::json!({ "reply_markup": kb });
        match existing {
            Some(id) => {
                self.tg
                    .edit_message(id, &render::esc(&txt), Some("HTML"), Some(&extra));
            }
            None => {
                // If this send fails the id stays None and the NEXT status
                // update sends a fresh message — the JS does the same, and a
                // long job against a failing API will spam status messages.
                let sent = self.tg.send_message(&txt, None, Some(&extra));
                if let Some(id) = sent
                    .as_ref()
                    .and_then(|m| m.get("message_id"))
                    .and_then(Value::as_i64)
                {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.message_id = Some(id);
                    }
                }
            }
        }
    }

    /// The drain-level error path: the status message is EDITED in place with
    /// the control keyboard swapping in for the stop keyboard, and is NOT
    /// deleted. When there is no status message the error is sent fresh.
    fn fail(&self, message: &str) {
        let extra = serde_json::json!({ "reply_markup": self.ctx.control_keyboard() });
        match self.message_id() {
            Some(id) => {
                self.tg
                    .edit_message(id, &render::esc(message), Some("HTML"), Some(&extra));
            }
            None => {
                self.tg
                    .send_message(&render::esc(message), Some("HTML"), Some(&extra));
            }
        }
    }
}

/// `deliverFinal(final, statusId)` — the shared delivery path for both lanes.
///
/// Ordering is load-bearing and reproduced exactly:
/// 1. try the rich send WITH the control keyboard;
/// 2. delete the status message — whether or not step 1 worked, and BEFORE
///    any fallback, so the user never sees a gap;
/// 3. only if step 1 failed, send the chunked HTML fallback. Intermediate
///    chunks carry NO keyboard; only the last one does.
///
/// `sendRichMessage` is not a real Bot API method today, so step 1 normally
/// 400s and step 3 is what actually delivers. That wasted call is deliberate:
/// the day the account gets the method, the rich path starts working with no
/// code change.
pub fn deliver_final(tg: &Tg, ctx: &dyn LaneContext, text: &str, status_id: Option<i64>) {
    let kb = ctx.control_keyboard();
    let sent = tg.send_rich(text, Some(serde_json::json!({ "reply_markup": kb.clone() })));

    if let Some(id) = status_id {
        tg.delete(id);
    }
    if sent.is_some() {
        return;
    }

    // Tables are converted to aligned code blocks ONLY here. A successful rich
    // send passes them through as raw markdown, so the two paths render tables
    // completely differently — existing behaviour.
    let body = if render::has_table(text) {
        render::rewrite_tables(text)
    } else {
        text.to_string()
    };
    let chunks = render::html_chunks(&body, render::DELIVER_FINAL_CHUNK_LIMIT);
    let last = chunks.len().saturating_sub(1);
    for (i, chunk) in chunks.iter().enumerate() {
        if i == last {
            let body = if chunk.is_empty() {
                ctx.prompt(tpl::EMPTY_CHUNK, &[])
            } else {
                chunk.clone()
            };
            tg.send_message(
                &body,
                Some("HTML"),
                Some(&serde_json::json!({ "reply_markup": kb })),
            );
        } else {
            tg.send_message(chunk, Some("HTML"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// A [`LaneContext`] backed by the real registry defaults, so the template
    /// bodies under test are the shipped ones.
    struct TestCtx {
        reg: stackhour_core::registry::Registry,
        logs: Mutex<Vec<String>>,
        sessions: Mutex<Vec<(String, String, Option<String>)>>,
        clock: AtomicI64,
        target: Mutex<Option<LocalTarget>>,
    }

    impl TestCtx {
        fn new() -> Arc<TestCtx> {
            Arc::new(TestCtx {
                reg: stackhour_core::registry::load_with(
                    std::path::Path::new("/nonexistent-stackhour-config"),
                    stackhour_core::registry::EnvSource::fixed(&[]),
                ),
                logs: Mutex::new(Vec::new()),
                sessions: Mutex::new(Vec::new()),
                clock: AtomicI64::new(0),
                target: Mutex::new(Some(LocalTarget {
                    name: "gcp".into(),
                    label: "☁️ GCP".into(),
                    ..LocalTarget::default()
                })),
            })
        }
    }

    impl LaneContext for TestCtx {
        fn engine(&self, name: &str) -> Option<EngineDef> {
            self.reg.engines.get(name).cloned()
        }
        fn target(&self, name: &str, _engine: &str) -> Option<LocalTarget> {
            let t = self.target.lock().unwrap().clone()?;
            if t.name == name {
                Some(t)
            } else {
                None
            }
        }
        fn prompt(&self, name: &str, vars: &[(&str, &str)]) -> String {
            self.reg.prompts.render(name, vars)
        }
        fn control_keyboard(&self) -> Value {
            serde_json::json!({ "inline_keyboard": [] })
        }
        fn session(&self, _t: &str, _e: &str, _a: Option<&str>) -> Option<String> {
            None
        }
        fn set_session(&self, t: &str, e: &str, _a: Option<&str>, id: Option<String>) {
            self.sessions.lock().unwrap().push((t.into(), e.into(), id));
        }
        fn log(&self, line: &str) {
            self.logs.lock().unwrap().push(line.to_string());
        }
        fn now_ms(&self) -> i64 {
            self.clock.load(Ordering::SeqCst)
        }
        fn default_target(&self) -> String {
            "gcp".into()
        }
    }

    fn lane(ctx: Arc<TestCtx>) -> LocalLane {
        // A closed loopback port: every call fails fast. One attempt with no
        // backoff keeps the lock-discipline test about locks, not about the
        // transport's retry ladder.
        let mut cfg = crate::telegram::TgConfig::new("t", 1).with_api_root("http://127.0.0.1:1");
        cfg.attempts = 1;
        cfg.backoff_base_ms = 0;
        let tg = Arc::new(Tg::with_config(cfg));
        LocalLane::new(tg, ctx)
    }

    #[test]
    fn the_final_text_prefers_any_output_over_an_error() {
        let ctx = TestCtx::new();
        let lane = lane(Arc::clone(&ctx));
        let r = RunResult {
            text: "the answer".into(),
            error: Some("boom".into()),
            code: Some(1),
            ..RunResult::default()
        };
        assert_eq!(lane.final_text(&r, "Claude"), "the answer");
    }

    #[test]
    fn a_clean_exit_with_no_text_is_no_output_not_an_error() {
        let ctx = TestCtx::new();
        let lane = lane(Arc::clone(&ctx));
        let r = RunResult {
            code: Some(0),
            ..RunResult::default()
        };
        assert_eq!(lane.final_text(&r, "Claude"), "(no output)");
    }

    #[test]
    fn a_signal_killed_child_also_falls_through_to_no_output() {
        // /stop SIGTERMs the child, which exits with no code. The JS's
        // `r.code ? … : '(no output)'` therefore delivers a message anyway.
        let ctx = TestCtx::new();
        let lane = lane(Arc::clone(&ctx));
        let r = RunResult {
            code: None,
            ..RunResult::default()
        };
        assert_eq!(lane.final_text(&r, "Claude"), "(no output)");
    }

    #[test]
    fn a_nonzero_exit_renders_the_engine_label_and_the_stderr_tail() {
        let ctx = TestCtx::new();
        let lane = lane(Arc::clone(&ctx));
        let r = RunResult {
            code: Some(3),
            stderr: "x".repeat(600),
            ..RunResult::default()
        };
        let out = lane.final_text(&r, "Codex");
        assert!(out.starts_with("⚠️ Codex exited (code 3)."), "{out}");
        assert!(out.ends_with(&"x".repeat(500)));
        assert!(!out.ends_with(&"x".repeat(501)));
    }

    #[test]
    fn stderr_shorter_than_the_tail_is_kept_whole() {
        assert_eq!(stderr_tail("short"), "short");
    }

    #[test]
    fn the_footer_carries_the_engine_target_and_duration() {
        let ctx = TestCtx::new();
        let out = ctx.prompt(
            tpl::FINAL,
            &[
                ("text", "hi"),
                ("engine", "Claude"),
                ("target", "☁️ GCP"),
                ("duration", &render::fmt_dur(95_000)),
            ],
        );
        assert_eq!(out, "hi\n\n— Claude · ☁️ GCP · 1m35s");
    }

    #[test]
    fn the_status_line_uses_the_registry_template() {
        let ctx = TestCtx::new();
        let out = ctx.prompt(
            tpl::STATUS_LINE,
            &[
                ("engine", "Claude"),
                ("target", "☁️ GCP"),
                ("activity", &ctx.prompt(tpl::STATUS_WORKING, &[])),
            ],
        );
        assert_eq!(out, "▹ Claude · ☁️ GCP · working…");
    }

    #[test]
    fn enqueue_never_strands_an_item_and_always_clears_busy() {
        // The queue is drained by a real thread; with an unreachable API every
        // send fails fast, so this exercises the lock discipline, not the
        // transport.
        let ctx = TestCtx::new();
        let lane = lane(Arc::clone(&ctx));
        // No engine named `nope` -> run_one bails after the status attempt.
        for _ in 0..8 {
            lane.enqueue(LocalJob {
                prompt: "hi".into(),
                engine: "nope".into(),
                target: "gcp".into(),
                agent: None,
            });
        }
        for _ in 0..200 {
            if !lane.is_busy() && lane.queued() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!lane.is_busy(), "busy flag was left set");
        assert_eq!(lane.queued(), 0, "items were stranded in the queue");
        assert_eq!(ctx.logs.lock().unwrap().len(), 8);
    }

    #[test]
    fn stop_current_reports_whether_there_was_a_child() {
        let ctx = TestCtx::new();
        let lane = lane(Arc::clone(&ctx));
        assert!(!lane.stop_current());
        lane.set_current(Some(RunningJob::default()));
        assert!(lane.stop_current());
    }
}
