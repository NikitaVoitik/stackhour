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
use std::collections::HashMap;
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
/// Activity detail truncation width (parity with `.slice(0, 80)`).
const ACTIVITY_DETAIL_MAX: usize = 80;

/// One engine invocation request.
#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    pub prompt: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    /// Composed system prompt (souls + skills), when an agent is active.
    pub system_prompt: Option<String>,
    /// Reasoning effort, spliced through the engine's declared `effort_args`.
    /// Engines that declare none ignore it (see `souls::agent_effort`).
    pub effort: Option<String>,
    pub cwd: Option<PathBuf>,
    /// true on the coordinator's local lane (enables partial_messages_flag).
    pub live_status: bool,
    /// Target extraPath, prepended to the child's PATH.
    pub extra_path: Option<String>,
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
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        #[cfg(not(unix))]
        let _ = pid;
    }
}

/// The exact log line emitted when the resume-retry rule fires (parity with
/// coordinator.mjs / worker.mjs).
pub fn resume_retry_log_line(engine: &str, code: Option<i32>) -> String {
    format!(
        "{engine} resume failed ({}); retry fresh",
        code.unwrap_or_default()
    )
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
pub fn run_engine(
    def: &EngineDef,
    req: RunRequest,
    activity: Option<mpsc::Sender<String>>,
) -> RunResult {
    let (_job, handle) = spawn_engine(def, req, activity);
    handle.join().unwrap_or_else(|_| RunResult {
        error: Some("engine reader thread panicked".to_string()),
        ..RunResult::default()
    })
}

/// Like [`run_engine`], plus the resume-retry rule: exit code != 0 with a
/// session id and no text -> ONE fresh (session-less) rerun, with the exact
/// log line.
pub fn run_with_resume_retry(
    def: &EngineDef,
    req: RunRequest,
    activity: Option<mpsc::Sender<String>>,
) -> RunResult {
    let had_session = req.session_id.is_some();
    let mut fresh = req.clone();
    let first = run_engine(def, req, activity.clone());

    let failed = first.code.is_some_and(|c| c != 0);
    if !(failed && had_session && first.text.is_empty()) {
        return first;
    }
    if let Some(tx) = activity.as_ref() {
        let _ = tx.send(resume_retry_log_line(&def.name, first.code));
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
    req: RunRequest,
    activity: Option<mpsc::Sender<String>>,
) -> (RunningJob, std::thread::JoinHandle<RunResult>) {
    let job = RunningJob::default();
    let argv = build_argv(def, &req);

    let mut cmd = Command::new(&def.bin);
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
        cmd.env("PATH", prepend_path(extra));
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Node resolves `child.on('error')` with the message and no code.
            let msg = e.to_string();
            let handle = std::thread::spawn(move || RunResult {
                error: Some(msg),
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

fn prepend_path(extra: &str) -> String {
    match std::env::var("PATH") {
        Ok(existing) if !existing.is_empty() => format!("{extra}:{existing}"),
        _ => extra.to_string(),
    }
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
                let Some(content) = ev.pointer("/message/content").and_then(|c| c.as_array())
                else {
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
                    self.emit(activity, claude_activity_line(tool));
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
        let is_message = item.and_then(|i| i.get("type")).and_then(|v| v.as_str())
            == Some("agent_message");
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
            self.emit(activity, codex_activity_line(item));
        }
    }

    /// Throttled activity emit (JS: `Date.now() - lastStatus > 800`). Empty
    /// lines are dropped without consuming the throttle window.
    fn emit(&mut self, activity: Option<&mpsc::Sender<String>>, line: String) {
        let Some(tx) = activity else { return };
        if line.is_empty() {
            return;
        }
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

fn truncate_detail(detail: &str) -> String {
    let collapsed = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(ACTIVITY_DETAIL_MAX).collect()
}

/// `⚙️ <ToolName>[: <detail>]` — parity with coordinator.mjs `activityLine`.
fn claude_activity_line(tool: &serde_json::Value) -> String {
    let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or_default();
    if name.is_empty() {
        return String::new();
    }
    let input = tool.get("input");
    let field = |k: &str| input.and_then(|i| i.get(k)).and_then(|v| v.as_str());
    let detail = if name == "Bash" {
        field("command").unwrap_or_default()
    } else {
        field("file_path")
            .or_else(|| field("pattern"))
            .or_else(|| field("url"))
            .or_else(|| field("command"))
            .unwrap_or_default()
    };
    let detail = truncate_detail(detail);
    if detail.is_empty() {
        format!("⚙️ {name}")
    } else {
        format!("⚙️ {name}: {detail}")
    }
}

/// `⚙️ <Friendly>[: <detail>]` — parity with coordinator.mjs `codexActivity`.
fn codex_activity_line(item: &serde_json::Value) -> String {
    let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or_default();
    if ty.is_empty() || ty == "agent_message" {
        return String::new();
    }
    let field = |k: &str| item.get(k).and_then(|v| v.as_str());
    let detail = field("command")
        .or_else(|| field("query"))
        .or_else(|| field("name"))
        .or_else(|| field("path"))
        .unwrap_or_default();
    let detail = truncate_detail(detail);

    let names: HashMap<&str, &str> = HashMap::from([
        ("command_execution", "Command"),
        ("file_change", "File change"),
        ("mcp_tool_call", "Tool"),
        ("web_search", "Web search"),
        ("todo_list", "Plan"),
    ]);
    let label = names.get(ty).copied().unwrap_or(ty);
    if detail.is_empty() {
        format!("⚙️ {label}")
    } else {
        format!("⚙️ {label}: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                &json!({"type":"item.completed","item":{"type":"agent_message","text":"one"}})
                    .to_string(),
                &json!({"type":"item.completed","item":{"type":"agent_message","text":"two"}})
                    .to_string(),
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

    #[test]
    fn activity_lines_match_the_js_formatting() {
        assert_eq!(
            claude_activity_line(&json!({"name":"Bash","input":{"command":"ls  -l\n"}})),
            "⚙️ Bash: ls -l"
        );
        assert_eq!(
            claude_activity_line(&json!({"name":"Read","input":{"file_path":"/tmp/x"}})),
            "⚙️ Read: /tmp/x"
        );
        assert_eq!(
            codex_activity_line(&json!({"type":"command_execution","command":"echo hi"})),
            "⚙️ Command: echo hi"
        );
        assert_eq!(
            codex_activity_line(&json!({"type":"agent_message","text":"hi"})),
            ""
        );
    }
}
