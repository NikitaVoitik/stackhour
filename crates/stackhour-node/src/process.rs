//! Claude and Codex child-process supervision for the control-plane node.

use stackhour_core::engine::{ArgvVars, EngineDef, PromptDelivery, StreamKind};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const SYSTEM_PROMPT_TURN: &str = "[System instructions]\n{{system}}\n\n[User message]\n{{prompt}}";

#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    pub prompt: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub system_prompt: Option<String>,
    pub effort: Option<String>,
    pub cwd: Option<PathBuf>,
    pub live_status: bool,
    pub bin: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RunResult {
    pub text: String,
    pub session_id: Option<String>,
    pub code: Option<i32>,
    pub error: Option<String>,
    pub stderr: String,
}

/// A cloneable cancellation handle for one active child process.
#[derive(Debug, Clone, Default)]
pub struct RunningJob {
    pid: Arc<Mutex<Option<u32>>>,
}

impl RunningJob {
    pub fn terminate(&self) {
        let _ = self.terminate_if_running();
    }

    /// Terminate the child only while its process is still live.
    ///
    /// The PID slot is cleared immediately after `wait`, so callers can avoid
    /// recording an interruption for a process that has already completed.
    pub fn terminate_if_running(&self) -> bool {
        let pid = self.pid.lock().ok().and_then(|guard| *guard);
        let Some(pid) = pid else { return false };
        #[cfg(unix)]
        if let Ok(raw_pid) = i32::try_from(pid) {
            if let Some(pid) = rustix::process::Pid::from_raw(raw_pid) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
        }
        #[cfg(not(unix))]
        let _ = pid;
        true
    }
}

pub fn spawn_engine(
    def: &EngineDef,
    mut request: RunRequest,
) -> (RunningJob, std::thread::JoinHandle<RunResult>) {
    apply_system_prompt(def, &mut request);
    let argv = build_argv(def, &request);
    let binary = request.bin.clone().unwrap_or_else(|| def.bin.clone());
    let mut command = Command::new(binary);
    command
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }
    for (key, value) in &def.env {
        command.env(key, value);
    }

    let job = RunningJob::default();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = error.to_string();
            let session_id = request.session_id;
            let handle = std::thread::spawn(move || RunResult {
                error: Some(message),
                session_id,
                ..RunResult::default()
            });
            return (job, handle);
        }
    };
    if let Ok(mut slot) = job.pid.lock() {
        *slot = Some(child.id());
    }

    let stdin = child.stdin.take();
    if def.prompt_delivery == PromptDelivery::Stdin {
        let prompt = request.prompt.clone();
        std::thread::spawn(move || {
            if let Some(mut writer) = stdin {
                let _ = writer.write_all(prompt.as_bytes());
            }
        });
    } else {
        drop(stdin);
    }

    let stderr = child.stderr.take();
    let stderr_thread = std::thread::spawn(move || {
        let mut buffer = String::new();
        if let Some(mut reader) = stderr {
            let _ = reader.read_to_string(&mut buffer);
        }
        buffer
    });

    let stdout = child.stdout.take();
    let kind = def.kind;
    let initial_session = request.session_id;
    let pid_slot = Arc::clone(&job.pid);
    let handle = std::thread::spawn(move || {
        let mut state = StreamState::new(kind, initial_session);
        if let Some(output) = stdout {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                state.feed(&line);
            }
        }
        let code = child.wait().ok().and_then(|status| status.code());
        if let Ok(mut slot) = pid_slot.lock() {
            *slot = None;
        }
        RunResult {
            text: state.text,
            session_id: state.session_id,
            code,
            error: None,
            stderr: stderr_thread.join().unwrap_or_default(),
        }
    });

    (job, handle)
}

fn apply_system_prompt(def: &EngineDef, request: &mut RunRequest) {
    if def.system_prompt_args.is_some() {
        return;
    }
    if request.session_id.is_some() && def.kind != StreamKind::ClaudeStreamJson {
        return;
    }
    let Some(system) = request.system_prompt.take() else {
        return;
    };
    if system.trim().is_empty() {
        return;
    }
    request.prompt = SYSTEM_PROMPT_TURN
        .replace("{{system}}", &system)
        .replace("{{prompt}}", &request.prompt);
}

fn build_argv(def: &EngineDef, request: &RunRequest) -> Vec<String> {
    let mut argv = def.assemble_argv(&ArgvVars {
        session_id: request.session_id.as_deref(),
        model: request.model.as_deref(),
        permission_mode: request.permission_mode.as_deref(),
        system_prompt: request.system_prompt.as_deref(),
        effort: request.effort.as_deref(),
        allow_tools: &[],
        deny_tools: &[],
        live_status: request.live_status,
    });
    if def.prompt_delivery == PromptDelivery::LastArg {
        argv.push(request.prompt.clone());
    }
    argv
}

