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

use stackhour_core::registry::EngineDef;
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// One engine invocation request.
#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    pub prompt: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    /// Composed system prompt (souls + skills), when an agent is active.
    pub system_prompt: Option<String>,
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
    /// Child exit code (None = killed by signal).
    pub code: Option<i32>,
    pub error: Option<String>,
    pub stderr: String,
}

/// A live child handle, used by /stop.
#[derive(Debug, Clone)]
pub struct RunningJob {
    #[allow(dead_code)] // scaffold: read only by the todo!() bodies
    child: Arc<Mutex<Option<Child>>>,
}

impl RunningJob {
    /// SIGTERM the child (best-effort, idempotent).
    pub fn terminate(&self) {
        todo!()
    }
}

/// Assemble the final argv for a request (unit-tested standalone — the argv
/// byte-parity surface).
pub fn build_argv(def: &EngineDef, req: &RunRequest) -> Vec<String> {
    let _ = (def, req);
    todo!()
}

/// Run the engine to completion, streaming activity lines to `activity`.
pub fn run_engine(def: &EngineDef, req: RunRequest, activity: Option<mpsc::Sender<String>>) -> RunResult {
    let _ = (def, req, activity);
    todo!()
}

/// Like [`run_engine`], plus the resume-retry rule: exit code != 0 with a
/// session id and no text -> ONE fresh (session-less) rerun, with the exact
/// log line.
pub fn run_with_resume_retry(
    def: &EngineDef,
    req: RunRequest,
    activity: Option<mpsc::Sender<String>>,
) -> RunResult {
    let _ = (def, req, activity);
    todo!()
}

/// Spawn variant returning the terminate handle alongside the join logic
/// (used by the coordinator's local lane).
pub fn spawn_engine(
    def: &EngineDef,
    req: RunRequest,
    activity: Option<mpsc::Sender<String>>,
) -> (RunningJob, std::thread::JoinHandle<RunResult>) {
    let _ = (def, req, activity);
    todo!()
}
