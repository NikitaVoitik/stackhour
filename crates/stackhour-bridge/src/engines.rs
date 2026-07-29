//! Data-driven engine runner over registry EngineDefs.
//!
//! Argv assembly (fresh vs resume — claude flag-style appended, codex
//! subcommand inserted with the sessionId before the '-' stdin sentinel;
//! model / permission / system-prompt flag splicing; partial_messages_flag
//! only on the live-status coordinator lane); spawn with cwd/env/PATH;
//! prompt via stdin (or last-arg); a reader thread per child parsing the
//! StreamKind (claude: first-session-id-wins except result overwrite,
//! assistant events REPLACE text, result kept when longer; codex:
//! thread.started overwrites session, agent_message APPENDS with '\n\n';
//! plain-lines: accumulate); activity events over an mpsc channel for status
//! edits; a stderr capture thread; the resume-retry rule (exit code != 0 &&
//! had sessionId && no text -> ONE fresh rerun with the exact log line); a
//! SIGTERM handle for /stop.
//!
//! ## The extensibility contract
//!
//! Nothing in this module matches on an engine NAME. Everything it needs —
//! the binary, the flags, how the prompt is delivered, which of the three
//! shipped stream parsers to use, extra env — is read off the [`EngineDef`],
//! which may equally well have come from `engines/<name>.toml`. A brand-new
//! engine is therefore a config change, not a code change; see
//! `tests/custom_engine_e2e.rs`, which is the regression gate for that.

use stackhour_core::registry::engine::ArgvVars;
use stackhour_core::registry::{EngineDef, PromptDelivery, StreamKind};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Minimum gap between streamed activity edits for the JSON stream parsers
/// (parity with coordinator.mjs's `Date.now() - lastStatus > 800`). The
/// `plain-lines` parser has no upstream throttle and emits every line.
const ACTIVITY_THROTTLE: Duration = Duration::from_millis(800);

/// One engine invocation request.
#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    pub prompt: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    /// Trusted system prompt for this run. Engines with no system-prompt argv
    /// support receive it as a bounded prompt prefix on fresh attempts.
    pub system_prompt: Option<String>,
    /// Reasoning effort, spliced through the engine's declared `effort_args`.
    /// Engines that declare none ignore it (see `souls::agent_effort`).
    pub effort: Option<String>,
    pub cwd: Option<PathBuf>,
    /// true on the coordinator's local lane (enables partial_messages_flag).
    pub live_status: bool,
    /// Target `extraPath`. When set it becomes the child's WHOLE `PATH`, not a
    /// prefix — coordinator.mjs spawns with `PATH: tgt.extraPath ||
    /// process.env.PATH`, so the child cannot see anything the target did not
    /// list. Reproduced deliberately: prepending would silently widen what a
    /// target's engine can execute.
    pub extra_path: Option<String>,
    /// Per-target binary override (`targets.<name>.claudeBin` / `codexBin`).
    /// `None` falls back to the [`EngineDef`]'s own `bin`.
    pub bin: Option<String>,
    /// The standing house rules (registry `house-rules`). Used only when no
    /// agent supplied a `system_prompt`: engines that declare
    /// `system_prompt_args` get it as their system prompt, engines that do not
    /// get it prepended to the prompt on FRESH attempts only.
    pub house_rules: Option<String>,
    /// Body of the `house-rules-turn` template, pre-rendered by the caller
    /// with `{{system}}` still in place. `None` uses the built-in shape.
    pub house_rules_turn: Option<String>,
    /// Effective tool allow-list (agent `[tools]` unioned with its skills').
    /// Empty = unrestricted.
    pub allow_tools: Vec<String>,
    /// Effective tool deny-list. Empty = nothing denied.
    pub deny_tools: Vec<String>,
}

/// What an engine run produced.
#[derive(Debug, Clone, Default)]
pub struct RunResult {
    pub text: String,
    pub session_id: Option<String>,
    /// Child exit code (None = killed by signal, or the spawn itself failed).
    pub code: Option<i32>,
    pub error: Option<String>,
    pub stderr: String,
    /// True when the resume-retry rule fired and this is the fresh rerun.
    pub retried_fresh: bool,
}

/// A live child handle, used by /stop. Cheap to clone; terminating an already
/// exited child is a no-op.
#[derive(Debug, Clone, Default)]
pub struct RunningJob {
    /// `None` once the child has been reaped, or if the spawn failed.
    pid: Arc<Mutex<Option<u32>>>,
}

