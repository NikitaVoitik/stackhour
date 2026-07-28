//! The engine boundary, production CLI engine, and test stub engine.
//!
//! An [`Engine`] turns dispatched [`NodeWork`] into a stream of durable node
//! events. The supervisor owns the socket; the engine only ever touches an
//! [`EngineOutbox`], never the WebSocket, so the same trait can later be
//! implemented by the real ACP SDK adapter without changing the transport.
//!
//! [`StubEngine`] is the transport-test fake: no subprocess, no ACP — just a
//! deterministic, interruptible sequence of events that proves the durable
//! connect / dispatch / ack / reconnect / resume loop end to end.

use serde_json::json;
use stackhour_bridge::engines::{spawn_engine, RunRequest, RunningJob};
use stackhour_domain::entities::AccessPolicy;
use stackhour_domain::event::EventDraft;
use stackhour_domain::ids::{CommandId, EventId, NodeId, RunId, TaskId};
use stackhour_domain::protocol::{NodeToHub, NodeWork};
use stackhour_domain::EventKind;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// A frame queued for the single writer task. Protocol messages are the norm;
/// `Pong` lets the receive loop answer a ws-level ping without sharing the sink.
pub enum Outgoing {
    /// One JSON-serialized protocol message, sent as a ws text frame.
    Protocol(NodeToHub),
    /// A ws-level pong echoing a received ping's payload.
    Pong(Vec<u8>),
}

/// How long the stub pauses between streamed chunks. Small so tests are fast
/// and interrupts land promptly; the pause is always cancellable.
const STUB_STEP: Duration = Duration::from_millis(10);

/// The handle an [`Engine`] uses to emit node-to-hub traffic. Cloneable and
/// cheap; every clone feeds the same single writer task.
///
/// The node does not yet track a resume cursor: the hub returns no per-event
/// sequence to the node in Phase 1, so there is no hub sequence the node could
/// honestly claim to have processed. Until the hub echoes assigned sequences
/// back over the link, the node reconnects with `resume_after_sequence: None`
/// (an honest "fresh start") rather than advertising a private emit count as if
/// it were a hub sequence. Wiring a real cursor is the job of the slice that
/// consumes those echoed sequences.
#[derive(Clone)]
pub struct EngineOutbox {
    tx: mpsc::Sender<Outgoing>,
    node_id: NodeId,
}

impl EngineOutbox {
    pub(crate) fn new(tx: mpsc::Sender<Outgoing>, node_id: NodeId) -> Self {
        EngineOutbox { tx, node_id }
    }

    /// Emit a fully-formed [`EventDraft`] as a fresh-id `NodeEvent`. Returns
    /// `false` if the link is gone (the writer half is closed), which a
    /// streaming engine should treat as "stop".
    pub async fn emit_draft(&self, draft: EventDraft) -> bool {
        let msg = NodeToHub::NodeEvent {
            event_id: EventId::new(),
            draft,
        };
        self.tx.send(Outgoing::Protocol(msg)).await.is_ok()
    }

    /// Emit a node event for `kind` on `task_id` (optionally scoped to a run)
    /// with the given payload. Convenience over [`EngineOutbox::emit_draft`].
    pub async fn emit(
        &self,
        kind: EventKind,
        task_id: TaskId,
        run_id: Option<RunId>,
        payload: serde_json::Value,
    ) -> bool {
        let mut draft = EventDraft::new(kind, task_id, self.node_id.clone());
        if let Some(run_id) = run_id {
            draft = draft.with_run(run_id);
        }
        self.emit_draft(draft.with_payload(payload)).await
    }

    /// Acknowledge a dispatched command. Returns `false` if the link is gone.
    pub async fn ack(&self, command_id: CommandId) -> bool {
        self.tx
            .send(Outgoing::Protocol(NodeToHub::CommandAck { command_id }))
            .await
            .is_ok()
    }
}

/// A supervised engine: the thing that actually services dispatched work.
///
/// Methods are synchronous and return immediately — an implementation that
/// streams over time spawns its own task so the supervisor's receive loop stays
/// responsive (able to deliver an interrupt while a run is mid-stream). The ACP
/// SDK adapter will implement this same trait.
pub trait Engine: Send + Sync + 'static {
    /// Handle one dispatched command (a `HubToNode::DispatchCommand`):
    /// acknowledge it and perform (or begin streaming) the work.
    fn dispatch(&self, command_id: CommandId, work: NodeWork, out: EngineOutbox);

    /// Handle a `CancelCommand` for a previously dispatched command — withdraw
    /// or interrupt whatever run that dispatch started.
    fn cancel(&self, command_id: CommandId, out: EngineOutbox);
}

