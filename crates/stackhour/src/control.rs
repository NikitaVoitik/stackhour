use serde::Deserialize;
use serde_json::Value;
use stackhour_core::{Error, Result};
use stackhour_domain::{
    AssistantWake, ClientCommand, CommandId, Event, EventKind, HubToClient, NodeId, RunId, TaskId,
};
use stackhour_hub::{AssistantSession, AssistantSettings, HubState};
use stackhour_node::{spawn_engine, CliEngine, CliEngineConfig, NodeConfig, RunRequest, RunningJob};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_MEMORY_CONTEXT_BYTES: usize = 16 * 1024;
const MAX_CONVERSATION_HISTORY_BYTES: usize = 16 * 1024;
const MAX_MEMORY_NOTE_CHARS: usize = 280;

#[derive(Default)]
struct ClaireRunnerState {
    jobs: HashMap<RunId, RunningJob>,
    interrupted: HashSet<RunId>,
}

#[derive(Clone, Default)]
struct ClaireRunner {
    state: Arc<Mutex<ClaireRunnerState>>,
    claude_bin: Option<String>,
    codex_bin: Option<String>,
}

struct ClaireTurn {
    session: AssistantSession,
    system_prompt: Option<String>,
    prompt: String,
}

impl ClaireRunner {
    fn new(claude_bin: Option<String>, codex_bin: Option<String>) -> Self {
        Self {
            state: Arc::new(Mutex::new(ClaireRunnerState::default())),
            claude_bin,
            codex_bin,
        }
    }

    fn start(
        &self,
        hub: Arc<HubState>,
        channel: String,
        settings: AssistantSettings,
        turn: &ClaireTurn,
    ) -> Result<()> {
        let run_id = turn
            .session
            .run_id
            .ok_or_else(|| Error::msg("Claire's hub-local run is missing"))?;
        if self.state.lock().unwrap().jobs.contains_key(&run_id) {
            return Err(Error::msg("Claire already has a turn running"));
        }
        let definition = match settings.engine.as_str() {
            "claude" => stackhour_core::engine::builtin_claude(),
            "codex" => stackhour_core::engine::builtin_codex(),
            _ => return Err(Error::msg("Claire's engine must be claude or codex")),
        };
        let binary = if settings.engine == "claude" {
            self.claude_bin.clone()
        } else {
            self.codex_bin.clone()
        };
        let request = RunRequest {
            prompt: turn.prompt.clone(),
            session_id: turn.session.provider_session_id.clone(),
            model: settings.model(),
            permission_mode: Some("default".to_string()),
            system_prompt: turn.system_prompt.clone(),
            effort: Some(settings.reasoning_effort.clone()),
            cwd: settings.workspace.as_deref().map(PathBuf::from),
            live_status: true,
            bin: binary,
        };
        let (job, handle) = spawn_engine(&definition, request);
        let cleanup_job = job.clone();
        self.state.lock().unwrap().jobs.insert(run_id, job);
        let runner = self.clone();
        let task_id = turn.session.task_id;
        let failure_hub = Arc::clone(&hub);
        let spawned = std::thread::Builder::new()
            .name("stackhour-claire-turn".to_string())
            .spawn(move || {
                let result = handle.join().ok();
                let interrupted = {
                    let mut state = runner.state.lock().unwrap();
                    state.interrupted.remove(&run_id)
                };
                if interrupted {
                    runner.state.lock().unwrap().jobs.remove(&run_id);
                    return;
                }
                match result {
                    Some(result) if result.code == Some(0) && result.error.is_none() => {
                        if let Ok(Some(mut session)) = hub.assistant_session(&channel) {
                            if session.run_id == Some(run_id) {
                                session.provider_session_id = result.session_id.clone();
                                let _ = hub.save_assistant_session(&channel, &session);
                            }
                        }
                        let _ = hub.append_hub_assistant_event(
                            EventKind::MessageAssistantCompleted,
                            task_id,
                            run_id,
                            result.session_id.clone(),
                            serde_json::json!({"text": result.text}),
                        );
                        let _ = hub.append_hub_assistant_event(
                            EventKind::RunCompleted,
                            task_id,
                            run_id,
                            result.session_id,
                            serde_json::json!({}),
                        );
                    }
                    Some(result) => {
                        let error = result.error.unwrap_or_else(|| {
                            let stderr = result.stderr.trim();
                            if stderr.is_empty() {
                                format!("engine exited with code {:?}", result.code)
                            } else {
                                stderr.to_string()
                            }
                        });
                        let _ = hub.append_hub_assistant_event(
                            EventKind::RunFailed,
                            task_id,
                            run_id,
                            result.session_id,
                            serde_json::json!({"error": error, "code": result.code}),
                        );
                    }
                    None => {
                        let _ = hub.append_hub_assistant_event(
                            EventKind::RunFailed,
                            task_id,
                            run_id,
                            None,
                            serde_json::json!({"error": "Claire engine supervisor failed"}),
                        );
                    }
                }
                runner.state.lock().unwrap().jobs.remove(&run_id);
            });
        if let Err(error) = spawned {
            cleanup_job.terminate();
            self.state.lock().unwrap().jobs.remove(&run_id);
            let _ = failure_hub.append_hub_assistant_event(
                EventKind::RunFailed,
                task_id,
                run_id,
                turn.session.provider_session_id.clone(),
                serde_json::json!({"error": format!("cannot start Claire supervisor: {error}")}),
            );
            return Err(Error::msg(format!("cannot start Claire supervisor: {error}")));
        }
        Ok(())
    }

    fn stop(&self, hub: &HubState, session: &AssistantSession) -> Result<bool> {
        let Some(run_id) = session.run_id else {
            return Ok(false);
        };
        let job = {
            let mut state = self.state.lock().unwrap();
            let job = state.jobs.get(&run_id).cloned();
            if job.as_ref().is_some_and(RunningJob::terminate_if_running) {
                state.interrupted.insert(run_id);
                job
            } else {
                None
            }
        };
        let Some(job) = job else {
            return Ok(false);
        };
        drop(job);
        hub.append_hub_assistant_event(
            EventKind::RunInterrupted,
            session.task_id,
            run_id,
            session.provider_session_id.clone(),
            serde_json::json!({}),
        )?;
        Ok(true)
    }

    fn is_running(&self, run_id: RunId) -> bool {
        self.state.lock().unwrap().jobs.contains_key(&run_id)
    }
}

pub fn run(args: &[String], cfg: &stackhour_core::config::Config) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("hub") => run_hub(cfg),
        Some("node") => run_node(cfg),
        Some("fake-telegram") => crate::fake_telegram::run(&args[1..]),
        _ => Err(Error::msg("usage: stackhour control <hub|node|fake-telegram>")),
    }
}

fn section<'a>(cfg: &'a stackhour_core::config::Config, key: &str) -> Option<&'a Value> {
    cfg.raw.get("control")?.get(key)
}

fn string(value: Option<&Value>, key: &str) -> Option<String> {
    value?
        .get(key)?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn required(value: Option<&Value>, key: &str) -> Result<String> {
    string(value, key).ok_or_else(|| Error::msg(format!("control.{key} is required")))
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::msg(e.to_string()))
}

fn run_hub(cfg: &stackhour_core::config::Config) -> Result<()> {
    let hub = section(cfg, "hub");
    let bind = string(hub, "bind").unwrap_or_else(|| "127.0.0.1:4050".to_string());
    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| Error::msg(format!("bad control.hub.bind: {e}")))?;
    let db = string(hub, "db")
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.paths.data_dir.join("control.db"));
    let state = HubState::open_secured(&db, required(hub, "nodeToken")?, required(hub, "clientToken")?)?;

    if section(cfg, "telegram")
        .and_then(|v| v.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        start_telegram(cfg, state.clone())?;
    }

    println!("stackhour control hub: http://{addr}");
    runtime()?.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        stackhour_hub::serve(state, listener).await
    })
}

fn run_node(cfg: &stackhour_core::config::Config) -> Result<()> {
    let node = section(cfg, "node");
    let node_id = string(node, "id")
        .unwrap_or_else(|| hostname::get().unwrap_or_default().to_string_lossy().to_string());
    let engine = CliEngine::new(CliEngineConfig {
        claude_bin: string(node, "claudeBin"),
        codex_bin: string(node, "codexBin"),
        default_workspace: string(node, "workspace").map(PathBuf::from),
    });
    let config = NodeConfig::new(
        required(node, "hubUrl")?,
        NodeId::from(node_id.clone()),
        required(node, "token")?,
    )
    .with_capabilities(serde_json::json!({"engines": ["claude", "codex"]}));

    println!("stackhour control node: {node_id}");
    runtime()?.block_on(async move {
        let (stop, signal) = stackhour_node::shutdown();
        let node_task = tokio::spawn(stackhour_node::run_with_engine(config, Arc::new(engine), signal));
        tokio::select! {
            result = node_task => result.map_err(|e| Error::msg(e.to_string()))?,
            result = tokio::signal::ctrl_c() => {
                result.map_err(|e| Error::msg(e.to_string()))?;
                stop.shutdown();
                Ok(())
            }
        }
    })
}

