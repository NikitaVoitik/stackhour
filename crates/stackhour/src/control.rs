use serde::Deserialize;
use serde_json::Value;
use stackhour_core::{Error, Result};
use stackhour_domain::{
    AccessPolicy, ClientCommand, CommandId, Event, EventKind, HubToClient, NodeId, RunId, TaskId,
};
use stackhour_hub::{AssistantSession, AssistantSettings, HubState};
use stackhour_node::{CliEngine, CliEngineConfig, NodeConfig};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_MEMORY_CONTEXT_BYTES: usize = 16 * 1024;
const MAX_CONVERSATION_HISTORY_BYTES: usize = 16 * 1024;
const MAX_MEMORY_NOTE_CHARS: usize = 280;

pub fn run(args: &[String], cfg: &stackhour_core::config::Config) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("hub") => run_hub(cfg),
        Some("node") => run_node(cfg),
        _ => Err(Error::msg("usage: stackhour control <hub|node>")),
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
    let channel = format!("telegram.{chat_id}");

    let mut initial = AssistantSettings {
        node_id: string(config, "nodeId").unwrap_or_else(|| "local".to_string()),
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

    let tracked = Arc::new(Mutex::new(HashSet::<TaskId>::new()));
    let busy_claire = Arc::new(Mutex::new(HashSet::<RunId>::new()));
    let make_tg = move || {
        let mut c = stackhour_bridge::telegram::TgConfig::new(token.clone(), chat_id);
        if let Some(root) = &api_root {
            c = c.with_api_root(root.clone());
        }
        stackhour_bridge::telegram::Tg::with_config(c)
    };
    let input_tg = make_tg();
    let output_tg = make_tg();
    let input_state = state.clone();
    let input_tracked = tracked.clone();
    let input_busy = busy_claire.clone();
    let input_channel = channel.clone();

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
                match handle_claire_input(&input_state, &input_channel, text, &input_tracked, &input_busy) {
                    Ok(Some(reply)) => {
                        input_tg.send(&reply);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        input_tg.send(&format!("Claire couldn't do that: {}", error.message()));
                    }
                }
            }
        }
    });

    std::thread::spawn(move || {
        let mut cursor = state
            .read_events_after(0)
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
                        handle_claire_event(&state, &channel, &event, &tracked, &busy_claire, &output_tg);
                        continue;
                    }
                    if !tracked.lock().unwrap().contains(&event.task_id) {
                        continue;
                    }
                    match event.kind {
                        EventKind::MessageAssistantCompleted => {
                            if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                                output_tg
                                    .send(&format!("Task {} finished:\n{text}", short_task(event.task_id)));
                            }
                            tracked.lock().unwrap().remove(&event.task_id);
                        }
                        EventKind::RunFailed => {
                            let error = event
                                .payload
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("Unknown engine error.");
                            output_tg.send(&format!("Task {} failed: {error}", short_task(event.task_id)));
                            tracked.lock().unwrap().remove(&event.task_id);
                        }
                        _ => {}
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    Ok(())
}