/// Local process settings for the Claude and Codex adapters.
#[derive(Clone, Debug, Default)]
pub struct CliEngineConfig {
    pub claude_bin: Option<String>,
    pub codex_bin: Option<String>,
    pub default_workspace: Option<PathBuf>,
}

#[derive(Default)]
struct CliState {
    runs: HashMap<RunId, RunSpec>,
    sessions: HashMap<RunId, String>,
    jobs: HashMap<RunId, RunningJob>,
    command_runs: HashMap<CommandId, RunId>,
    seen: HashSet<CommandId>,
}

#[derive(Clone)]
struct RunSpec {
    task_id: TaskId,
    engine: String,
    access_policy: AccessPolicy,
    workspace: Option<PathBuf>,
}

/// A production engine that launches the installed Claude or Codex CLI.
#[derive(Clone, Default)]
pub struct CliEngine {
    config: CliEngineConfig,
    state: Arc<Mutex<CliState>>,
}

impl CliEngine {
    pub fn new(config: CliEngineConfig) -> Self {
        CliEngine {
            config,
            state: Arc::new(Mutex::new(CliState::default())),
        }
    }

    fn stop_run(&self, run_id: RunId) {
        let job = self.state.lock().unwrap().jobs.get(&run_id).cloned();
        if let Some(job) = job {
            job.terminate();
        }
    }
}

impl Engine for CliEngine {
    fn dispatch(&self, command_id: CommandId, work: NodeWork, out: EngineOutbox) {
        if !self.state.lock().unwrap().seen.insert(command_id) {
            tokio::spawn(async move {
                out.ack(command_id).await;
            });
            return;
        }

        match work {
            NodeWork::StartRun {
                run_id,
                task_id,
                engine,
                access_policy,
                workspace_path,
            } => {
                let workspace = workspace_path
                    .map(PathBuf::from)
                    .or_else(|| self.config.default_workspace.clone());
                self.state.lock().unwrap().runs.insert(
                    run_id,
                    RunSpec {
                        task_id,
                        engine,
                        access_policy,
                        workspace,
                    },
                );
                tokio::spawn(async move {
                    out.ack(command_id).await;
                });
            }
            NodeWork::SendPrompt {
                task_id,
                run_id,
                text,
                ..
            } => {
                let spec = self.state.lock().unwrap().runs.get(&run_id).cloned();
                let Some(spec) = spec else {
                    tokio::spawn(async move {
                        out.ack(command_id).await;
                        out.emit(
                            EventKind::RunFailed,
                            task_id,
                            Some(run_id),
                            json!({"error": "run is not initialized on this node"}),
                        )
                        .await;
                    });
                    return;
                };

                let def = match spec.engine.as_str() {
                    "claude" => stackhour_core::registry::engine::builtin_claude(),
                    "codex" => stackhour_core::registry::engine::builtin_codex(),
                    other => {
                        let error = format!("unsupported engine: {other}");
                        tokio::spawn(async move {
                            out.ack(command_id).await;
                            out.emit(
                                EventKind::RunFailed,
                                task_id,
                                Some(run_id),
                                json!({"error": error}),
                            )
                            .await;
                        });
                        return;
                    }
                };
                let session_id = self.state.lock().unwrap().sessions.get(&run_id).cloned();
                let permission_mode = match spec.access_policy {
                    AccessPolicy::FullAccess => "bypassPermissions",
                    AccessPolicy::Supervised | AccessPolicy::Automatic => "default",
                };
                let bin = match spec.engine.as_str() {
                    "claude" => self.config.claude_bin.clone(),
                    "codex" => self.config.codex_bin.clone(),
                    _ => None,
                };
                let req = RunRequest {
                    prompt: text,
                    session_id,
                    permission_mode: Some(permission_mode.to_string()),
                    cwd: spec.workspace,
                    live_status: true,
                    bin,
                    ..RunRequest::default()
                };
                let (job, handle) = spawn_engine(&def, req, None);
                {
                    let mut state = self.state.lock().unwrap();
                    state.jobs.insert(run_id, job);
                    state.command_runs.insert(command_id, run_id);
                }
                let engine = self.clone();
                tokio::spawn(async move {
                    out.ack(command_id).await;
                    let result = tokio::task::spawn_blocking(move || handle.join())
                        .await
                        .ok()
                        .and_then(std::result::Result::ok);
                    {
                        let mut state = engine.state.lock().unwrap();
                        state.jobs.remove(&run_id);
                        state.command_runs.remove(&command_id);
                        if let Some(session) = result.as_ref().and_then(|r| r.session_id.clone()) {
                            state.sessions.insert(run_id, session);
                        }
                    }
                    match result {
                        Some(result) if result.code == Some(0) && result.error.is_none() => {
                            let mut draft = EventDraft::new(
                                EventKind::MessageAssistantCompleted,
                                spec.task_id,
                                out.node_id.clone(),
                            )
                            .with_run(run_id)
                            .with_payload(json!({"text": result.text}));
                            draft.provider_session_id = result.session_id;
                            out.emit_draft(draft).await;
                            out.emit(EventKind::RunCompleted, spec.task_id, Some(run_id), json!({}))
                                .await;
                        }
                        Some(result) => {
                            out.emit(
                                EventKind::RunFailed,
                                spec.task_id,
                                Some(run_id),
                                json!({
                                    "error": result.error.unwrap_or_else(|| {
                                        let stderr = result.stderr.trim();
                                        if stderr.is_empty() {
                                            format!("engine exited with code {:?}", result.code)
                                        } else {
                                            stderr.to_string()
                                        }
                                    }),
                                    "code": result.code,
                                }),
                            )
                            .await;
                        }
                        None => {
                            out.emit(
                                EventKind::RunFailed,
                                spec.task_id,
                                Some(run_id),
                                json!({"error": "engine supervisor failed"}),
                            )
                            .await;
                        }
                    }
                });
            }
            NodeWork::InterruptRun { run_id } => {
                self.stop_run(run_id);
                tokio::spawn(async move {
                    out.ack(command_id).await;
                });
            }
        }
    }