impl RunningJob {
    /// SIGTERM the child (best-effort, idempotent).
    pub fn terminate(&self) {
        let pid = self.pid.lock().ok().and_then(|g| *g);
        let Some(pid) = pid else { return };
        #[cfg(unix)]
        if let Ok(raw_pid) = i32::try_from(pid) {
            if let Some(pid) = rustix::process::Pid::from_raw(raw_pid) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
        }
        #[cfg(not(unix))]
        let _ = pid;
    }
}

/// The resume-retry log line the MAC WORKER writes (worker.mjs).
///
/// The coordinator's local lane writes a DIFFERENT one — see
/// [`resume_retry_log_line_local`]. Two lanes, two strings; the reference has
/// both and a single shared wording would be a (small) divergence.
pub fn resume_retry_log_line(engine: &str, code: Option<i32>) -> String {
    format!(
        "{engine} resume failed ({}); retry fresh",
        code.unwrap_or_default()
    )
}

/// The resume-retry log line the coordinator's LOCAL lane writes
/// (coordinator.mjs:281) — it names the target and says "retrying", not
/// "retry".
///
/// This goes to the log FILE only. The JS never shows it in Telegram, so it
/// must not be pushed down the activity channel, which would render it as a
/// status edit the user sees.
pub fn resume_retry_log_line_local(engine: &str, target: &str, code: Option<i32>) -> String {
    format!(
        "{engine} resume failed on {target} ({}); retrying fresh",
        code.unwrap_or_default()
    )
}

/// The default `house-rules-turn` shape, used when the caller passes none.
const HOUSE_RULES_TURN_FALLBACK: &str = "[Standing style rules]\n{{system}}\n\n{{prompt}}";
const SYSTEM_PROMPT_TURN_FALLBACK: &str = "[System instructions]\n{{system}}\n\n[User message]\n{{prompt}}";

/// Deliver a trusted system prompt to engines that have no dedicated argv
/// support. Resumed sessions already carry the instructions, while a failed
/// resume retried fresh receives them again.
pub fn apply_system_prompt(def: &EngineDef, req: &mut RunRequest) {
    if def.system_prompt_args.is_some() || req.session_id.is_some() {
        return;
    }
    let Some(system) = req.system_prompt.take() else {
        return;
    };
    if system.trim().is_empty() {
        return;
    }
    req.prompt = SYSTEM_PROMPT_TURN_FALLBACK
        .replace("{{system}}", &system)
        .replace("{{prompt}}", &req.prompt);
}

/// Fold the standing house rules into a request, exactly where the JS puts
/// them.
///
/// * An engine that declares `system_prompt_args` (claude) takes them as its
///   system prompt — but only when no agent already supplied one. An active
///   agent's composed soul is a deliberate override, not an addition.
/// * An engine that does not (codex) gets them prepended to the prompt, and
///   ONLY on a fresh attempt: coordinator.mjs guards the prefix with
///   `engine === 'codex' && !resume`, so a resumed thread — which already
///   carries the rules — does not get them again. Because the resume-retry
///   rerun clears `session_id`, the retry picks the prefix up, matching the
///   JS, where the prompt is composed per attempt rather than per run.
///
/// Empty or whitespace-only rules are a no-op, which is how a user disables
/// them: an empty `prompts/house-rules.md`.
pub fn apply_house_rules(def: &EngineDef, req: &mut RunRequest) {
    let Some(rules) = req.house_rules.clone() else {
        return;
    };
    if rules.trim().is_empty() {
        return;
    }
    if def.system_prompt_args.is_some() {
        if req.system_prompt.is_none() {
            req.system_prompt = Some(rules);
        }
        return;
    }
    if req.system_prompt.is_some() || req.session_id.is_some() {
        return;
    }
    let template = req
        .house_rules_turn
        .clone()
        .unwrap_or_else(|| HOUSE_RULES_TURN_FALLBACK.to_string());
    req.prompt = template
        .replace("{{system}}", &rules)
        .replace("{{prompt}}", &req.prompt);
}

/// Assemble the final argv for a request (unit-tested standalone — the argv
/// byte-parity surface). Delegates to the registry's reference implementation
/// so config-defined and built-in engines take the identical path.
pub fn build_argv(def: &EngineDef, req: &RunRequest) -> Vec<String> {
    let mut argv = def.assemble_argv(&ArgvVars {
        session_id: req.session_id.as_deref(),
        model: req.model.as_deref(),
        permission_mode: req.permission_mode.as_deref(),
        system_prompt: req.system_prompt.as_deref(),
        effort: req.effort.as_deref(),
        allow_tools: &req.allow_tools,
        deny_tools: &req.deny_tools,
        live_status: req.live_status,
    });
    // Engines that take the prompt as a positional argument get it appended
    // last; stdin engines already carry their `-` sentinel from the template.
    if def.prompt_delivery == PromptDelivery::LastArg {
        argv.push(req.prompt.clone());
    }
    argv
}