fn start_telegram(cfg: &stackhour_core::config::Config, state: Arc<HubState>) -> Result<()> {
    let config = section(cfg, "telegram");
    let token = required(config, "token")?;
    let chat_id = config
        .and_then(|v| v.get("chatId"))
        .and_then(Value::as_i64)
        .ok_or_else(|| Error::msg("control.telegram.chatId is required"))?;
    let api_root = string(config, "apiRoot");
    if let Some(root) = &api_root {
        validate_telegram_api_root(root)?;
    }
    let channel = format!("telegram.{chat_id}");

    let mut initial = AssistantSettings {
        engine: string(config, "engine").unwrap_or_else(|| "claude".to_string()),
        workspace: string(config, "workspace"),
        claude_model: string(config, "claudeModel"),
        codex_model: string(config, "codexModel"),
        ..AssistantSettings::default()
    };
    if let Some(personality) = string(config, "personality") {
        initial.personality = personality;
    }
    if let Some(command) = string(config, "memoryCommand") {
        initial.memory_enabled = true;
        initial.memory_command = Some(command);
        initial.memory_dir = string(config, "memoryDir");
    }
    state.initialize_assistant_settings(&initial)?;
    state.set_active_assistant_channel(&channel)?;
    recover_hub_local_assistant(&state, &channel)?;

    let claire_runner = ClaireRunner::new(string(config, "claudeBin"), string(config, "codexBin"));
    let busy_claire = Arc::new(Mutex::new(HashSet::<RunId>::new()));
    let turn_gate = Arc::new(Mutex::new(()));
    let make_tg = move || {
        let mut c = crate::telegram::TelegramConfig::new(token.clone(), chat_id);
        if let Some(root) = &api_root {
            c = c.with_api_root(root.clone());
        }
        crate::telegram::Telegram::with_config(c)
    };
    let input_tg = make_tg();
    let output_tg = make_tg();
    let input_state = state.clone();
    let input_busy = busy_claire.clone();
    let input_gate = turn_gate.clone();
    let input_channel = channel.clone();
    let input_runner = claire_runner.clone();

    std::thread::spawn(move || {
        let mut offset = 0;
        loop {
            let Some(updates) = input_tg.get_updates(offset) else {
                std::thread::sleep(input_tg.empty_poll_sleep());
                continue;
            };
            for update in updates.as_array().into_iter().flatten() {
                offset = update.get("update_id").and_then(Value::as_i64).unwrap_or(offset) + 1;
                let message = update.get("message").or_else(|| update.get("edited_message"));
                let Some(message) = message else { continue };
                if message.pointer("/chat/id").and_then(Value::as_i64) != Some(chat_id) {
                    continue;
                }
                if message.pointer("/from/is_bot").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let Some(text) = message.get("text").and_then(Value::as_str) else {
                    continue;
                };
                match handle_claire_input(
                    &input_state,
                    &input_channel,
                    text,
                    &input_busy,
                    &input_gate,
                    &input_runner,
                ) {
                    Ok(Some(reply)) => {
                        input_tg.send_text(&reply);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        input_tg.send_text("Claire could not accept that message. Please try again.");
                    }
                }
            }
        }
    });

    std::thread::spawn(move || {
        let mut cursor = state
            .read_recent_events(1)
            .ok()
            .and_then(|v| v.last().map(|e| e.sequence))
            .unwrap_or(0);
        loop {
            if let Ok(events) = state.read_events_after(cursor) {
                for event in events {
                    cursor = event.sequence;
                    let claire_session = state.assistant_session(&channel).ok().flatten();
                    let is_claire = claire_session.as_ref().is_some_and(|session| {
                        session.task_id == event.task_id
                            && (event.run_id.is_none() || event.run_id == session.run_id)
                    });
                    if is_claire {
                        handle_claire_event(&state, &channel, &event, &busy_claire, &turn_gate, |reply| {
                            output_tg.send_text(reply)
                        });
                        continue;
                    }
                }
            }
            reconcile_claire_runner(&state, &channel, &busy_claire, &turn_gate, &claire_runner);
            start_pending_claire_follow_up(
                &state,
                &channel,
                &busy_claire,
                &turn_gate,
                &claire_runner,
                |reply| output_tg.send_text(reply),
            );
            wake_claire_for_pending_worker(&state, &channel, &busy_claire, &turn_gate, &claire_runner);
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    Ok(())
}

fn validate_telegram_api_root(root: &str) -> Result<()> {
    let url = reqwest::Url::parse(root)
        .map_err(|error| Error::msg(format!("bad control.telegram.apiRoot: {error}")))?;
    if url.username() != "" || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err(Error::msg(
            "control.telegram.apiRoot cannot contain credentials, query, or fragment",
        ));
    }
    if url.scheme() == "https" && url.host_str() == Some("api.telegram.org") {
        return Ok(());
    }
    let loopback_http = url.scheme() == "http" && url.host_str().is_some_and(is_loopback_host);
    if loopback_http {
        return Ok(());
    }
    Err(Error::msg(
        "control.telegram.apiRoot must be official HTTPS or loopback HTTP",
    ))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn reconcile_claire_runner(
    state: &HubState,
    channel: &str,
    busy_claire: &Mutex<HashSet<RunId>>,
    turn_gate: &Mutex<()>,
    runner: &ClaireRunner,
) {
    let _turn = turn_gate.lock().unwrap_or_else(|poison| poison.into_inner());
    let Ok(Some(session)) = state.assistant_session(channel) else {
        return;
    };
    let Some(run_id) = session.run_id else {
        return;
    };
    if !busy_claire.lock().unwrap().contains(&run_id) || runner.is_running(run_id) {
        return;
    }
    let terminal_is_durable = state.task_events(session.task_id).is_ok_and(|events| {
        events.into_iter().any(|event| {
            event.run_id == Some(run_id)
                && matches!(
                    event.kind,
                    EventKind::RunCompleted | EventKind::RunFailed | EventKind::RunInterrupted
                )
        })
    });
    if terminal_is_durable {
        return;
    }
    if session.pending_wake_event_id.is_some() {
        defer_session_wake(
            state,
            channel,
            &session,
            "assistant process exited without a durable terminal event",
        );
    }
    let _ = state.append_hub_assistant_event(
        EventKind::RunFailed,
        session.task_id,
        run_id,
        session.provider_session_id,
        serde_json::json!({"error": "assistant process exited without a durable terminal event"}),
    );
}

fn start_pending_claire_follow_up(
    state: &Arc<HubState>,
    channel: &str,
    busy_claire: &Mutex<HashSet<RunId>>,
    turn_gate: &Mutex<()>,
    runner: &ClaireRunner,
    send: impl Fn(&str) -> bool,
) {
    let _turn = turn_gate.lock().unwrap_or_else(|poison| poison.into_inner());
    if !busy_claire.lock().unwrap().is_empty() {
        return;
    }
    let Ok(Some(mut session)) = state.assistant_session(channel) else {
        return;
    };
    let Some(prompt) = session.pending_follow_up.take() else {
        return;
    };
    session.action_follow_up_in_progress = true;
    if state.save_assistant_session(channel, &session).is_err() {
        return;
    }
    if let Err(error) = start_claire_turn(state, channel, &prompt, busy_claire, runner) {
        let mut terminal_is_durable = false;
        if let Ok(Some(mut current)) = state.assistant_session(channel) {
            if current.run_id.is_some() {
                append_claire_start_failure(
                    state,
                    &ClaireTurn {
                        session: current.clone(),
                        system_prompt: None,
                        prompt: prompt.clone(),
                    },
                    error.message(),
                );
                terminal_is_durable = current.run_id.is_some_and(|run_id| {
                    state.task_events(current.task_id).is_ok_and(|events| {
                        events.into_iter().any(|event| {
                            event.run_id == Some(run_id)
                                && matches!(
                                    event.kind,
                                    EventKind::RunCompleted
                                        | EventKind::RunFailed
                                        | EventKind::RunInterrupted
                                )
                        })
                    })
                });
            }
            current.pending_follow_up = None;
            current.action_follow_up_in_progress = false;
            let _ = state.save_assistant_session(channel, &current);
        }
        if session.pending_wake_event_id.is_some() {
            defer_session_wake(
                state,
                channel,
                &session,
                "action-result follow-up failed to start",
            );
        } else if !terminal_is_durable {
            let _ = send("Claire couldn't finalize that turn. Please try again.");
        }
    }
}

fn handle_claire_input(
    state: &Arc<HubState>,
    channel: &str,
    text: &str,
    busy_claire: &Mutex<HashSet<RunId>>,
    turn_gate: &Mutex<()>,
    runner: &ClaireRunner,
) -> Result<Option<String>> {
    let _turn = turn_gate.lock().unwrap_or_else(|poison| poison.into_inner());
    let trimmed = text.trim();
    let current_run = state
        .assistant_session(channel)?
        .and_then(|session| session.run_id);
    let current_turn_is_busy =
        current_run.is_some_and(|run_id| busy_claire.lock().unwrap().contains(&run_id));
    if current_turn_is_busy && !matches!(trimmed, "/stop" | "/where") {
        return Ok(Some(
            "I'm still working on the previous message. Let me finish that turn first.".to_string(),
        ));
    }
    match trimmed {
        "/help" | "/start" => Ok(Some(
            "I'm Claire. Talk to me normally, or use:\n\
             /claude · /codex — switch my engine\n\
             /model [name] — show or change this engine's model\n\
             /where — show my current configuration\n\
             /tasks — recent Stackhour task activity\n\
             /remember <fact> — save a durable OptMem note\n\
             /new — start a fresh conversation\n\
             /stop — interrupt my current turn"
                .to_string(),
        )),
        "/where" => {
            let settings = state.assistant_settings()?;
            Ok(Some(format!(
                "{} · {}{} · effort {} · hub-local · memory {}",
                settings.name,
                settings.engine,
                settings
                    .model()
                    .map(|model| format!(" / {model}"))
                    .unwrap_or_default(),
                settings.reasoning_effort,
                if settings.memory_enabled { "OptMem" } else { "off" }
            )))
        }
        "/claude" | "/codex" => {
            let engine = trimmed.trim_start_matches('/');
            let mut settings = state.assistant_settings()?;
            settings.engine = engine.to_string();
            state.save_assistant_settings(&settings)?;
            clear_current_run(state, channel, engine)?;
            Ok(Some(format!(
                "Switched to {engine}{}.",
                settings
                    .model()
                    .map(|model| format!(" using {model}"))
                    .unwrap_or_default()
            )))
        }
        "/new" => {
            if let Some(session) = state.assistant_session(channel)? {
                defer_session_wake(state, channel, &session, "conversation reset");
            }
            state.clear_assistant_session(channel)?;
            Ok(Some("Fresh conversation. What are we doing?".to_string()))
        }
        "/stop" => {
            let session = state.assistant_session(channel)?;
            if let Some(session) = session {
                if runner.stop(state, &session)? {
                    defer_session_wake(state, channel, &session, "assessment stopped by user");
                    if let Some(run_id) = session.run_id {
                        busy_claire.lock().unwrap().remove(&run_id);
                    }
                    let mut cleared = session;
                    cleared.run_id = None;
                    cleared.provider_session_id = None;
                    cleared.pending_wake_event_id = None;
                    cleared.pending_follow_up = None;
                    cleared.action_follow_up_in_progress = false;
                    state.save_assistant_session(channel, &cleared)?;
                    return Ok(Some("I asked the current turn to stop.".to_string()));
                }
            }
            Ok(Some("Nothing is running.".to_string()))
        }
        "/tasks" => start_claire_turn(
            state,
            channel,
            "Review current Stackhour task activity and decide what is useful to tell Nikita.",
            busy_claire,
            runner,
        ),
        _ if trimmed == "/model" => {
            let settings = state.assistant_settings()?;
            Ok(Some(
                settings
                    .model()
                    .map(|model| format!("{} uses {model}.", settings.engine))
                    .unwrap_or_else(|| format!("{} uses its CLI default model.", settings.engine)),
            ))
        }
        _ if trimmed.starts_with("/model ") => {
            let model = trimmed.trim_start_matches("/model ").trim();
            validate_selector("model", model)?;
            let mut settings = state.assistant_settings()?;
            if settings.engine == "claude" {
                settings.claude_model = Some(model.to_string());
            } else {
                settings.codex_model = Some(model.to_string());
            }
            state.save_assistant_settings(&settings)?;
            clear_current_run(state, channel, &settings.engine)?;
            Ok(Some(format!(
                "{} will use {model} on the next turn.",
                settings.engine
            )))
        }
        _ if trimmed.starts_with("/remember ") => {
            let note = trimmed.trim_start_matches("/remember ").trim();
            save_memory_note(&state.assistant_settings()?, note)?;
            Ok(Some("Remembered.".to_string()))
        }
        _ if trimmed.starts_with('/') => Ok(Some(
            "I don't know that command. Use /help, or just talk to me.".to_string(),
        )),
        _ => start_claire_turn(state, channel, trimmed, busy_claire, runner),
    }
}

fn start_claire_turn(
    state: &Arc<HubState>,
    channel: &str,
    prompt: &str,
    busy_claire: &Mutex<HashSet<RunId>>,
    runner: &ClaireRunner,
) -> Result<Option<String>> {
    let turn = submit_claire_prompt(state, channel, prompt)?;
    if let Some(run_id) = turn.session.run_id {
        busy_claire.lock().unwrap().insert(run_id);
    }
    let settings = match state.assistant_settings() {
        Ok(settings) => settings,
        Err(error) => {
            if let Some(run_id) = turn.session.run_id {
                busy_claire.lock().unwrap().remove(&run_id);
                let _ = state.append_hub_assistant_event(
                    EventKind::RunFailed,
                    turn.session.task_id,
                    run_id,
                    turn.session.provider_session_id,
                    serde_json::json!({"error": error.message()}),
                );
            }
            return Err(error);
        }
    };
    if let Err(error) = runner.start(Arc::clone(state), channel.to_string(), settings, &turn) {
        if let Some(run_id) = turn.session.run_id {
            busy_claire.lock().unwrap().remove(&run_id);
        }
        append_claire_start_failure(state, &turn, error.message());
        return Err(error);
    }
    Ok(None)
}

fn append_claire_start_failure(state: &HubState, turn: &ClaireTurn, error: &str) {
    let Some(run_id) = turn.session.run_id else {
        return;
    };
    let already_terminal = state.task_events(turn.session.task_id).is_ok_and(|events| {
        events.into_iter().any(|event| {
            event.run_id == Some(run_id)
                && matches!(
                    event.kind,
                    EventKind::RunCompleted | EventKind::RunFailed | EventKind::RunInterrupted
                )
        })
    });
    if !already_terminal {
        let _ = state.append_hub_assistant_event(
            EventKind::RunFailed,
            turn.session.task_id,
            run_id,
            turn.session.provider_session_id.clone(),
            serde_json::json!({"error": error}),
        );
    }
}

fn recover_hub_local_assistant(state: &HubState, channel: &str) -> Result<()> {
    let Some(mut session) = state.assistant_session(channel)? else {
        return Ok(());
    };
    if let Some(run_id) = session.run_id.take() {
        state.append_hub_assistant_event(
            EventKind::RunInterrupted,
            session.task_id,
            run_id,
            session.provider_session_id.clone(),
            serde_json::json!({"reason": "hub restarted"}),
        )?;
    }
    if let Some(wake_event_id) = session.pending_wake_event_id {
        state.defer_assistant_wake(&wake_event_id, "hub restarted during assessment")?;
    }
    session.pending_wake_event_id = None;
    state.save_assistant_session(channel, &session)
}

fn clear_current_run(state: &HubState, channel: &str, engine: &str) -> Result<()> {
    if let Some(mut session) = state.assistant_session(channel)? {
        defer_session_wake(state, channel, &session, "assistant configuration changed");
        session.run_id = None;
        session.engine = engine.to_string();
        session.provider_session_id = None;
        session.pending_wake_event_id = None;
        session.pending_follow_up = None;
        session.action_follow_up_in_progress = false;
        state.save_assistant_session(channel, &session)?;
    }
    Ok(())
}

fn defer_session_wake(state: &HubState, channel: &str, session: &AssistantSession, reason: &str) {
    if let Some(wake_event_id) = session.pending_wake_event_id {
        let _ = state.defer_assistant_wake(&wake_event_id, reason);
        if let Ok(Some(mut current)) = state.assistant_session(channel) {
            if current.pending_wake_event_id == Some(wake_event_id) {
                current.pending_wake_event_id = None;
                let _ = state.save_assistant_session(channel, &current);
            }
        }
    }
}

fn submit_claire_prompt(state: &HubState, channel: &str, text: &str) -> Result<ClaireTurn> {
    let settings = state.assistant_settings()?;
    let mut session = match state.assistant_session(channel)? {
        Some(session) => session,
        None => {
            let event = event_for_receipt(
                state,
                state.submit_command(ClientCommand::CreateTask {
                    command_id: CommandId::new(),
                    title: format!("{} · Telegram", settings.name),
                }),
            )?;
            AssistantSession {
                task_id: event.task_id,
                run_id: None,
                engine: settings.engine.clone(),
                provider_session_id: None,
                pending_wake_event_id: None,
                pending_follow_up: None,
                action_follow_up_in_progress: false,
            }
        }
    };

    let engine_changed = session.engine != settings.engine;
    let needs_run = session.run_id.is_none() || engine_changed;
    let prompt_contract = build_claire_system_prompt(state, &settings, session.task_id)?;
    let system_prompt = Some(prompt_contract.clone());
    if needs_run {
        session.run_id = Some(state.start_hub_assistant_run(
            session.task_id,
            &settings.engine,
            settings.model(),
            Some(settings.reasoning_effort.clone()),
            prompt_contract,
            settings.workspace.clone(),
        )?);
        session.engine = settings.engine;
        if engine_changed {
            session.provider_session_id = None;
        }
        state.save_assistant_session(channel, &session)?;
    }

    let run_id = session
        .run_id
        .ok_or_else(|| Error::msg("Claire's run was not created"))?;
    let prompt = format!(
        "[Current Stackhour context]\n{}\n\n[Message from Nikita]\n{text}",
        recent_task_context(state)?
    );
    state.append_hub_assistant_message(session.task_id, run_id, &prompt)?;
    Ok(ClaireTurn {
        session,
        system_prompt,
        prompt,
    })
}

fn wake_claire_for_pending_worker(
    state: &Arc<HubState>,
    channel: &str,
    busy_claire: &Mutex<HashSet<RunId>>,
    turn_gate: &Mutex<()>,
    runner: &ClaireRunner,
) {
    let _turn = turn_gate.lock().unwrap_or_else(|poison| poison.into_inner());
    if !busy_claire.lock().unwrap().is_empty() {
        return;
    }
    let Some(wake) = state
        .pending_assistant_wakes(channel)
        .ok()
        .and_then(|wakes| wakes.into_iter().next())
    else {
        return;
    };
    let context = worker_wake_context(state, &wake)
        .unwrap_or_else(|error| format!("Worker history could not be loaded: {}", error.message()));
    let prompt = format!(
        "Durable worker terminal wake {}: task {}, run {} ended as {}.\n\n\
         Worker timeline:\n{}\n\n\
         Assess the result. Decide the next Stackhour actions, if any, and what Nikita should be \
         told. Do not merely repeat raw tool output.",
        wake.event_id,
        wake.task_id,
        wake.run_id,
        wake.kind.as_str(),
        context
    );
    let mut turn = match submit_claire_prompt(state, channel, &prompt) {
        Ok(turn) => turn,
        Err(error) => {
            let _ = state.defer_assistant_wake(&wake.event_id, error.message());
            return;
        }
    };
    turn.session.pending_wake_event_id = Some(wake.event_id);
    if let Err(error) = state.save_assistant_session(channel, &turn.session) {
        let _ = state.defer_assistant_wake(&wake.event_id, error.message());
        return;
    }
    let Some(run_id) = turn.session.run_id else {
        defer_session_wake(state, channel, &turn.session, "Claire run was not created");
        return;
    };
    busy_claire.lock().unwrap().insert(run_id);
    let settings = match state.assistant_settings() {
        Ok(settings) => settings,
        Err(error) => {
            busy_claire.lock().unwrap().remove(&run_id);
            defer_session_wake(state, channel, &turn.session, error.message());
            return;
        }
    };
    if let Err(error) = runner.start(Arc::clone(state), channel.to_string(), settings, &turn) {
        busy_claire.lock().unwrap().remove(&run_id);
        append_claire_start_failure(state, &turn, error.message());
        defer_session_wake(state, channel, &turn.session, error.message());
    }
}

fn worker_wake_context(state: &HubState, wake: &AssistantWake) -> Result<String> {
    let lines = state
        .task_events(wake.task_id)?
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::MessageUser => event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .map(|text| format!("Instruction: {text}")),
            EventKind::MessageAssistantCompleted => event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .map(|text| format!("Worker result: {text}")),
            EventKind::RunFailed => event
                .payload
                .get("error")
                .and_then(Value::as_str)
                .map(|error| format!("Worker failure: {error}")),
            EventKind::RunCompleted => Some("Worker run completed.".to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    Ok(bounded_tail(
        if lines.is_empty() {
            "No worker output was recorded.".to_string()
        } else {
            lines.join("\n")
        },
        MAX_CONVERSATION_HISTORY_BYTES,
    ))
}

fn build_claire_system_prompt(
    state: &HubState,
    settings: &AssistantSettings,
    task_id: TaskId,
) -> Result<String> {
    let memory = load_memory_context(settings)
        .unwrap_or_else(|error| format!("OptMem is unavailable for this turn: {}", error.message()));
    let history = conversation_history(state, task_id)?;
    Ok(claire_system_prompt(settings, &memory, &history))
}

fn claire_system_prompt(settings: &AssistantSettings, memory: &str, history: &str) -> String {
    format!(
        "{personality}\n\n\
         Your name is {name}. You are the persistent assistant; worker tasks are separate agents \
         you coordinate through typed Stackhour actions.\n\n\
         Return one JSON object and nothing else:\n\
         {{\"notify\":true,\"reply\":\"natural Telegram reply\",\
         \"remember\":[\"durable fact, <=280 chars\"],\"actions\":[...]}}\n\
         Supported actions:\n\
         - {{\"type\":\"create_task\",\"title\":\"...\",\"prompt\":\"...\",\
         \"node_id\":\"optional\",\"engine\":\"claude|codex\",\"model\":\"optional\",\
         \"reasoning_effort\":\"low|medium|high\",\"workspace\":\"optional absolute path\"}}\n\
         - {{\"type\":\"send_message\",\"task_id\":\"uuid\",\"run_id\":\"uuid\",\"text\":\"...\"}}\n\
         - {{\"type\":\"stop_run\",\"run_id\":\"uuid\"}}\n\
         Use actions only when Nikita asked you to operate Stackhour. Never invent ids or report \
         success before an action result is returned. Do not place secrets in memory. Administrative \
         operations, credential changes, installs, deletion, backup restore, and approval decisions \
         are not available in this first tool boundary. Set notify=false when no Telegram message \
         is useful. You alone decide which worker progress, questions, tool results, and outcomes \
         appear there.\n\n\
         [OptMem]\n{memory}\n\n\
         [Conversation before this provider run]\n{history}",
        personality = settings.personality,
        name = settings.name,
    )
}

fn conversation_history(state: &HubState, task_id: TaskId) -> Result<String> {
    let events = state.task_events(task_id)?;
    let lines = events
        .iter()
        .rev()
        .filter_map(|event| match event.kind {
            EventKind::MessageUser => event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .map(|text| format!("Nikita: {text}")),
            EventKind::MessageAssistantCompleted => event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .map(|text| format!("Claire: {text}")),
            _ => None,
        })
        .take(20)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        Ok("No earlier messages.".to_string())
    } else {
        Ok(bounded_tail(
            lines.into_iter().rev().collect::<Vec<_>>().join("\n"),
            MAX_CONVERSATION_HISTORY_BYTES,
        ))
    }
}

fn bounded_tail(text: String, max_bytes: usize) -> String {
    const OMITTED: &str = "[Earlier conversation omitted]\n";
    if text.len() <= max_bytes {
        return text;
    }
    let tail_bytes = max_bytes.saturating_sub(OMITTED.len());
    let mut start = text.len().saturating_sub(tail_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("{OMITTED}{}", &text[start..])
}

fn recent_task_context(state: &HubState) -> Result<String> {
    let events = state.read_recent_events(200)?;
    let mut lines = Vec::new();
    for event in events.iter().rev() {
        let summary = match event.kind {
            EventKind::TaskCreated => event
                .payload
                .get("title")
                .and_then(Value::as_str)
                .map(|title| format!("{} created: {title}", short_task(event.task_id))),
            EventKind::RunStarted => Some(format!(
                "{} run {} started on {}",
                short_task(event.task_id),
                event
                    .run_id
                    .map(short_run)
                    .unwrap_or_else(|| "unknown".to_string()),
                event.node_id
            )),
            EventKind::RunCompleted => Some(format!("{} completed", short_task(event.task_id))),
            EventKind::RunFailed => Some(format!("{} failed", short_task(event.task_id))),
            EventKind::RunInterrupted => Some(format!("{} interrupted", short_task(event.task_id))),
            _ => None,
        };
        if let Some(summary) = summary {
            lines.push(summary);
            if lines.len() == 12 {
                break;
            }
        }
    }
    if lines.is_empty() {
        Ok("No task activity yet.".to_string())
    } else {
        Ok(lines.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }
}

#[derive(Debug, Deserialize)]
struct ClaireEnvelope {
    #[serde(default = "default_notify")]
    notify: bool,
    #[serde(default)]
    reply: String,
    #[serde(default)]
    remember: Vec<String>,
    #[serde(default)]
    actions: Vec<ClaireAction>,
    #[serde(skip)]
    malformed: bool,
}

fn default_notify() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaireAction {
    CreateTask {
        title: String,
        prompt: String,
        node_id: Option<String>,
        engine: Option<String>,
        model: Option<String>,
        reasoning_effort: Option<String>,
        workspace: Option<String>,
    },
    SendMessage {
        task_id: String,
        run_id: String,
        text: String,
    },
    StopRun {
        run_id: String,
    },
    #[serde(other)]
    Unknown,
}

fn handle_claire_event(
    state: &HubState,
    channel: &str,
    event: &Event,
    busy_claire: &Mutex<HashSet<RunId>>,
    turn_gate: &Mutex<()>,
    send: impl Fn(&str) -> bool,
) {
    let _turn = turn_gate.lock().unwrap_or_else(|poison| poison.into_inner());
    let projection = process_claire_event(state, channel, event, busy_claire);
    let delivered = projection.reply.as_deref().is_none_or(send);
    if delivered && !projection.awaiting_follow_up {
        if let Some(wake_event_id) = projection.wake_event_id {
            let _ = state.complete_assistant_wake(&wake_event_id);
            if let Ok(Some(mut session)) = state.assistant_session(channel) {
                if session.pending_wake_event_id == Some(wake_event_id) {
                    session.pending_wake_event_id = None;
                    let _ = state.save_assistant_session(channel, &session);
                }
            }
        }
    } else if !delivered {
        if let Some(wake_event_id) = projection.wake_event_id {
            let _ = state.defer_assistant_wake(&wake_event_id, "Telegram delivery failed");
            if let Ok(Some(mut session)) = state.assistant_session(channel) {
                if session.pending_wake_event_id == Some(wake_event_id) {
                    session.pending_wake_event_id = None;
                    let _ = state.save_assistant_session(channel, &session);
                }
            }
        }
    }
}

struct ClaireProjection {
    reply: Option<String>,
    wake_event_id: Option<stackhour_domain::EventId>,
    awaiting_follow_up: bool,
}

fn process_claire_event(
    state: &HubState,
    channel: &str,
    event: &Event,
    busy_claire: &Mutex<HashSet<RunId>>,
) -> ClaireProjection {
    match event.kind {
        EventKind::MessageAssistantCompleted => {
            let raw = event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let envelope = parse_claire_envelope(raw);
            if envelope.malformed {
                let mut was_worker_wake = false;
                if let Ok(Some(mut session)) = state.assistant_session(channel) {
                    was_worker_wake = session.pending_wake_event_id.is_some();
                    session.action_follow_up_in_progress = false;
                    session.pending_follow_up = None;
                    let _ = state.save_assistant_session(channel, &session);
                    defer_session_wake(state, channel, &session, "Claire returned a malformed envelope");
                }
                return ClaireProjection {
                    reply: (!was_worker_wake)
                        .then(|| "Claire returned an invalid response. Please try again.".to_string()),
                    wake_event_id: None,
                    awaiting_follow_up: false,
                };
            }
            let follow_up_in_progress = state
                .assistant_session(channel)
                .ok()
                .flatten()
                .is_some_and(|session| session.action_follow_up_in_progress);
            if follow_up_in_progress && (!envelope.remember.is_empty() || !envelope.actions.is_empty()) {
                let mut was_worker_wake = false;
                if let Ok(Some(mut session)) = state.assistant_session(channel) {
                    was_worker_wake = session.pending_wake_event_id.is_some();
                    session.action_follow_up_in_progress = false;
                    session.pending_follow_up = None;
                    let _ = state.save_assistant_session(channel, &session);
                    defer_session_wake(
                        state,
                        channel,
                        &session,
                        "Claire requested another action during final disclosure",
                    );
                }
                return ClaireProjection {
                    reply: (!was_worker_wake)
                        .then(|| "Claire couldn't finalize that turn. Please try again.".to_string()),
                    wake_event_id: None,
                    awaiting_follow_up: false,
                };
            }
            if follow_up_in_progress {
                if let Ok(Some(mut session)) = state.assistant_session(channel) {
                    session.action_follow_up_in_progress = false;
                    let _ = state.save_assistant_session(channel, &session);
                }
            }
            let mut action_results = Vec::new();
            let has_internal_work = !envelope.remember.is_empty() || !envelope.actions.is_empty();
            match state.assistant_settings() {
                Ok(settings) => {
                    for note in envelope.remember {
                        if let Err(error) = save_memory_note(&settings, &note) {
                            action_results.push(format!("Memory action failed: {}", error.message()));
                        }
                    }
                    for action in envelope.actions {
                        match execute_claire_action(state, channel, &settings, action) {
                            Ok(result) => action_results.push(result),
                            Err(error) => action_results.push(format!("Action failed: {}", error.message())),
                        }
                    }
                }
                Err(error) if has_internal_work => action_results.push(format!(
                    "Internal settings unavailable; no requested action ran: {}",
                    error.message()
                )),
                Err(_) => {}
            }
            let reply = if action_results.is_empty() {
                envelope
                    .notify
                    .then_some(envelope.reply)
                    .filter(|reply| !reply.trim().is_empty())
            } else {
                if let Ok(Some(mut session)) = state.assistant_session(channel) {
                    session.pending_follow_up = Some(format!(
                        "Internal Stackhour action results:\n{}\n\n\
                         Reassess now and make the final notify/reply decision. Do not claim \
                         anything beyond these results.",
                        action_results.join("\n")
                    ));
                    let _ = state.save_assistant_session(channel, &session);
                }
                None
            };
            ClaireProjection {
                reply,
                wake_event_id: state
                    .assistant_session(channel)
                    .ok()
                    .flatten()
                    .and_then(|session| session.pending_wake_event_id),
                awaiting_follow_up: !action_results.is_empty(),
            }
        }
        EventKind::RunFailed => {
            if let Some(run_id) = event.run_id {
                busy_claire.lock().unwrap().remove(&run_id);
            }
            let error = event
                .payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Unknown engine error.");
            let mut was_worker_wake = false;
            let mut matched_session = false;
            if let Ok(Some(mut session)) = state.assistant_session(channel) {
                if session.run_id == event.run_id {
                    matched_session = true;
                    if let Some(wake_event_id) = session.pending_wake_event_id {
                        was_worker_wake = true;
                        let _ = state.defer_assistant_wake(&wake_event_id, error);
                    }
                    session.run_id = None;
                    session.pending_wake_event_id = None;
                    session.pending_follow_up = None;
                    session.action_follow_up_in_progress = false;
                    let _ = state.save_assistant_session(channel, &session);
                }
            }
            ClaireProjection {
                reply: (matched_session && !was_worker_wake)
                    .then(|| "Claire couldn't finish that turn. Please try again.".to_string()),
                wake_event_id: None,
                awaiting_follow_up: false,
            }
        }
        EventKind::RunInterrupted => {
            if let Some(run_id) = event.run_id {
                busy_claire.lock().unwrap().remove(&run_id);
            }
            if let Ok(Some(mut session)) = state.assistant_session(channel) {
                if session.run_id == event.run_id {
                    if let Some(wake_event_id) = session.pending_wake_event_id {
                        let _ = state.defer_assistant_wake(&wake_event_id, "assessment interrupted");
                    }
                    session.run_id = None;
                    session.pending_wake_event_id = None;
                    session.pending_follow_up = None;
                    session.action_follow_up_in_progress = false;
                    let _ = state.save_assistant_session(channel, &session);
                }
            }
            ClaireProjection {
                reply: None,
                wake_event_id: None,
                awaiting_follow_up: false,
            }
        }
        EventKind::RunCompleted => {
            if let Some(run_id) = event.run_id {
                busy_claire.lock().unwrap().remove(&run_id);
            }
            if let Ok(Some(mut session)) = state.assistant_session(channel) {
                if session.run_id == event.run_id {
                    session.run_id = None;
                    let _ = state.save_assistant_session(channel, &session);
                }
            }
            ClaireProjection {
                reply: None,
                wake_event_id: None,
                awaiting_follow_up: false,
            }
        }
        _ => ClaireProjection {
            reply: None,
            wake_event_id: None,
            awaiting_follow_up: false,
        },
    }
}

fn parse_claire_envelope(raw: &str) -> ClaireEnvelope {
    let trimmed = raw.trim();
    let json = trimmed
        .strip_prefix("```json")
        .and_then(|body| body.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    serde_json::from_str(json).unwrap_or_else(|_| ClaireEnvelope {
        notify: false,
        reply: String::new(),
        remember: Vec::new(),
        actions: Vec::new(),
        malformed: true,
    })
}

fn execute_claire_action(
    state: &HubState,
    channel: &str,
    settings: &AssistantSettings,
    action: ClaireAction,
) -> Result<String> {
    match action {
        ClaireAction::CreateTask {
            title,
            prompt,
            node_id,
            engine,
            model,
            reasoning_effort,
            workspace,
        } => {
            let engine = engine.unwrap_or_else(|| settings.engine.clone());
            validate_selector("engine", &engine)?;
            if !matches!(engine.as_str(), "claude" | "codex") {
                return Err(Error::msg("worker engine must be claude or codex"));
            }
            if let Some(model) = model.as_deref() {
                validate_selector("model", model)?;
            }
            let effort = reasoning_effort.unwrap_or_else(|| settings.reasoning_effort.clone());
            validate_effort(&effort)?;
            if let Some(node_id) = node_id.as_deref() {
                validate_selector("node id", node_id)?;
            }
            let preferred_node = node_id.map(NodeId::from);
            if let Some(path) = workspace.as_deref() {
                if !Path::new(path).is_absolute() {
                    return Err(Error::msg("worker workspace must be an absolute path"));
                }
            }
            let (task_id, run_id) = submit_task(
                state,
                &title,
                &prompt,
                channel,
                preferred_node,
                &engine,
                model,
                Some(effort),
                workspace.or_else(|| settings.workspace.clone()),
            )?;
            Ok(format!(
                "Started task {} (run {}).",
                short_task(task_id),
                short_run(run_id)
            ))
        }
        ClaireAction::SendMessage {
            task_id,
            run_id,
            text,
        } => {
            let task_id =
                TaskId::from_str(&task_id).map_err(|error| Error::msg(format!("bad task id: {error}")))?;
            let run_id =
                RunId::from_str(&run_id).map_err(|error| Error::msg(format!("bad run id: {error}")))?;
            state.track_assistant_worker(channel, task_id, run_id)?;
            ensure_accepted(state.submit_command(ClientCommand::SendUserMessage {
                command_id: CommandId::new(),
                task_id,
                run_id: Some(run_id),
                text,
                client_message_id: CommandId::new().to_string(),
            }))?;
            Ok(format!("Sent a follow-up to task {}.", short_task(task_id)))
        }
        ClaireAction::StopRun { run_id } => {
            let run_id =
                RunId::from_str(&run_id).map_err(|error| Error::msg(format!("bad run id: {error}")))?;
            ensure_accepted(state.submit_command(ClientCommand::InterruptRun {
                command_id: CommandId::new(),
                run_id,
            }))?;
            Ok(format!("Stopped run {}.", short_run(run_id)))
        }
        ClaireAction::Unknown => Err(Error::msg("unsupported Claire action")),
    }
}

#[allow(clippy::too_many_arguments)]
fn submit_task(
    state: &HubState,
    title: &str,
    prompt: &str,
    assistant_channel: &str,
    node_id: Option<NodeId>,
    engine: &str,
    model: Option<String>,
    reasoning_effort: Option<String>,
    workspace_path: Option<String>,
) -> Result<(TaskId, RunId)> {
    state.create_worker_task(
        title.chars().take(100).collect(),
        prompt.to_string(),
        Some(assistant_channel.to_string()),
        node_id,
        engine.to_string(),
        model,
        reasoning_effort,
        workspace_path,
    )
}

fn load_memory_context(settings: &AssistantSettings) -> Result<String> {
    if !settings.memory_enabled {
        return Ok("OptMem is disabled.".to_string());
    }
    run_optmem(settings, &["wake"])
}

fn save_memory_note(settings: &AssistantSettings, note: &str) -> Result<()> {
    if !settings.memory_enabled {
        return Err(Error::msg("OptMem is disabled"));
    }
    if note.trim().is_empty() || note.chars().count() > MAX_MEMORY_NOTE_CHARS {
        return Err(Error::msg(format!(
            "memory notes must be one non-empty line of at most {MAX_MEMORY_NOTE_CHARS} characters"
        )));
    }
    if note.chars().any(char::is_control) {
        return Err(Error::msg("memory notes must be one printable line"));
    }
    let _ = run_optmem(settings, &["note", note])?;
    Ok(())
}

fn run_optmem(settings: &AssistantSettings, args: &[&str]) -> Result<String> {
    let executable = settings
        .memory_command
        .as_deref()
        .ok_or_else(|| Error::msg("OptMem command is not configured"))?;
    if !Path::new(executable).is_absolute() {
        return Err(Error::msg("OptMem command must be an absolute path"));
    }
    let mut command = Command::new(executable);
    command.args(args);
    if let Some(memory_dir) = &settings.memory_dir {
        command.env("MEMORY_DIR", memory_dir);
    }
    let output = command
        .output()
        .map_err(|error| Error::msg(format!("cannot run OptMem: {error}")))?;
    let bytes = if output.status.success() {
        &output.stdout
    } else {
        &output.stderr
    };
    let bounded = &bytes[..bytes.len().min(MAX_MEMORY_CONTEXT_BYTES)];
    let text = String::from_utf8_lossy(bounded).trim().to_string();
    if output.status.success() {
        Ok(text)
    } else {
        Err(Error::msg(if text.is_empty() {
            format!("OptMem exited with {}", output.status)
        } else {
            text
        }))
    }
}

fn validate_selector(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 200
        || value
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/' | ':')))
    {
        return Err(Error::msg(format!("{label} contains unsupported characters")));
    }
    Ok(())
}

fn validate_effort(value: &str) -> Result<()> {
    if matches!(value, "low" | "medium" | "high") {
        Ok(())
    } else {
        Err(Error::msg("reasoning effort must be low, medium, or high"))
    }
}

fn short_task(task_id: TaskId) -> String {
    task_id.to_string().chars().take(8).collect()
}

fn short_run(run_id: RunId) -> String {
    run_id.to_string().chars().take(8).collect()
}

fn event_for_receipt(state: &HubState, receipt: HubToClient) -> Result<Event> {
    let sequence = ensure_accepted(receipt)?;
    state
        .read_events_after(sequence - 1)?
        .into_iter()
        .find(|event| event.sequence == sequence)
        .ok_or_else(|| Error::msg("accepted command event is missing"))
}

fn ensure_accepted(receipt: HubToClient) -> Result<i64> {
    match receipt {
        HubToClient::CommandReceipt {
            accepted: true,
            assigned_sequence: Some(sequence),
            ..
        } => Ok(sequence),
        HubToClient::CommandReceipt { error, .. } => {
            Err(Error::msg(format!("hub rejected command: {error:?}")))
        }
        _ => Err(Error::msg("hub returned an invalid command receipt")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_rejects_blank_values() {
        let value = serde_json::json!({"x": "  ", "y": "ok"});
        assert_eq!(string(Some(&value), "x"), None);
        assert_eq!(string(Some(&value), "y"), Some("ok".to_string()));
    }

    #[test]
    fn telegram_api_root_allows_only_official_https_or_loopback_http() {
        assert!(validate_telegram_api_root("https://api.telegram.org").is_ok());
        assert!(validate_telegram_api_root("http://127.0.0.1:4060").is_ok());
        assert!(validate_telegram_api_root("http://localhost:4060").is_ok());
        assert!(validate_telegram_api_root("http://[::1]:4060").is_ok());
        assert!(validate_telegram_api_root("http://api.telegram.org").is_err());
        assert!(validate_telegram_api_root("https://attacker.example").is_err());
        assert!(validate_telegram_api_root("http://0.0.0.0:4060").is_err());
    }

    #[test]
    fn claire_conversation_reuses_its_task_and_run() {
        let state = HubState::in_memory("secret").unwrap();
        let first_turn = submit_claire_prompt(&state, "telegram.1", "hello").unwrap();
        let first = first_turn.session;
        let second_turn = submit_claire_prompt(&state, "telegram.1", "again").unwrap();
        let second = second_turn.session;
        assert_eq!(first, second);
        assert!(first_turn.system_prompt.is_some());
        assert!(
            second_turn.system_prompt.is_some(),
            "a stateless provider turn must receive Claire's contract again"
        );
        let events = state.task_events(first.task_id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == EventKind::TaskCreated)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == EventKind::RunStarted)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == EventKind::MessageUser)
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn claire_provider_process_runs_on_the_hub_and_persists_its_session() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let executable = dir.path().join("fake-claude");
        std::fs::write(
            &executable,
            "#!/bin/sh\n\
             printf '%s\\n' \
             '{\"type\":\"result\",\"session_id\":\"hub-provider-session\",\
             \"result\":\"{\\\"notify\\\":true,\\\"reply\\\":\\\"hello\\\",\
             \\\"actions\\\":[{\\\"type\\\":\\\"stop_run\\\",\\\"run_id\\\":\\\"bad\\\"}]}\"}'\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let state = HubState::in_memory("secret").unwrap();
        let channel = "telegram.1";
        let turn = submit_claire_prompt(&state, channel, "hello").unwrap();
        let runner = ClaireRunner::new(Some(executable.to_string_lossy().to_string()), None);
        runner
            .start(
                Arc::clone(&state),
                channel.to_string(),
                state.assistant_settings().unwrap(),
                &turn,
            )
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let completed = loop {
            let events = state.task_events(turn.session.task_id).unwrap();
            if events.iter().any(|event| event.kind == EventKind::RunCompleted) {
                assert!(events
                    .iter()
                    .filter(|event| {
                        matches!(
                            event.kind,
                            EventKind::RunStarted
                                | EventKind::MessageAssistantCompleted
                                | EventKind::RunCompleted
                        )
                    })
                    .all(|event| event.node_id.as_str() == "hub"));
                break events
                    .into_iter()
                    .find(|event| event.kind == EventKind::MessageAssistantCompleted)
                    .unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "hub-local Claire process did not finish"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let projection = process_claire_event(&state, channel, &completed, &Mutex::new(HashSet::new()));
        assert!(projection.reply.is_none());
        assert!(projection.awaiting_follow_up);
        assert!(state
            .assistant_session(channel)
            .unwrap()
            .unwrap()
            .pending_follow_up
            .unwrap()
            .contains("Action failed"));
        assert_eq!(
            state
                .assistant_session(channel)
                .unwrap()
                .unwrap()
                .provider_session_id
                .as_deref(),
            Some("hub-provider-session")
        );
        assert!(state.list_nodes().unwrap().is_empty());
    }

    #[test]
    fn switching_engine_keeps_task_but_starts_a_new_run() {
        let state = HubState::in_memory("secret").unwrap();
        let first = submit_claire_prompt(&state, "telegram.1", "hello")
            .unwrap()
            .session;
        let mut settings = state.assistant_settings().unwrap();
        settings.engine = "codex".to_string();
        settings.codex_model = Some("gpt-5.6-luna".to_string());
        state.save_assistant_settings(&settings).unwrap();
        clear_current_run(&state, "telegram.1", "codex").unwrap();
        let second = submit_claire_prompt(&state, "telegram.1", "continue")
            .unwrap()
            .session;
        assert_eq!(first.task_id, second.task_id);
        assert_ne!(first.run_id, second.run_id);
        assert_eq!(second.engine, "codex");
    }

    #[test]
    fn claire_rejects_a_second_message_while_her_turn_is_running() {
        let state = HubState::in_memory("secret").unwrap();
        let channel = "telegram.1";
        let session = submit_claire_prompt(&state, channel, "hello").unwrap().session;
        let run_id = session.run_id.unwrap();
        let busy = Mutex::new(HashSet::from([run_id]));
        let gate = Mutex::new(());
        let before = state.task_events(session.task_id).unwrap().len();

        let reply = handle_claire_input(&state, channel, "again", &busy, &gate, &ClaireRunner::default())
            .unwrap()
            .unwrap();

        assert!(reply.contains("still working"));
        assert_eq!(state.task_events(session.task_id).unwrap().len(), before);
    }

    #[test]
    fn malformed_envelope_fails_closed_without_raw_telegram_output() {
        let parsed = parse_claire_envelope("normal answer");
        assert!(parsed.malformed);
        assert!(!parsed.notify);
        assert!(parsed.reply.is_empty());
        assert!(parsed.actions.is_empty());
        assert!(parsed.remember.is_empty());
    }

    #[test]
    fn notify_false_without_reply_is_valid_and_keeps_known_actions() {
        let parsed =
            parse_claire_envelope(r#"{"notify":false,"actions":[{"type":"stop_run","run_id":"bad"}]}"#);
        assert!(!parsed.malformed);
        assert!(!parsed.notify);
        assert_eq!(parsed.actions.len(), 1);
    }

    #[test]
    fn conversation_history_keeps_a_utf8_safe_bounded_tail() {
        let bounded = bounded_tail("Claire 🌙 ".repeat(10_000), 1_024);
        assert!(bounded.len() <= 1_024);
        assert!(bounded.starts_with("[Earlier conversation omitted]\n"));
        assert!(bounded.ends_with(' '));
    }

    #[test]
    fn envelope_accepts_json_code_fence() {
        let parsed = parse_claire_envelope(
            "```json\n{\"reply\":\"hi\",\"remember\":[\"Nikita likes terse replies\"],\"actions\":[]}\n```",
        );
        assert_eq!(parsed.reply, "hi");
        assert_eq!(parsed.remember, ["Nikita likes terse replies"]);
    }

    #[test]
    fn envelope_allows_claire_to_suppress_telegram_output() {
        let parsed = parse_claire_envelope(
            r#"{"notify":false,"reply":"internal assessment","remember":[],"actions":[]}"#,
        );
        assert!(!parsed.notify);
    }

    #[test]
    fn notify_false_projection_never_calls_telegram_send() {
        let state = HubState::in_memory("secret").unwrap();
        let channel = "telegram.1";
        let session = submit_claire_prompt(&state, channel, "quietly assess")
            .unwrap()
            .session;
        let run_id = session.run_id.unwrap();
        state
            .append_hub_assistant_event(
                EventKind::MessageAssistantCompleted,
                session.task_id,
                run_id,
                None,
                serde_json::json!({
                    "text": "{\"notify\":false,\"reply\":\"private assessment\",\"actions\":[]}"
                }),
            )
            .unwrap();
        let event = state
            .task_events(session.task_id)
            .unwrap()
            .into_iter()
            .find(|event| event.kind == EventKind::MessageAssistantCompleted)
            .unwrap();
        let sends = std::cell::Cell::new(0);

        handle_claire_event(
            &state,
            channel,
            &event,
            &Mutex::new(HashSet::from([run_id])),
            &Mutex::new(()),
            |_| {
                sends.set(sends.get() + 1);
                true
            },
        );

        assert_eq!(sends.get(), 0);
    }

    #[test]
    fn final_action_follow_up_cannot_schedule_another_action_follow_up() {
        let state = HubState::in_memory("secret").unwrap();
        let channel = "telegram.1";
        let mut session = submit_claire_prompt(&state, channel, "do something")
            .unwrap()
            .session;
        let run_id = session.run_id.unwrap();
        session.action_follow_up_in_progress = true;
        state.save_assistant_session(channel, &session).unwrap();
        state
            .append_hub_assistant_event(
                EventKind::MessageAssistantCompleted,
                session.task_id,
                run_id,
                None,
                serde_json::json!({
                    "text": "{\"notify\":true,\"reply\":\"done\",\"remember\":[\"again\"],\"actions\":[]}"
                }),
            )
            .unwrap();
        let event = state
            .task_events(session.task_id)
            .unwrap()
            .into_iter()
            .find(|event| event.kind == EventKind::MessageAssistantCompleted)
            .unwrap();

        let projection = process_claire_event(&state, channel, &event, &Mutex::new(HashSet::new()));
        let saved = state.assistant_session(channel).unwrap().unwrap();

        assert_eq!(
            projection.reply.as_deref(),
            Some("Claire couldn't finalize that turn. Please try again.")
        );
        assert!(!projection.awaiting_follow_up);
        assert_eq!(saved.pending_follow_up, None);
        assert!(!saved.action_follow_up_in_progress);
    }

    #[test]
    fn reconciliation_waits_for_an_already_durable_terminal_event() {
        let state = HubState::in_memory("secret").unwrap();
        let channel = "telegram.1";
        let session = submit_claire_prompt(&state, channel, "hello").unwrap().session;
        let run_id = session.run_id.unwrap();
        state
            .append_hub_assistant_event(
                EventKind::RunCompleted,
                session.task_id,
                run_id,
                None,
                serde_json::json!({}),
            )
            .unwrap();
        let busy = Mutex::new(HashSet::from([run_id]));

        reconcile_claire_runner(&state, channel, &busy, &Mutex::new(()), &ClaireRunner::default());

        assert!(busy.lock().unwrap().contains(&run_id));
        assert!(!state
            .task_events(session.task_id)
            .unwrap()
            .iter()
            .any(|event| event.kind == EventKind::RunFailed && event.run_id == Some(run_id)));
    }

    #[test]
    fn selector_rejects_whitespace_and_shell_metacharacters() {
        assert!(validate_selector("model", "gpt-5.6-luna").is_ok());
        assert!(validate_selector("model", "gpt 5").is_err());
        assert!(validate_selector("model", "x;touch").is_err());
    }

    #[test]
    fn claire_does_not_create_an_orphan_task_when_no_worker_is_eligible() {
        let state = HubState::in_memory("secret").unwrap();
        let settings = AssistantSettings::default();
        let error = execute_claire_action(
            &state,
            "telegram.1",
            &settings,
            ClaireAction::CreateTask {
                title: "Check CI".to_string(),
                prompt: "Find the failure.".to_string(),
                node_id: Some("local".to_string()),
                engine: Some("codex".to_string()),
                model: Some("gpt-5.6-luna".to_string()),
                reasoning_effort: Some("high".to_string()),
                workspace: Some("/tmp/project".to_string()),
            },
        )
        .unwrap_err();
        assert!(error.message().contains("not active and eligible"));
        assert!(state.read_events_after(0).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn claire_can_create_a_worker_on_an_active_capable_node() {
        use std::os::unix::fs::PermissionsExt;

        let runtime = runtime().unwrap();
        runtime.block_on(async {
            let directory = tempfile::TempDir::new().unwrap();
            let executable = directory.path().join("fake-codex");
            std::fs::write(
                &executable,
                "#!/bin/sh\n\
                 cat >/dev/null\n\
                 echo '{\"type\":\"thread.started\",\"thread_id\":\"worker-thread\"}'\n\
                 echo '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\
                 \"text\":\"worker answer\"}}'\n",
            )
            .unwrap();
            let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&executable, permissions).unwrap();
            let claire_executable = directory.path().join("fake-claude");
            std::fs::write(
                &claire_executable,
                "#!/bin/sh\n\
                 cat >/dev/null\n\
                 echo '{\"type\":\"result\",\"session_id\":\"claire-wake\",\
                 \"result\":\"{\\\"reply\\\":\\\"Worker checked; all good.\\\",\
                 \\\"remember\\\":[],\\\"actions\\\":[]}\"}'\n",
            )
            .unwrap();
            let mut permissions = std::fs::metadata(&claire_executable).unwrap().permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&claire_executable, permissions).unwrap();
            let state = HubState::in_memory("secret").unwrap();
            let (address, hub_task) = stackhour_hub::spawn(Arc::clone(&state), "127.0.0.1:0")
                .await
                .unwrap();
            let config = NodeConfig::new(
                format!("ws://{address}/v1/node/connect"),
                NodeId::from("worker"),
                "secret",
            )
            .with_capabilities(serde_json::json!({"engines": ["codex"]}))
            .with_backoff(Duration::from_millis(10), Duration::from_millis(20));
            let (shutdown, signal) = stackhour_node::shutdown();
            let node_task = tokio::spawn(async move {
                let _ = stackhour_node::run_with_engine(
                    config,
                    Arc::new(CliEngine::new(CliEngineConfig {
                        codex_bin: Some(executable.to_string_lossy().to_string()),
                        ..CliEngineConfig::default()
                    })),
                    signal,
                )
                .await;
            });
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while !state
                .list_nodes()
                .unwrap()
                .iter()
                .any(|node| node.id == "worker" && node.status == "connected")
            {
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            let result = execute_claire_action(
                &state,
                "telegram.1",
                &AssistantSettings::default(),
                ClaireAction::CreateTask {
                    title: "Check CI".to_string(),
                    prompt: "Find the failure.".to_string(),
                    node_id: Some("worker".to_string()),
                    engine: Some("codex".to_string()),
                    model: None,
                    reasoning_effort: Some("high".to_string()),
                    workspace: Some("/tmp/project".to_string()),
                },
            )
            .unwrap();
            assert!(result.contains("Started task"));
            let wake_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while state.pending_assistant_wakes("telegram.1").unwrap().is_empty() {
                assert!(
                    tokio::time::Instant::now() < wake_deadline,
                    "worker events: {:?}",
                    state.read_events_after(0).unwrap()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let wake_id = state.pending_assistant_wakes("telegram.1").unwrap()[0].event_id;
            let busy = Mutex::new(HashSet::new());
            let gate = Mutex::new(());
            let runner = ClaireRunner::new(Some(claire_executable.to_string_lossy().to_string()), None);
            wake_claire_for_pending_worker(&state, "telegram.1", &busy, &gate, &runner);
            let assessment_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            let completed = loop {
                if let Some(event) = state.read_events_after(0).unwrap().into_iter().find(|event| {
                    event.kind == EventKind::MessageAssistantCompleted && event.node_id.as_str() == "hub"
                }) {
                    break event;
                }
                assert!(tokio::time::Instant::now() < assessment_deadline);
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            let sent = Mutex::new(Vec::new());
            handle_claire_event(&state, "telegram.1", &completed, &busy, &gate, |reply| {
                sent.lock().unwrap().push(reply.to_string());
                false
            });
            assert_eq!(sent.lock().unwrap().as_slice(), ["Worker checked; all good."]);
            assert_eq!(
                state
                    .assistant_session("telegram.1")
                    .unwrap()
                    .unwrap()
                    .pending_wake_event_id,
                None
            );
            assert!(
                state.complete_assistant_wake(&wake_id).unwrap(),
                "failed Telegram delivery must leave the wake unprocessed"
            );
            assert!(!state.complete_assistant_wake(&wake_id).unwrap());

            shutdown.shutdown();
            let _ = node_task.await;
            hub_task.abort();
        });
    }

    #[test]
    fn hub_restart_clears_dead_claire_run_without_completing_pending_wake_marker() {
        let state = HubState::in_memory("secret").unwrap();
        let channel = "telegram.1";
        let mut session = submit_claire_prompt(&state, channel, "hello").unwrap().session;
        let old_run = session.run_id.unwrap();
        session.pending_wake_event_id = Some(stackhour_domain::EventId::new());
        state.save_assistant_session(channel, &session).unwrap();

        recover_hub_local_assistant(&state, channel).unwrap();

        let recovered = state.assistant_session(channel).unwrap().unwrap();
        assert_eq!(recovered.run_id, None);
        assert_eq!(recovered.pending_wake_event_id, None);
        assert!(state
            .task_events(recovered.task_id)
            .unwrap()
            .iter()
            .any(|event| event.kind == EventKind::RunInterrupted && event.run_id == Some(old_run)));
    }

    #[cfg(unix)]
    #[test]
    fn optmem_is_invoked_without_a_shell_and_with_a_bounded_contract() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("memo");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s|%s|%s' \"$1\" \"$2\" \"$MEMORY_DIR\"\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();

        let settings = AssistantSettings {
            memory_enabled: true,
            memory_command: Some(script.to_string_lossy().to_string()),
            memory_dir: Some(dir.path().join("memory").to_string_lossy().to_string()),
            ..AssistantSettings::default()
        };

        assert!(load_memory_context(&settings).unwrap().starts_with("wake||"));
        save_memory_note(&settings, "literal; not a shell command").unwrap();
    }

    #[test]
    fn ensure_accepted_rejects_non_receipt() {
        assert!(ensure_accepted(HubToClient::Heartbeat).is_err());
    }
}