    fn cancel(&self, command_id: CommandId, _out: EngineOutbox) {
        let run_id = self.state.lock().unwrap().command_runs.get(&command_id).copied();
        if let Some(run_id) = run_id {
            self.stop_run(run_id);
        }
    }
}

/// Per-engine mutable bookkeeping: the cancel trigger for each active run, the
/// dispatch-command -> run mapping so a `CancelCommand` (which names a command,
/// not a run) can find the run to interrupt, and the set of command ids already
/// dispatched so a redelivery is idempotent.
#[derive(Default)]
struct StubState {
    runs: HashMap<RunId, watch::Sender<bool>>,
    command_runs: HashMap<CommandId, RunId>,
    /// Every dispatch command id ever seen. A redelivered `DispatchCommand`
    /// (same command id) must yield exactly one run timeline and one durable
    /// effect, so a second sighting only re-sends the ack. Retained for the
    /// node's lifetime (retiring a run does not forget its command id).
    seen: HashSet<CommandId>,
}

/// A fake in-process engine with no real subprocess. It emits the Phase-1 event
/// vocabulary on a deterministic, interruptible schedule so the durable loop is
/// provable before ACP is integrated.
#[derive(Clone, Default)]
pub struct StubEngine {
    state: Arc<Mutex<StubState>>,
}

impl StubEngine {
    /// A fresh stub engine with no active runs.
    pub fn new() -> Self {
        StubEngine::default()
    }

    /// Register a cancel trigger for `run_id` and return the observer the
    /// streaming task watches. Replaces any prior trigger for the same run.
    fn arm(&self, run_id: RunId) -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(false);
        self.state.lock().unwrap().runs.insert(run_id, tx);
        rx
    }

    /// Fire the cancel trigger for `run_id`, if it is still active.
    fn trip(&self, run_id: RunId) {
        if let Some(tx) = self.state.lock().unwrap().runs.get(&run_id) {
            let _ = tx.send(true);
        }
    }

    /// Forget a finished run and any dispatch that pointed at it.
    fn retire(&self, run_id: RunId, command_id: CommandId) {
        let mut state = self.state.lock().unwrap();
        state.runs.remove(&run_id);
        state.command_runs.remove(&command_id);
    }
}