fn handle_claire_input(
    state: &HubState,
    channel: &str,
    text: &str,
    tracked: &Mutex<HashSet<TaskId>>,
    busy_claire: &Mutex<HashSet<RunId>>,
) -> Result<Option<String>> {
    let trimmed = text.trim();
    let current_run = state
        .assistant_session(channel)?
        .and_then(|session| session.run_id);
    let current_turn_is_busy =
        current_run.is_some_and(|run_id| busy_claire.lock().unwrap().contains(&run_id));
    if current_turn_is_busy && !matches!(trimmed, "/stop" | "/where" | "/tasks") {
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
                "{} · {}{} · effort {} · node {} · memory {}",
                settings.name,
                settings.engine,
                settings
                    .model()
                    .map(|model| format!(" / {model}"))
                    .unwrap_or_default(),
                settings.reasoning_effort,
                settings.node_id,
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
            state.clear_assistant_session(channel)?;
            Ok(Some("Fresh conversation. What are we doing?".to_string()))
        }
        "/stop" => {
            let session = state.assistant_session(channel)?;
            if let Some(run_id) = session.and_then(|session| session.run_id) {
                ensure_accepted(state.submit_command(ClientCommand::InterruptRun {
                    command_id: CommandId::new(),
                    run_id,
                }))?;
                Ok(Some("I asked the current turn to stop.".to_string()))
            } else {
                Ok(Some("Nothing is running.".to_string()))
            }
        }
        "/tasks" => Ok(Some(recent_task_context(state)?)),
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
        _ => {
            let (session, started) = submit_claire_prompt(state, channel, trimmed)?;
            if let Some(run_id) = session.run_id {
                busy_claire.lock().unwrap().insert(run_id);
            }
            if started {
                Ok(Some("Claire is awake.".to_string()))
            } else {
                let _ = tracked;
                Ok(None)
            }
        }
    }
}

fn clear_current_run(state: &HubState, channel: &str, engine: &str) -> Result<()> {
    if let Some(mut session) = state.assistant_session(channel)? {
        session.run_id = None;
        session.engine = engine.to_string();
        state.save_assistant_session(channel, &session)?;
    }
    Ok(())
}

fn submit_claire_prompt(state: &HubState, channel: &str, text: &str) -> Result<(AssistantSession, bool)> {
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
            }
        }
    };

    let needs_run = session.run_id.is_none() || session.engine != settings.engine;
    if needs_run {
        let memory = load_memory_context(&settings)
            .unwrap_or_else(|error| format!("OptMem is unavailable for this turn: {}", error.message()));
        let history = conversation_history(state, session.task_id)?;
        let system_prompt = claire_system_prompt(&settings, &memory, &history);
        let event = event_for_receipt(
            state,
            state.submit_command(ClientCommand::StartRun {
                command_id: CommandId::new(),
                task_id: session.task_id,
                node_id: NodeId::from(settings.node_id.clone()),
                engine: settings.engine.clone(),
                model: settings.model(),
                reasoning_effort: Some(settings.reasoning_effort.clone()),
                system_prompt: Some(system_prompt),
                access_policy: AccessPolicy::Supervised,
                workspace_path: settings.workspace.clone(),
            }),
        )?;
        session.run_id = event.run_id;
        session.engine = settings.engine;
        state.save_assistant_session(channel, &session)?;
    }

    let run_id = session
        .run_id
        .ok_or_else(|| Error::msg("Claire's run was not created"))?;
    let prompt = format!(
        "[Current Stackhour context]\n{}\n\n[Message from Nikita]\n{text}",
        recent_task_context(state)?
    );
    ensure_accepted(state.submit_command(ClientCommand::SendUserMessage {
        command_id: CommandId::new(),
        task_id: session.task_id,
        run_id: Some(run_id),
        text: prompt,
        client_message_id: CommandId::new().to_string(),
    }))?;
    Ok((session, needs_run))
}