/// Run the engine to completion, streaming activity lines to `activity`.
pub fn run_engine(def: &EngineDef, req: RunRequest, activity: Option<mpsc::Sender<String>>) -> RunResult {
    let (_job, handle) = spawn_engine(def, req, activity);
    handle.join().unwrap_or_else(|_| RunResult {
        error: Some("engine reader thread panicked".to_string()),
        ..RunResult::default()
    })
}

/// Like [`run_engine`], plus the resume-retry rule: exit code != 0 with a
/// session id and no text -> ONE fresh (session-less) rerun. `on_retry` is
/// called once, with the failed attempt's exit code, just before the rerun;
/// the caller formats and logs its own lane's line.
pub fn run_with_resume_retry(
    def: &EngineDef,
    req: RunRequest,
    activity: Option<mpsc::Sender<String>>,
    on_retry: Option<&dyn Fn(Option<i32>)>,
) -> RunResult {
    let had_session = req.session_id.is_some();
    let mut fresh = req.clone();
    let first = run_engine(def, req, activity.clone());

    let failed = first.code.is_some_and(|c| c != 0);
    if !(failed && had_session && first.text.is_empty()) {
        return first;
    }
    // The lane owns the wording AND the sink: the coordinator writes
    // `resume_retry_log_line_local` to coordinator.log, the worker writes
    // `resume_retry_log_line` to its own log. Neither reaches Telegram.
    if let Some(log) = on_retry {
        log(first.code);
    }
    fresh.session_id = None;
    let mut second = run_engine(def, fresh, activity);
    second.retried_fresh = true;
    second
}

/// Spawn variant returning the terminate handle alongside the join logic
/// (used by the coordinator's local lane).
pub fn spawn_engine(
    def: &EngineDef,
    mut req: RunRequest,
    activity: Option<mpsc::Sender<String>>,
) -> (RunningJob, std::thread::JoinHandle<RunResult>) {
    let job = RunningJob::default();
    apply_system_prompt(def, &mut req);
    apply_house_rules(def, &mut req);
    let argv = build_argv(def, &req);

    // The target's per-engine binary override wins over the EngineDef's own
    // `bin` (JS: `tgt.claudeBin` / `tgt.codexBin || <default>`).
    let bin = req.bin.clone().unwrap_or_else(|| def.bin.clone());
    let mut cmd = Command::new(&bin);
    cmd.args(&argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in &def.env {
        cmd.env(k, v);
    }
    if let Some(extra) = &req.extra_path {
        cmd.env("PATH", extra);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Node resolves `child.on('error')` with the message, NO code, and
            // the session id captured so far — which on a spawn failure is the
            // one we were asked to resume. Dropping it would silently clear
            // context the JS preserves.
            let msg = e.to_string();
            let seeded = req.session_id.clone();
            let handle = std::thread::spawn(move || RunResult {
                error: Some(msg),
                session_id: seeded,
                ..RunResult::default()
            });
            return (job, handle);
        }
    };
    if let Ok(mut slot) = job.pid.lock() {
        *slot = Some(child.id());
    }

    // Prompt on stdin, on its own thread: a prompt larger than the pipe
    // buffer would otherwise deadlock against our own stdout reader.
    let stdin = child.stdin.take();
    if def.prompt_delivery == PromptDelivery::Stdin {
        let prompt = req.prompt.clone();
        std::thread::spawn(move || {
            if let Some(mut w) = stdin {
                let _ = w.write_all(prompt.as_bytes());
            }
        });
    } else {
        drop(stdin);
    }

    let stderr = child.stderr.take();
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut r) = stderr {
            let _ = r.read_to_string(&mut buf);
        }
        buf
    });

    let stdout = child.stdout.take();
    let kind = def.kind;
    let seed_session = req.session_id.clone();
    let pid_slot = Arc::clone(&job.pid);

    let handle = std::thread::spawn(move || {
        let mut state = StreamState::new(kind, seed_session);
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                state.feed(&line, activity.as_ref());
            }
        }
        let code = child.wait().ok().and_then(|s| s.code());
        if let Ok(mut slot) = pid_slot.lock() {
            *slot = None;
        }
        RunResult {
            text: state.text,
            session_id: state.session_id,
            code,
            error: None,
            stderr: stderr_thread.join().unwrap_or_default(),
            retried_fresh: false,
        }
    });

    (job, handle)
}