impl Engine for StubEngine {
    fn dispatch(&self, command_id: CommandId, work: NodeWork, out: EngineOutbox) {
        // Idempotency: replaying the same command id yields exactly one effect.
        // On a redelivered dispatch (a command id already seen — e.g. the hub
        // missed the ack on a drop and re-sent), re-send only the CommandAck,
        // which the hub de-dups by command id, and do not arm a cancel channel,
        // insert a run mapping, or spawn a second stream. Arm/insert/spawn only
        // on first sight so a redelivery cannot mint a duplicate run timeline.
        if !self.state.lock().unwrap().seen.insert(command_id) {
            tokio::spawn(async move {
                out.ack(command_id).await;
            });
            return;
        }
        match work {
            NodeWork::StartRun { run_id, task_id, .. } => {
                let cancel = self.arm(run_id);
                self.state.lock().unwrap().command_runs.insert(command_id, run_id);
                let engine = self.clone();
                tokio::spawn(async move {
                    stream_run(engine, command_id, run_id, task_id, out, cancel).await;
                });
            }
            NodeWork::SendPrompt { task_id, run_id, .. } => {
                let cancel = self.arm(run_id);
                self.state.lock().unwrap().command_runs.insert(command_id, run_id);
                let engine = self.clone();
                tokio::spawn(async move {
                    stream_prompt(engine, command_id, run_id, task_id, out, cancel).await;
                });
            }
            NodeWork::InterruptRun { run_id } => {
                // Acknowledge the dispatch, then trip the target run's stream,
                // which emits run.interrupted and stops itself.
                let engine = self.clone();
                tokio::spawn(async move {
                    out.ack(command_id).await;
                    engine.trip(run_id);
                });
            }
        }
    }

    fn cancel(&self, command_id: CommandId, _out: EngineOutbox) {
        // A CancelCommand names the dispatch; map it to the run and interrupt.
        let run_id = self.state.lock().unwrap().command_runs.get(&command_id).copied();
        if let Some(run_id) = run_id {
            self.trip(run_id);
        }
    }
}

/// Return `true` immediately if already cancelled, otherwise sleep for `step`
/// and return `true` only if a cancel arrives first. The stub's only await
/// points, so an interrupt is honoured within one step.
async fn interruptible_pause(cancel: &mut watch::Receiver<bool>, step: Duration) -> bool {
    if *cancel.borrow() {
        return true;
    }
    tokio::select! {
        () = tokio::time::sleep(step) => false,
        // The only transition is false -> true; a change (or closed sender)
        // means "interrupt".
        _ = cancel.changed() => true,
    }
}

/// StartRun: run.started -> deltas -> assistant.completed -> run.completed,
/// or run.interrupted if cancelled mid-stream.
async fn stream_run(
    engine: StubEngine,
    command_id: CommandId,
    run_id: RunId,
    task_id: TaskId,
    out: EngineOutbox,
    mut cancel: watch::Receiver<bool>,
) {
    out.ack(command_id).await;

    if !out
        .emit(EventKind::RunStarted, task_id, Some(run_id), json!({}))
        .await
    {
        engine.retire(run_id, command_id);
        return;
    }

    let chunks = ["Working on it", "…done."];
    let mut assembled = String::new();
    for chunk in chunks {
        if interruptible_pause(&mut cancel, STUB_STEP).await {
            emit_interrupted(&out, task_id, run_id).await;
            engine.retire(run_id, command_id);
            return;
        }
        assembled.push_str(chunk);
        if !out
            .emit(
                EventKind::MessageAssistantDelta,
                task_id,
                Some(run_id),
                json!({ "text": chunk }),
            )
            .await
        {
            engine.retire(run_id, command_id);
            return;
        }
    }

    if interruptible_pause(&mut cancel, STUB_STEP).await {
        emit_interrupted(&out, task_id, run_id).await;
        engine.retire(run_id, command_id);
        return;
    }
    out.emit(
        EventKind::MessageAssistantCompleted,
        task_id,
        Some(run_id),
        json!({ "text": assembled }),
    )
    .await;
    out.emit(EventKind::RunCompleted, task_id, Some(run_id), json!({}))
        .await;
    engine.retire(run_id, command_id);
}

