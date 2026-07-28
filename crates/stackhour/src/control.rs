use serde_json::Value;
use stackhour_core::{Error, Result};
use stackhour_domain::{
    AccessPolicy, ClientCommand, CommandId, EventKind, HubToClient, NodeId, RunId, TaskId,
};
use stackhour_hub::HubState;
use stackhour_node::{CliEngine, CliEngineConfig, NodeConfig};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    let node_id = NodeId::from(string(config, "nodeId").unwrap_or_else(|| "local".to_string()));
    let engine = string(config, "engine").unwrap_or_else(|| "claude".to_string());
    let workspace = string(config, "workspace");
    let api_root = string(config, "apiRoot");
    let tracked = Arc::new(Mutex::new(HashSet::<TaskId>::new()));
    let last_run = Arc::new(Mutex::new(None::<RunId>));

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
    let input_last_run = last_run;

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
                let Some(text) = message.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if text == "/stop" {
                    let run_id = *input_last_run.lock().unwrap();
                    if let Some(run_id) = run_id {
                        input_state.submit_command(ClientCommand::InterruptRun {
                            command_id: CommandId::new(),
                            run_id,
                        });
                        input_tg.send("Interrupt requested.");
                    }
                    continue;
                }
                match submit_prompt(&input_state, text, node_id.clone(), &engine, workspace.clone()) {
                    Ok((task_id, run_id)) => {
                        input_tracked.lock().unwrap().insert(task_id);
                        *input_last_run.lock().unwrap() = Some(run_id);
                        input_tg.send("Task started.");
                    }
                    Err(error) => {
                        input_tg.send(&format!("Task failed to start: {}", error.message()));
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
                    if !tracked.lock().unwrap().contains(&event.task_id) {
                        continue;
                    }
                    match event.kind {
                        EventKind::MessageAssistantCompleted => {
                            if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                                output_tg.send(text);
                            }
                            tracked.lock().unwrap().remove(&event.task_id);
                        }
                        EventKind::RunFailed => {
                            let error = event
                                .payload
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("Unknown engine error.");
                            output_tg.send(&format!("Task failed: {error}"));
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

fn submit_prompt(
    state: &HubState,
    text: &str,
    node_id: NodeId,
    engine: &str,
    workspace_path: Option<String>,
) -> Result<(TaskId, RunId)> {
    let task_event = event_for_receipt(
        state,
        state.submit_command(ClientCommand::CreateTask {
            command_id: CommandId::new(),
            title: text.chars().take(100).collect(),
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
            access_policy: AccessPolicy::Automatic,
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
        text: text.to_string(),
        client_message_id: CommandId::new().to_string(),
    }))?;
    Ok((task_id, run_id))
}

fn event_for_receipt(state: &HubState, receipt: HubToClient) -> Result<stackhour_domain::Event> {
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
    fn submit_prompt_creates_task_run_and_message() {
        let state = HubState::in_memory("secret").unwrap();
        let (task, run) = submit_prompt(&state, "test it", NodeId::from("local"), "claude", None).unwrap();
        let events = state.read_events_after(0).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].task_id, task);
        assert_eq!(events[1].run_id, Some(run));
        assert_eq!(events[2].kind, EventKind::MessageUser);
    }

    #[test]
    fn ensure_accepted_rejects_non_receipt() {
        assert!(ensure_accepted(HubToClient::Heartbeat).is_err());
    }
}