// ---------- stream parsers ----------

/// Accumulated state for one child's stdout, per [`StreamKind`].
struct StreamState {
    kind: StreamKind,
    text: String,
    session_id: Option<String>,
    last_activity: Option<Instant>,
}

impl StreamState {
    fn new(kind: StreamKind, session_id: Option<String>) -> Self {
        Self {
            kind,
            text: String::new(),
            session_id,
            last_activity: None,
        }
    }

    fn feed(&mut self, line: &str, activity: Option<&mpsc::Sender<String>>) {
        match self.kind {
            // No upstream throttle: every line is both content and status.
            StreamKind::PlainLines => {
                if line.trim().is_empty() {
                    return;
                }
                if !self.text.is_empty() {
                    self.text.push('\n');
                }
                self.text.push_str(line);
                if let Some(tx) = activity {
                    let _ = tx.send(line.to_string());
                }
            }
            StreamKind::ClaudeStreamJson | StreamKind::CodexJsonl => {
                if line.trim().is_empty() {
                    return;
                }
                let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
                    return; // JS: `catch { continue; }`
                };
                match self.kind {
                    StreamKind::ClaudeStreamJson => self.feed_claude(&ev, activity),
                    StreamKind::CodexJsonl => self.feed_codex(&ev, activity),
                    StreamKind::PlainLines => unreachable!(),
                }
            }
        }
    }

    /// First session id wins, except a `result` event, which overwrites.
    /// `assistant` REPLACES the text; `result` keeps whichever is longer.
    fn feed_claude(&mut self, ev: &serde_json::Value, activity: Option<&mpsc::Sender<String>>) {
        if self.session_id.is_none() {
            if let Some(sid) = ev.get("session_id").and_then(|v| v.as_str()) {
                self.session_id = Some(sid.to_string());
            }
        }
        match ev.get("type").and_then(|v| v.as_str()) {
            Some("assistant") => {
                let Some(content) = ev.pointer("/message/content").and_then(|c| c.as_array()) else {
                    return;
                };
                let t: String = content
                    .iter()
                    .filter(|c| c.get("type").and_then(|v| v.as_str()) == Some("text"))
                    .filter_map(|c| c.get("text").and_then(|v| v.as_str()))
                    .collect();
                if !t.is_empty() {
                    self.text = t;
                }
                let last_tool = content
                    .iter()
                    .rfind(|c| c.get("type").and_then(|v| v.as_str()) == Some("tool_use"));
                if let Some(tool) = last_tool {
                    self.emit(activity, crate::render::claude_activity(tool));
                }
            }
            Some("result") => {
                if let Some(sid) = ev.get("session_id").and_then(|v| v.as_str()) {
                    self.session_id = Some(sid.to_string());
                }
                if let Some(r) = ev.get("result").and_then(|v| v.as_str()) {
                    if r.chars().count() >= self.text.chars().count() {
                        self.text = r.to_string();
                    }
                }
            }
            _ => {}
        }
    }

    /// `thread.started` OVERWRITES the session; `agent_message` items APPEND
    /// with a blank line between them.
    fn feed_codex(&mut self, ev: &serde_json::Value, activity: Option<&mpsc::Sender<String>>) {
        let ty = ev.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        if ty == "thread.started" {
            if let Some(tid) = ev.get("thread_id").and_then(|v| v.as_str()) {
                self.session_id = Some(tid.to_string());
            }
        }
        if ty != "item.started" && ty != "item.completed" {
            return;
        }
        let item = ev.get("item");
        let is_message = item.and_then(|i| i.get("type")).and_then(|v| v.as_str()) == Some("agent_message");
        if ty == "item.completed" && is_message {
            let msg = item
                .and_then(|i| i.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !msg.is_empty() {
                if self.text.is_empty() {
                    self.text = msg.to_string();
                } else {
                    self.text = format!("{}\n\n{}", self.text, msg);
                }
            }
            return;
        }
        if let Some(item) = item {
            self.emit(activity, crate::render::codex_activity(item));
        }
    }

    /// Throttled activity emit (JS: `Date.now() - lastStatus > 800`). Empty
    /// lines are dropped without consuming the throttle window.
    fn emit(&mut self, activity: Option<&mpsc::Sender<String>>, line: Option<String>) {
        let Some(tx) = activity else { return };
        let Some(line) = line.filter(|l| !l.is_empty()) else {
            return;
        };
        let now = Instant::now();
        if self
            .last_activity
            .is_some_and(|t| now.duration_since(t) <= ACTIVITY_THROTTLE)
        {
            return;
        }
        self.last_activity = Some(now);
        let _ = tx.send(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use stackhour_core::registry::engine::{builtin_claude, builtin_codex};

    fn drain(kind: StreamKind, lines: &[&str]) -> (RunResult, Vec<String>) {
        let (tx, rx) = mpsc::channel();
        let mut state = StreamState::new(kind, None);
        for l in lines {
            state.feed(l, Some(&tx));
        }
        drop(tx);
        (
            RunResult {
                text: state.text,
                session_id: state.session_id,
                ..RunResult::default()
            },
            rx.iter().collect(),
        )
    }

    #[test]
    fn system_prompt_is_prefixed_for_fresh_codex_but_not_a_resume() {
        let def = builtin_codex();
        let mut fresh = RunRequest {
            prompt: "Do the work.".to_string(),
            system_prompt: Some("You are Claire.".to_string()),
            ..RunRequest::default()
        };
        apply_system_prompt(&def, &mut fresh);
        assert_eq!(
            fresh.prompt,
            "[System instructions]\nYou are Claire.\n\n[User message]\nDo the work."
        );
        assert_eq!(fresh.system_prompt, None);

        let mut resumed = RunRequest {
            prompt: "Continue.".to_string(),
            session_id: Some("thread-1".to_string()),
            system_prompt: Some("You are Claire.".to_string()),
            ..RunRequest::default()
        };
        apply_system_prompt(&def, &mut resumed);
        assert_eq!(resumed.prompt, "Continue.");
    }

    #[test]
    fn system_prompt_stays_structured_for_claude() {
        let def = builtin_claude();
        let mut req = RunRequest {
            prompt: "Do the work.".to_string(),
            system_prompt: Some("You are Claire.".to_string()),
            ..RunRequest::default()
        };
        apply_system_prompt(&def, &mut req);
        assert_eq!(req.prompt, "Do the work.");
        assert_eq!(req.system_prompt.as_deref(), Some("You are Claire."));
    }

    #[test]
    fn claude_keeps_the_first_session_id_but_result_overwrites_it() {
        let (r, _) = drain(
            StreamKind::ClaudeStreamJson,
            &[
                &json!({"type": "system", "session_id": "first"}).to_string(),
                &json!({"type": "system", "session_id": "second"}).to_string(),
                &json!({"type": "result", "session_id": "final", "result": "done"}).to_string(),
            ],
        );
        assert_eq!(r.session_id.as_deref(), Some("final"));
        assert_eq!(r.text, "done");
    }

    #[test]
    fn claude_assistant_replaces_text_and_result_only_wins_when_not_shorter() {
        let (r, _) = drain(
            StreamKind::ClaudeStreamJson,
            &[
                &json!({"type":"assistant","message":{"content":[{"type":"text","text":"short"}]}})
                    .to_string(),
                &json!({"type":"assistant","message":{"content":[{"type":"text","text":"a much longer answer"}]}}).to_string(),
                &json!({"type":"result","result":"tiny"}).to_string(),
            ],
        );
        assert_eq!(r.text, "a much longer answer");
    }

    #[test]
    fn codex_thread_started_overwrites_and_messages_append() {
        let (r, _) = drain(
            StreamKind::CodexJsonl,
            &[
                &json!({"type":"thread.started","thread_id":"t-1"}).to_string(),
                &json!({"type":"item.completed","item":{"type":"agent_message","text":"one"}}).to_string(),
                &json!({"type":"item.completed","item":{"type":"agent_message","text":"two"}}).to_string(),
            ],
        );
        assert_eq!(r.session_id.as_deref(), Some("t-1"));
        assert_eq!(r.text, "one\n\ntwo");
    }

    #[test]
    fn plain_lines_accumulate_and_stream_every_line() {
        let (r, streamed) = drain(StreamKind::PlainLines, &["one", "", "two"]);
        assert_eq!(r.text, "one\ntwo");
        assert_eq!(streamed, vec!["one", "two"]);
    }

    #[test]
    fn malformed_json_lines_are_skipped_not_fatal() {
        let (r, _) = drain(
            StreamKind::ClaudeStreamJson,
            &[
                "not json at all",
                &json!({"type":"result","result":"ok"}).to_string(),
            ],
        );
        assert_eq!(r.text, "ok");
    }
}