fn claire_system_prompt(settings: &AssistantSettings, memory: &str, history: &str) -> String {
    format!(
        "{personality}\n\n\
         Your name is {name}. You are the persistent assistant; worker tasks are separate agents \
         you coordinate through typed Stackhour actions.\n\n\
         Return one JSON object and nothing else:\n\
         {{\"reply\":\"natural Telegram reply\",\"remember\":[\"durable fact, <=280 chars\"],\
         \"actions\":[...]}}\n\
         Supported actions:\n\
         - {{\"type\":\"create_task\",\"title\":\"...\",\"prompt\":\"...\",\
         \"node_id\":\"optional\",\"engine\":\"claude|codex\",\"model\":\"optional\",\
         \"reasoning_effort\":\"low|medium|high\",\"workspace\":\"optional absolute path\"}}\n\
         - {{\"type\":\"send_message\",\"task_id\":\"uuid\",\"run_id\":\"uuid\",\"text\":\"...\"}}\n\
         - {{\"type\":\"stop_run\",\"run_id\":\"uuid\"}}\n\
         Use actions only when Nikita asked you to operate Stackhour. Never invent ids or report \
         success before an action result is returned. Do not place secrets in memory. Administrative \
         operations, credential changes, installs, deletion, backup restore, and approval decisions \
         are not available in this first tool boundary.\n\n\
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
    let events = state.read_events_after(0)?;
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
    reply: String,
    #[serde(default)]
    remember: Vec<String>,
    #[serde(default)]
    actions: Vec<ClaireAction>,
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
}

fn handle_claire_event(
    state: &HubState,
    channel: &str,
    event: &Event,
    tracked: &Mutex<HashSet<TaskId>>,
    busy_claire: &Mutex<HashSet<RunId>>,
    telegram: &stackhour_bridge::telegram::Tg,
) {
    match event.kind {
        EventKind::MessageAssistantCompleted => {
            if let Some(run_id) = event.run_id {
                busy_claire.lock().unwrap().remove(&run_id);
            }
            let raw = event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let envelope = parse_claire_envelope(raw);
            let mut replies = vec![envelope.reply];
            if let Ok(settings) = state.assistant_settings() {
                for note in envelope.remember {
                    if let Err(error) = save_memory_note(&settings, &note) {
                        replies.push(format!("Memory note failed: {}", error.message()));
                    }
                }
                for action in envelope.actions {
                    match execute_claire_action(state, &settings, action, tracked) {
                        Ok(result) => replies.push(result),
                        Err(error) => replies.push(format!("Action failed: {}", error.message())),
                    }
                }
            }
            let reply = replies
                .into_iter()
                .filter(|part| !part.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            if !reply.is_empty() {
                telegram.send(&reply);
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
            telegram.send(&format!("I hit a problem: {error}"));
            if let Ok(Some(mut session)) = state.assistant_session(channel) {
                session.run_id = None;
                let _ = state.save_assistant_session(channel, &session);
            }
        }
        EventKind::RunInterrupted => {
            if let Some(run_id) = event.run_id {
                busy_claire.lock().unwrap().remove(&run_id);
            }
            if let Ok(Some(mut session)) = state.assistant_session(channel) {
                session.run_id = None;
                let _ = state.save_assistant_session(channel, &session);
            }
        }
        _ => {}
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
        reply: raw.to_string(),
        remember: Vec::new(),
        actions: Vec::new(),
    })
}

fn execute_claire_action(
    state: &HubState,
    settings: &AssistantSettings,
    action: ClaireAction,
    tracked: &Mutex<HashSet<TaskId>>,
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
            let node_id = node_id.unwrap_or_else(|| settings.node_id.clone());
            validate_selector("node id", &node_id)?;
            if let Some(path) = workspace.as_deref() {
                if !Path::new(path).is_absolute() {
                    return Err(Error::msg("worker workspace must be an absolute path"));
                }
            }
            let (task_id, run_id) = submit_task(
                state,
                &title,
                &prompt,
                NodeId::from(node_id),
                &engine,
                model,
                Some(effort),
                workspace.or_else(|| settings.workspace.clone()),
            )?;
            tracked.lock().unwrap().insert(task_id);
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
            ensure_accepted(state.submit_command(ClientCommand::SendUserMessage {
                command_id: CommandId::new(),
                task_id,
                run_id: Some(run_id),
                text,
                client_message_id: CommandId::new().to_string(),
            }))?;
            tracked.lock().unwrap().insert(task_id);
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
    }
}

#[allow(clippy::too_many_arguments)]
fn submit_task(
    state: &HubState,
    title: &str,
    prompt: &str,
    node_id: NodeId,
    engine: &str,
    model: Option<String>,
    reasoning_effort: Option<String>,
    workspace_path: Option<String>,
) -> Result<(TaskId, RunId)> {
    let task_event = event_for_receipt(
        state,
        state.submit_command(ClientCommand::CreateTask {
            command_id: CommandId::new(),
            title: title.chars().take(100).collect(),
        }),
    )?;
    let task_id = task_event.task_id;
    let run_event = event_for_receipt(
        state,
        state.submit_command(ClientCommand::StartRun {
            command_id: CommandId::new(),
            task_id,
            node_id,
            engine: engine.to_string(),
            model,
            reasoning_effort,
            system_prompt: None,
            access_policy: AccessPolicy::Supervised,
            workspace_path,
        }),
    )?;
    let run_id = run_event
        .run_id
        .ok_or_else(|| Error::msg("hub did not assign a run id"))?;
    ensure_accepted(state.submit_command(ClientCommand::SendUserMessage {
        command_id: CommandId::new(),
        task_id,
        run_id: Some(run_id),
        text: prompt.to_string(),
        client_message_id: CommandId::new().to_string(),
    }))?;
    Ok((task_id, run_id))
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
    fn claire_conversation_reuses_its_task_and_run() {
        let state = HubState::in_memory("secret").unwrap();
        let first = submit_claire_prompt(&state, "telegram.1", "hello").unwrap().0;
        let second = submit_claire_prompt(&state, "telegram.1", "again").unwrap().0;
        assert_eq!(first, second);
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

    #[test]
    fn switching_engine_keeps_task_but_starts_a_new_run() {
        let state = HubState::in_memory("secret").unwrap();
        let first = submit_claire_prompt(&state, "telegram.1", "hello").unwrap().0;
        let mut settings = state.assistant_settings().unwrap();
        settings.engine = "codex".to_string();
        settings.codex_model = Some("gpt-5.6-luna".to_string());
        state.save_assistant_settings(&settings).unwrap();
        clear_current_run(&state, "telegram.1", "codex").unwrap();
        let second = submit_claire_prompt(&state, "telegram.1", "continue").unwrap().0;
        assert_eq!(first.task_id, second.task_id);
        assert_ne!(first.run_id, second.run_id);
        assert_eq!(second.engine, "codex");
    }

    #[test]
    fn claire_rejects_a_second_message_while_her_turn_is_running() {
        let state = HubState::in_memory("secret").unwrap();
        let channel = "telegram.1";
        let session = submit_claire_prompt(&state, channel, "hello").unwrap().0;
        let run_id = session.run_id.unwrap();
        let tracked = Mutex::new(HashSet::new());
        let busy = Mutex::new(HashSet::from([run_id]));
        let before = state.task_events(session.task_id).unwrap().len();

        let reply = handle_claire_input(&state, channel, "again", &tracked, &busy)
            .unwrap()
            .unwrap();

        assert!(reply.contains("still working"));
        assert_eq!(state.task_events(session.task_id).unwrap().len(), before);
    }

    #[test]
    fn envelope_falls_back_to_plain_text() {
        let parsed = parse_claire_envelope("normal answer");
        assert_eq!(parsed.reply, "normal answer");
        assert!(parsed.actions.is_empty());
        assert!(parsed.remember.is_empty());
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
    fn selector_rejects_whitespace_and_shell_metacharacters() {
        assert!(validate_selector("model", "gpt-5.6-luna").is_ok());
        assert!(validate_selector("model", "gpt 5").is_err());
        assert!(validate_selector("model", "x;touch").is_err());
    }

    #[test]
    fn claire_can_create_a_tracked_worker_task() {
        let state = HubState::in_memory("secret").unwrap();
        let settings = AssistantSettings::default();
        let tracked = Mutex::new(HashSet::new());
        let result = execute_claire_action(
            &state,
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
            &tracked,
        )
        .unwrap();
        assert!(result.starts_with("Started task "));
        assert_eq!(tracked.lock().unwrap().len(), 1);
        let events = state.read_events_after(0).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].payload["model"], "gpt-5.6-luna");
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