struct StreamState {
    kind: StreamKind,
    text: String,
    session_id: Option<String>,
}

impl StreamState {
    fn new(kind: StreamKind, session_id: Option<String>) -> Self {
        Self {
            kind,
            text: String::new(),
            session_id,
        }
    }

    fn feed(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        match self.kind {
            StreamKind::PlainLines => {
                if !self.text.is_empty() {
                    self.text.push('\n');
                }
                self.text.push_str(line);
            }
            StreamKind::ClaudeStreamJson | StreamKind::CodexJsonl => {
                let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                    return;
                };
                if self.kind == StreamKind::ClaudeStreamJson {
                    self.feed_claude(&event);
                } else {
                    self.feed_codex(&event);
                }
            }
        }
    }

    fn feed_claude(&mut self, event: &serde_json::Value) {
        if self.session_id.is_none() {
            if let Some(session_id) = event.get("session_id").and_then(serde_json::Value::as_str) {
                self.session_id = Some(session_id.to_string());
            }
        }
        match event.get("type").and_then(serde_json::Value::as_str) {
            Some("assistant") => {
                let Some(content) = event
                    .pointer("/message/content")
                    .and_then(serde_json::Value::as_array)
                else {
                    return;
                };
                let text = content
                    .iter()
                    .filter(|item| item.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                    .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
                    .collect::<String>();
                if !text.is_empty() {
                    self.text = text;
                }
            }
            Some("result") => {
                if let Some(session_id) = event.get("session_id").and_then(serde_json::Value::as_str) {
                    self.session_id = Some(session_id.to_string());
                }
                if let Some(result) = event.get("result").and_then(serde_json::Value::as_str) {
                    if result.chars().count() >= self.text.chars().count() {
                        self.text = result.to_string();
                    }
                }
            }
            _ => {}
        }
    }

    fn feed_codex(&mut self, event: &serde_json::Value) {
        let event_type = event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if event_type == "thread.started" {
            if let Some(thread_id) = event.get("thread_id").and_then(serde_json::Value::as_str) {
                self.session_id = Some(thread_id.to_string());
            }
            return;
        }
        if event_type != "item.completed" {
            return;
        }
        let item = event.get("item");
        if item
            .and_then(|value| value.get("type"))
            .and_then(serde_json::Value::as_str)
            != Some("agent_message")
        {
            return;
        }
        let message = item
            .and_then(|value| value.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if message.is_empty() {
            return;
        }
        if self.text.is_empty() {
            self.text = message.to_string();
        } else {
            self.text.push_str("\n\n");
            self.text.push_str(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stackhour_core::engine::{builtin_claude, builtin_codex};

    #[test]
    fn codex_system_prompt_is_added_only_to_a_fresh_prompt() {
        let definition = builtin_codex();
        let mut fresh = RunRequest {
            prompt: "work".to_string(),
            system_prompt: Some("rules".to_string()),
            ..RunRequest::default()
        };
        apply_system_prompt(&definition, &mut fresh);
        assert!(fresh.prompt.contains("rules"));

        let mut resumed = RunRequest {
            prompt: "work".to_string(),
            session_id: Some("thread".to_string()),
            system_prompt: Some("rules".to_string()),
            ..RunRequest::default()
        };
        apply_system_prompt(&definition, &mut resumed);
        assert_eq!(resumed.prompt, "work");
    }

    #[test]
    fn claude_and_codex_streams_capture_answers_and_sessions() {
        let mut claude = StreamState::new(builtin_claude().kind, None);
        claude.feed(
            r#"{"type":"assistant","session_id":"s1","message":{"content":[{"type":"text","text":"answer"}]}}"#,
        );
        assert_eq!(claude.text, "answer");
        assert_eq!(claude.session_id.as_deref(), Some("s1"));

        let mut codex = StreamState::new(builtin_codex().kind, None);
        codex.feed(r#"{"type":"thread.started","thread_id":"t1"}"#);
        codex.feed(r#"{"type":"item.completed","item":{"type":"agent_message","text":"answer"}}"#);
        assert_eq!(codex.text, "answer");
        assert_eq!(codex.session_id.as_deref(), Some("t1"));
    }

    #[cfg(unix)]
    #[test]
    fn completed_process_cannot_be_recorded_as_newly_interrupted() {
        let (job, handle) = spawn_engine(
            &builtin_codex(),
            RunRequest {
                prompt: "done".to_string(),
                bin: Some("/bin/true".to_string()),
                ..RunRequest::default()
            },
        );
        let _ = handle.join().unwrap();
        assert!(!job.terminate_if_running());
    }
}