/// SendPrompt: assistant deltas -> assistant.completed for an active run, or
/// run.interrupted if cancelled mid-stream.
async fn stream_prompt(
    engine: StubEngine,
    command_id: CommandId,
    run_id: RunId,
    task_id: TaskId,
    out: EngineOutbox,
    mut cancel: watch::Receiver<bool>,
) {
    out.ack(command_id).await;

    let chunks = ["Sure", " — here goes."];
    let mut assembled = String::new();
    for chunk in chunks {
        if interruptible_pause(&mut cancel, STUB_STEP).await {
            emit_interrupted(&out, task_id, run_id).await;
            engine.retire(run_id, command_id);
            return;
        }
        assembled.push_str(chunk);
        if !out
            .emit(
                EventKind::MessageAssistantDelta,
                task_id,
                Some(run_id),
                json!({ "text": chunk }),
            )
            .await
        {
            engine.retire(run_id, command_id);
            return;
        }
    }

    if interruptible_pause(&mut cancel, STUB_STEP).await {
        emit_interrupted(&out, task_id, run_id).await;
        engine.retire(run_id, command_id);
        return;
    }
    out.emit(
        EventKind::MessageAssistantCompleted,
        task_id,
        Some(run_id),
        json!({ "text": assembled }),
    )
    .await;
    engine.retire(run_id, command_id);
}

/// Emit the terminal `run.interrupted` event for a cancelled stream.
async fn emit_interrupted(out: &EngineOutbox, task_id: TaskId, run_id: RunId) {
    out.emit(EventKind::RunInterrupted, task_id, Some(run_id), json!({}))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use stackhour_domain::entities::AccessPolicy;

    /// A redelivered `DispatchCommand` (same `command_id` — e.g. the hub missed
    /// the first ack and re-sent) must be acknowledged again, yet spawn no second
    /// run stream: exactly one `run.started` timeline, two acks.
    #[tokio::test]
    async fn redelivered_dispatch_acks_again_but_spawns_one_run() {
        let (tx, mut rx) = mpsc::channel::<Outgoing>(64);
        let out = EngineOutbox::new(tx, NodeId::from("laptop"));
        let engine = StubEngine::new();

        let command_id = CommandId::new();
        let work = NodeWork::StartRun {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            engine: "stub".to_string(),
            access_policy: AccessPolicy::Supervised,
            workspace_path: None,
        };

        // Dispatch the identical command twice; drop our outbox so the channel
        // closes once both spawned tasks finish and `recv` can drain to `None`.
        engine.dispatch(command_id, work.clone(), out.clone());
        engine.dispatch(command_id, work, out.clone());
        drop(out);

        let mut acks = 0;
        let mut run_started = 0;
        while let Some(msg) = rx.recv().await {
            match msg {
                Outgoing::Protocol(NodeToHub::CommandAck { .. }) => acks += 1,
                Outgoing::Protocol(NodeToHub::NodeEvent { draft, .. })
                    if draft.kind == EventKind::RunStarted =>
                {
                    run_started += 1;
                }
                _ => {}
            }
        }

        assert_eq!(acks, 2, "a redelivered dispatch must still be acked");
        assert_eq!(
            run_started, 1,
            "a redelivered dispatch must not spawn a second run"
        );
    }

    #[cfg(unix)]
    fn executable_script(body: &str) -> (tempfile::TempDir, String) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fake-engine");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        (dir, path.to_string_lossy().to_string())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_engine_launches_claude_and_emits_the_final_answer() {
        let (_dir, bin) = executable_script(
            "cat >/dev/null\necho '{\"type\":\"result\",\"session_id\":\"sess-1\",\"result\":\"real answer\"}'",
        );
        let engine = CliEngine::new(CliEngineConfig {
            claude_bin: Some(bin),
            ..CliEngineConfig::default()
        });
        let (tx, mut rx) = mpsc::channel::<Outgoing>(64);
        let out = EngineOutbox::new(tx, NodeId::from("local"));
        let task_id = TaskId::new();
        let run_id = RunId::new();
        engine.dispatch(
            CommandId::new(),
            NodeWork::StartRun {
                run_id,
                task_id,
                engine: "claude".to_string(),
                access_policy: AccessPolicy::Automatic,
                workspace_path: None,
            },
            out.clone(),
        );
        engine.dispatch(
            CommandId::new(),
            NodeWork::SendPrompt {
                task_id,
                run_id,
                text: "do work".to_string(),
                client_message_id: "m1".to_string(),
            },
            out,
        );

        let mut answer = None;
        let mut completed = false;
        for _ in 0..8 {
            let message = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let Outgoing::Protocol(NodeToHub::NodeEvent { draft, .. }) = message {
                if draft.kind == EventKind::MessageAssistantCompleted {
                    answer = draft
                        .payload
                        .get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    assert_eq!(draft.provider_session_id.as_deref(), Some("sess-1"));
                }
                if draft.kind == EventKind::RunCompleted {
                    completed = true;
                    break;
                }
            }
        }
        assert_eq!(answer.as_deref(), Some("real answer"));
        assert!(completed);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_engine_launches_codex_and_parses_jsonl() {
        let (_dir, bin) = executable_script(
            "cat >/dev/null\necho '{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}'\necho '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"codex answer\"}}'",
        );
        let engine = CliEngine::new(CliEngineConfig {
            codex_bin: Some(bin),
            ..CliEngineConfig::default()
        });
        let (tx, mut rx) = mpsc::channel::<Outgoing>(64);
        let out = EngineOutbox::new(tx, NodeId::from("local"));
        let task_id = TaskId::new();
        let run_id = RunId::new();
        engine.dispatch(
            CommandId::new(),
            NodeWork::StartRun {
                run_id,
                task_id,
                engine: "codex".to_string(),
                access_policy: AccessPolicy::Automatic,
                workspace_path: None,
            },
            out.clone(),
        );
        engine.dispatch(
            CommandId::new(),
            NodeWork::SendPrompt {
                task_id,
                run_id,
                text: "do work".to_string(),
                client_message_id: "m1".to_string(),
            },
            out,
        );
        let mut found = false;
        for _ in 0..8 {
            let message = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let Outgoing::Protocol(NodeToHub::NodeEvent { draft, .. }) = message {
                if draft.kind == EventKind::MessageAssistantCompleted {
                    assert_eq!(draft.payload["text"], "codex answer");
                    assert_eq!(draft.provider_session_id.as_deref(), Some("thread-1"));
                    found = true;
                    break;
                }
            }
        }
        assert!(found);
    }

    #[tokio::test]
    async fn cli_engine_rejects_an_unknown_engine_without_a_process() {
        let engine = CliEngine::new(CliEngineConfig::default());
        let (tx, mut rx) = mpsc::channel::<Outgoing>(16);
        let out = EngineOutbox::new(tx, NodeId::from("local"));
        let task_id = TaskId::new();
        let run_id = RunId::new();
        engine.dispatch(
            CommandId::new(),
            NodeWork::StartRun {
                run_id,
                task_id,
                engine: "unknown".to_string(),
                access_policy: AccessPolicy::Automatic,
                workspace_path: None,
            },
            out.clone(),
        );
        engine.dispatch(
            CommandId::new(),
            NodeWork::SendPrompt {
                task_id,
                run_id,
                text: "x".to_string(),
                client_message_id: "m".to_string(),
            },
            out,
        );
        let mut failed = false;
        for _ in 0..3 {
            if let Outgoing::Protocol(NodeToHub::NodeEvent { draft, .. }) =
                tokio::time::timeout(Duration::from_secs(1), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            {
                failed = draft.kind == EventKind::RunFailed;
            }
        }
        assert!(failed);
    }

    #[tokio::test]
    async fn prompt_for_an_uninitialized_run_fails_durably() {
        let engine = CliEngine::new(CliEngineConfig::default());
        let (tx, mut rx) = mpsc::channel::<Outgoing>(16);
        let out = EngineOutbox::new(tx, NodeId::from("local"));
        engine.dispatch(
            CommandId::new(),
            NodeWork::SendPrompt {
                task_id: TaskId::new(),
                run_id: RunId::new(),
                text: "x".to_string(),
                client_message_id: "m".to_string(),
            },
            out,
        );
        let mut failed = false;
        for _ in 0..2 {
            if let Outgoing::Protocol(NodeToHub::NodeEvent { draft, .. }) =
                tokio::time::timeout(Duration::from_secs(1), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            {
                failed = draft.kind == EventKind::RunFailed;
            }
        }
        assert!(failed);
    }
}
