//! stackhour-hub — the durable authority over task history.
//!
//! The hub is an [`axum`] WebSocket server. **Both** clients and execution
//! nodes dial *in* to it (a node's "outbound authenticated connection" is an
//! outbound WebSocket from the node to this hub). The hub applies every client
//! command and node event through [`stackhour_domain::Hub`] — the append-only
//! SQLite event log that is the *only* sequence authority — and live-broadcasts
//! each newly-appended [`Event`] to every subscribed client.
//!
//! This is the center of the first vertical slice from
//! `docs/architecture/remote-agent-control-plane.md`. The main binary adds the
//! Telegram projection. The node currently uses native CLI adapters.
//!
//! # Transport contract (frozen; identical in `stackhour-hub` and
//! `stackhour-node`)
//!
//! - **Framing.** Every WebSocket message is exactly one JSON-serialized
//!   [`stackhour_domain::protocol`] value as a *text* frame. One frame = one
//!   message.
//! - **Node link `GET /v1/node/connect`.** The node's first frame is a
//!   [`NodeHello`]. The hub compares its opaque bearer `token` against the
//!   configured shared secret in constant time and negotiates the protocol
//!   version via [`HubWelcome::negotiate`], then replies with a [`HubWelcome`]
//!   as its first frame. A rejected node is sent the welcome and closed.
//! - **Client link `GET /v1/client/connect`.** The client's first frame is a
//!   [`Subscribe`]. The hub replies [`HubToClient::SubscribeAck`] carrying its
//!   current head sequence, streams catch-up [`HubToClient::EventDelivery`]
//!   frames for every event after the cursor in ascending order, then streams
//!   live deliveries. A configured client secret is checked in the Subscribe
//!   frame. Empty-secret mode is available for local tests.
//! - **Heartbeats.** Both links exchange `Heartbeat` frames on an interval and
//!   drop a peer that goes silent for several intervals.
//! - **Sequencing / idempotency.** The hub is the only sequence authority.
//!   Client commands go through [`Hub::append_command`] (idempotent by
//!   `CommandId`); node events go through [`Hub::append_node_event`]
//!   (de-duplicated by `EventId` before a sequence is assigned).
//!
//! # Public surface
//!
//! [`HubState`] holds the guarded [`Hub`], the shared node secret, the live
//! event bus, the node sender registry, and ephemeral run→node routing. Build
//! it with [`HubState::in_memory`] or [`HubState::open`], turn it into a
//! [`router`], and serve it on a [`tokio::net::TcpListener`] with [`serve`]
//! (or [`spawn`], which binds and drives it for you — tests bind port 0 and
//! learn the address that way).

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Json, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, Duration, Instant};

use chrono::Utc;
use stackhour_core::{Error, Result};
use stackhour_domain::{
    AppendOutcome, AssistantWake, ClientCommand, EntityWrite, Event, EventDraft, EventKind, Hub, HubToClient,
    HubToNode, HubWelcome, NodeHello, NodeId, NodeToHub, NodeWork, ProtocolError, RunId, Subscribe, TaskId,
};
use stackhour_domain::{CommandId, ConnectionStatus, EventId, Node, Run, RunStatus, Task, TaskStatus};

/// How often each link sends a `Heartbeat` frame.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// A peer that sends nothing at all for this long is dropped — i.e. it missed
/// several heartbeat intervals.
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);

/// Capacity of the live-event broadcast channel. This bounds only the *live*
/// fast path: a client whose receiver falls this far behind gets a
/// [`broadcast::error::RecvError::Lagged`], at which point the client loop
/// resynchronizes it from the durable event log (the authority) and resumes
/// live delivery — so lagging costs a re-read, never a permanent gap. Sized
/// generously so the resync path is the rare exception, not the norm.
const EVENT_BUS_CAPACITY: usize = 1024;

/// The node id stamped on events that belong to no single execution node
/// (`task.created`, `approval.resolved`). The event log — not this label — is
/// the authority; it exists only because [`EventDraft`] always names a node.
const HUB_NODE_NAME: &str = "hub";

/// The sentinel task id for events that are not tied to a specific task
/// (`node.connected`/`node.disconnected`, and the fallback for an
/// `InterruptRun` whose run→task binding the hub has forgotten). The nil UUID;
/// the `events` table has no foreign key onto `tasks`, so this is a stable,
/// harmless placeholder. Approval resolutions no longer use it — they carry the
/// resolved approval's real task.
fn system_task() -> TaskId {
    TaskId::from_str("00000000-0000-0000-0000-000000000000").expect("nil uuid parses")
}

/// The [`NodeId`] used for hub-originated, node-agnostic events.
fn hub_node() -> NodeId {
    NodeId::from(HUB_NODE_NAME)
}

/// Ephemeral, in-process routing state — the doc's "runtime signals", never
/// durable. It lets [`ClientCommand::SendUserMessage`] and
/// [`ClientCommand::InterruptRun`] (which do not carry a `node_id`) find the
/// node that owns a run so the hub can dispatch work to it.
#[derive(Default)]
struct Routing {
    /// `run_id -> (owning node, task)` learned when a run is started.
    run: HashMap<RunId, (NodeId, TaskId)>,
    /// `task_id -> most recent owning node`, for messages sent without a run.
    task: HashMap<TaskId, NodeId>,
}

struct NodeConnection {
    id: u64,
    sender: mpsc::UnboundedSender<HubToNode>,
}

/// The shared, durable state of one hub: the guarded event log, the node
/// secret, the live-event bus, the connected-node sender registry, and the
/// ephemeral run→node routing. Cloneable only behind an [`Arc`]; construct with
/// [`HubState::in_memory`] or [`HubState::open`].
pub struct HubState {
    /// The single durable store. `rusqlite` is synchronous, so the guard is a
    /// plain [`std::sync::Mutex`] held only for the brief append/query and
    /// **never** across an `.await` on a socket.
    hub: Mutex<Hub>,
    /// The shared secret a node must present in its [`NodeHello`] token.
    node_secret: String,
    /// The secret required in the first client Subscribe frame. Empty means
    /// local unauthenticated mode.
    client_secret: String,
    /// Live bus of newly-appended events, fanned out to subscribed clients.
    events: broadcast::Sender<Event>,
    /// Senders into each connected node's write loop, keyed by node id.
    nodes: Mutex<HashMap<NodeId, NodeConnection>>,
    next_node_connection_id: AtomicU64,
    /// Serializes node registration/removal with the short worker scheduling
    /// critical section. A node may disconnect immediately after dispatch,
    /// but it cannot disappear between eligibility selection and the durable
    /// command bundle being queued for replay.
    execution_gate: Mutex<()>,
    /// Ephemeral run→node routing.
    routing: Mutex<Routing>,
}

/// A safe node snapshot for the control-panel API.
#[derive(Clone, Debug, Serialize)]
pub struct NodeView {
    pub id: String,
    pub label: String,
    pub status: String,
    pub software_version: String,
    pub capabilities: serde_json::Value,
    pub last_seen_sequence: Option<i64>,
}

const ASSISTANT_SETTINGS_KEY: &str = "assistant.settings";
const UPDATE_SETTINGS_KEY: &str = "update.settings";
const CLAIRE_PERSONALITY: &str = "You are Claire, Nikita's personal operations assistant. \
You are warm, composed, candid, lightly witty, and economical with words. You remember context, \
take ownership of follow-through, and distinguish clearly between what you know, what you inferred, \
and what you changed. You coordinate Stackhour tasks and coding agents; do not pretend work happened \
until a durable Stackhour event confirms it.";

/// Durable configuration for Claire and the worker models she selects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantSettings {
    pub name: String,
    pub personality: String,
    pub engine: String,
    pub claude_model: Option<String>,
    pub codex_model: Option<String>,
    pub reasoning_effort: String,
    pub workspace: Option<String>,
    pub memory_enabled: bool,
    pub memory_command: Option<String>,
    pub memory_dir: Option<String>,
}

/// Durable automatic-update policy. Automatic updates are opt-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSettings {
    pub automatic: bool,
    pub interval_hours: u16,
    pub include_nodes: bool,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        UpdateSettings {
            automatic: false,
            interval_hours: 24,
            include_nodes: true,
        }
    }
}

impl UpdateSettings {
    fn validate(&self) -> std::result::Result<(), String> {
        if !(1..=168).contains(&self.interval_hours) {
            return Err("interval_hours must be between 1 and 168".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
}

impl Default for AssistantSettings {
    fn default() -> Self {
        AssistantSettings {
            name: "Claire".to_string(),
            personality: CLAIRE_PERSONALITY.to_string(),
            engine: "claude".to_string(),
            claude_model: None,
            codex_model: None,
            reasoning_effort: "high".to_string(),
            workspace: None,
            memory_enabled: false,
            memory_command: None,
            memory_dir: None,
        }
    }
}

impl AssistantSettings {
    pub fn model(&self) -> Option<String> {
        match self.engine.as_str() {
            "claude" => self.claude_model.clone(),
            "codex" => self.codex_model.clone(),
            _ => None,
        }
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        validate_text("name", &self.name, 40)?;
        validate_text("personality", &self.personality, 16_000)?;
        if !matches!(self.engine.as_str(), "claude" | "codex") {
            return Err("engine must be claude or codex".to_string());
        }
        if !matches!(self.reasoning_effort.as_str(), "low" | "medium" | "high") {
            return Err("reasoning_effort must be low, medium, or high".to_string());
        }
        for (label, value) in [
            ("claude_model", self.claude_model.as_deref()),
            ("codex_model", self.codex_model.as_deref()),
        ] {
            if let Some(value) = value {
                validate_identifier(label, value)?;
            }
        }
        for (label, value) in [
            ("workspace", self.workspace.as_deref()),
            ("memory_command", self.memory_command.as_deref()),
            ("memory_dir", self.memory_dir.as_deref()),
        ] {
            if let Some(value) = value {
                validate_text(label, value, 1_024)?;
                if !Path::new(value).is_absolute() {
                    return Err(format!("{label} must be an absolute path"));
                }
            }
        }
        if self.memory_enabled && self.memory_command.is_none() {
            return Err("memory_command is required when OptMem is enabled".to_string());
        }
        Ok(())
    }
}

fn validate_text(label: &str, value: &str, max: usize) -> std::result::Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{label} must not be blank"));
    }
    if value.chars().count() > max || value.chars().any(char::is_control) {
        return Err(format!("{label} is invalid or longer than {max} characters"));
    }
    Ok(())
}

fn validate_identifier(label: &str, value: &str) -> std::result::Result<(), String> {
    validate_text(label, value, 200)?;
    if value
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/' | ':')))
    {
        return Err(format!("{label} contains unsupported characters"));
    }
    Ok(())
}

/// Durable binding from one assistant channel to its current task and run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantSession {
    pub task_id: TaskId,
    pub run_id: Option<RunId>,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_wake_event_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_follow_up: Option<String>,
    #[serde(default)]
    pub action_follow_up_in_progress: bool,
}

impl HubState {
    /// A hub backed by an in-memory SQLite database (tests, ephemeral use).
    pub fn in_memory(node_secret: impl Into<String>) -> Result<Arc<HubState>> {
        Ok(HubState::from_hub(
            Hub::open_in_memory()?,
            node_secret.into(),
            String::new(),
        ))
    }

    /// An in-memory hub with node and client credentials.
    pub fn in_memory_secured(
        node_secret: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Result<Arc<HubState>> {
        Ok(HubState::from_hub(
            Hub::open_in_memory()?,
            node_secret.into(),
            client_secret.into(),
        ))
    }

    /// A hub backed by an on-disk SQLite database at `db_path` (created and
    /// migrated if absent).
    pub fn open(db_path: impl AsRef<Path>, node_secret: impl Into<String>) -> Result<Arc<HubState>> {
        Ok(HubState::from_hub(
            Hub::open(db_path)?,
            node_secret.into(),
            String::new(),
        ))
    }

    /// Open a hub that requires credentials for both node and client links.
    pub fn open_secured(
        db_path: impl AsRef<Path>,
        node_secret: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Result<Arc<HubState>> {
        Ok(HubState::from_hub(
            Hub::open(db_path)?,
            node_secret.into(),
            client_secret.into(),
        ))
    }

    fn from_hub(hub: Hub, node_secret: String, client_secret: String) -> Arc<HubState> {
        let (events, _) = broadcast::channel(EVENT_BUS_CAPACITY);
        Arc::new(HubState {
            hub: Mutex::new(hub),
            node_secret,
            client_secret,
            events,
            nodes: Mutex::new(HashMap::new()),
            next_node_connection_id: AtomicU64::new(1),
            execution_gate: Mutex::new(()),
            routing: Mutex::new(Routing::default()),
        })
    }

    // --- durable append + broadcast ---------------------------------------

    /// Run a closure against the single guarded [`Hub`]. The lock is released
    /// when the closure returns; callers must never hold the returned value's
    /// borrow across an `.await` (they don't — every caller does its socket I/O
    /// after this returns).
    fn with_hub<T>(&self, f: impl FnOnce(&mut Hub) -> Result<T>) -> Result<T> {
        let mut guard = self.hub.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    }

    pub fn assistant_settings(&self) -> Result<AssistantSettings> {
        self.with_hub(|hub| match hub.get_setting(ASSISTANT_SETTINGS_KEY)? {
            Some(value) => {
                let settings: AssistantSettings = serde_json::from_value(value)
                    .map_err(|error| Error::msg(format!("bad assistant settings: {error}")))?;
                settings.validate().map_err(Error::msg)?;
                Ok(settings)
            }
            None => Ok(AssistantSettings::default()),
        })
    }

    pub fn save_assistant_settings(&self, settings: &AssistantSettings) -> Result<()> {
        settings.validate().map_err(Error::msg)?;
        self.with_hub(|hub| hub.put_setting(ASSISTANT_SETTINGS_KEY, &serde_json::to_value(settings)?))
    }

    pub fn initialize_assistant_settings(&self, settings: &AssistantSettings) -> Result<()> {
        settings.validate().map_err(Error::msg)?;
        self.with_hub(|hub| {
            if hub.get_setting(ASSISTANT_SETTINGS_KEY)?.is_none() {
                hub.put_setting(ASSISTANT_SETTINGS_KEY, &serde_json::to_value(settings)?)?;
            }
            Ok(())
        })
    }

    pub fn update_settings(&self) -> Result<UpdateSettings> {
        self.with_hub(|hub| match hub.get_setting(UPDATE_SETTINGS_KEY)? {
            Some(value) => {
                let settings: UpdateSettings = serde_json::from_value(value)
                    .map_err(|error| Error::msg(format!("bad update settings: {error}")))?;
                settings.validate().map_err(Error::msg)?;
                Ok(settings)
            }
            None => Ok(UpdateSettings::default()),
        })
    }

    pub fn save_update_settings(&self, settings: &UpdateSettings) -> Result<()> {
        settings.validate().map_err(Error::msg)?;
        self.with_hub(|hub| hub.put_setting(UPDATE_SETTINGS_KEY, &serde_json::to_value(settings)?))
    }

    fn dispatch_update_to_nodes(&self, version: &str) -> usize {
        let expires_at = Utc::now()
            .checked_add_signed(chrono::Duration::minutes(10))
            .expect("ten minute update expiry is representable");
        let senders = self.nodes.lock().unwrap_or_else(|poison| poison.into_inner());
        senders
            .values()
            .filter(|connection| {
                connection
                    .sender
                    .send(HubToNode::DispatchCommand {
                        command_id: CommandId::new(),
                        expires_at: Some(expires_at),
                        work: NodeWork::Update {
                            version: version.to_string(),
                        },
                    })
                    .is_ok()
            })
            .count()
    }

    pub fn assistant_session(&self, channel: &str) -> Result<Option<AssistantSession>> {
        let key = format!("assistant.session.{channel}");
        self.with_hub(|hub| {
            hub.get_setting(&key)?
                .filter(|value| !value.is_null())
                .map(|value| {
                    serde_json::from_value(value)
                        .map_err(|error| Error::msg(format!("bad assistant session: {error}")))
                })
                .transpose()
        })
    }

    pub fn save_assistant_session(&self, channel: &str, session: &AssistantSession) -> Result<()> {
        let key = format!("assistant.session.{channel}");
        self.with_hub(|hub| hub.put_setting(&key, &serde_json::to_value(session)?))
    }

    pub fn active_assistant_channel(&self) -> Result<Option<String>> {
        self.with_hub(|hub| {
            Ok(hub
                .get_setting("assistant.active_channel")?
                .and_then(|value| value.as_str().map(str::to_string))
                .filter(|value| !value.trim().is_empty()))
        })
    }

    pub fn set_active_assistant_channel(&self, channel: &str) -> Result<()> {
        validate_identifier("assistant channel", channel).map_err(Error::msg)?;
        self.with_hub(|hub| hub.put_setting("assistant.active_channel", &Value::String(channel.to_string())))
    }

    pub fn clear_assistant_session(&self, channel: &str) -> Result<()> {
        let key = format!("assistant.session.{channel}");
        self.with_hub(|hub| hub.put_setting(&key, &Value::Null))
    }

    pub fn pending_assistant_wakes(&self, channel: &str) -> Result<Vec<AssistantWake>> {
        self.with_hub(|hub| hub.pending_assistant_wakes(channel))
    }

    pub fn complete_assistant_wake(&self, event_id: &EventId) -> Result<bool> {
        self.with_hub(|hub| hub.complete_assistant_wake(event_id))
    }

    pub fn defer_assistant_wake(&self, event_id: &EventId, error: &str) -> Result<bool> {
        self.with_hub(|hub| hub.defer_assistant_wake(event_id, error))
    }

    pub fn track_assistant_worker(&self, channel: &str, task_id: TaskId, run_id: RunId) -> Result<()> {
        let Some((node_id, bound_task)) = self.run_binding(&run_id) else {
            return Err(Error::msg("cannot track an unknown worker run"));
        };
        if bound_task != task_id || node_id == hub_node() {
            return Err(Error::msg(
                "assistant worker must be a node-owned run for this task",
            ));
        }
        self.with_hub(|hub| hub.register_assistant_worker(channel, task_id))
    }

    /// Start Claire's provider run inside the hub process. This deliberately
    /// bypasses node dispatch: the hub is the only legal execution location
    /// for the persistent assistant.
    pub fn start_hub_assistant_run(
        &self,
        task_id: TaskId,
        engine: &str,
        model: Option<String>,
        reasoning_effort: Option<String>,
        system_prompt: String,
        workspace_path: Option<String>,
    ) -> Result<RunId> {
        validate_run_configuration(
            engine,
            model.as_deref(),
            reasoning_effort.as_deref(),
            Some(&system_prompt),
            workspace_path.as_deref(),
        )
        .map_err(Error::msg)?;
        let run_id = RunId::new();
        let run = Run {
            id: run_id,
            task_id,
            node_id: hub_node(),
            engine: engine.to_string(),
            model: model.clone(),
            reasoning_effort: reasoning_effort.clone(),
            system_prompt: Some(system_prompt),
            access_policy: stackhour_domain::AccessPolicy::Supervised,
            workspace_path: workspace_path.clone(),
            status: RunStatus::Started,
            started_at: Utc::now(),
        };
        let draft = EventDraft::new(EventKind::RunStarted, task_id, hub_node())
            .with_run(run_id)
            .with_payload(json!({
                "engine": engine,
                "model": model,
                "reasoning_effort": reasoning_effort,
                "access_policy": stackhour_domain::AccessPolicy::Supervised,
                "workspace_path": workspace_path,
                "hub_local_assistant": true,
            }));
        self.append_and_broadcast_command(CommandId::new(), draft, EntityWrite::Run(run), None)?;
        Ok(run_id)
    }

    /// Persist a user message addressed to Claire without routing it onto the
    /// node link.
    pub fn append_hub_assistant_message(&self, task_id: TaskId, run_id: RunId, text: &str) -> Result<()> {
        let mut draft = EventDraft::new(EventKind::MessageUser, task_id, hub_node()).with_payload(json!({
            "text": text,
            "client_message_id": CommandId::new().to_string(),
        }));
        draft.run_id = Some(run_id);
        self.append_and_broadcast_command(CommandId::new(), draft, EntityWrite::None, None)?;
        Ok(())
    }

    /// Persist one terminal/output event produced by Claire's hub-local
    /// provider process.
    pub fn append_hub_assistant_event(
        &self,
        kind: EventKind,
        task_id: TaskId,
        run_id: RunId,
        provider_session_id: Option<String>,
        payload: Value,
    ) -> Result<()> {
        if !matches!(
            kind,
            EventKind::MessageAssistantCompleted
                | EventKind::RunCompleted
                | EventKind::RunFailed
                | EventKind::RunInterrupted
        ) {
            return Err(Error::msg("unsupported hub-assistant event kind"));
        }
        let mut draft = EventDraft::new(kind, task_id, hub_node())
            .with_run(run_id)
            .with_payload(payload);
        draft.provider_session_id = provider_session_id;
        self.append_and_broadcast_node_event(EventId::new(), draft)?;
        Ok(())
    }

    /// Append a client command and, iff it created a new event, persist any
    /// durable entity it mints and broadcast that event to subscribed clients.
    ///
    /// `persist` runs only for a genuinely-new command (`created == true`),
    /// inside the same store lock and *before* the broadcast, so the durable
    /// `Task`/`Run` row exists the instant any client observes the event. A
    /// replayed command persists nothing, so a client retry never mints a
    /// duplicate row. The broadcast happens *while the store lock is held*, so
    /// the fan-out order is exactly the sequence order — clients never observe a
    /// reordered or gapped live stream.
    fn append_and_broadcast_command(
        &self,
        command_id: CommandId,
        draft: EventDraft,
        entity: EntityWrite,
        dispatch: Option<&(NodeId, HubToNode)>,
    ) -> Result<AppendOutcome> {
        let events = &self.events;
        self.with_hub(|hub| {
            let queued = dispatch.map(|(node, message)| (node, message));
            let outcome = hub.append_command_bundle(command_id, draft, entity, queued)?;
            if outcome.created {
                if let Some(event) = fetch_event(hub, outcome.sequence)? {
                    let _ = events.send(event);
                }
            }
            Ok(outcome)
        })
    }

    /// Append a node event (de-duplicated by `event_id`) and, iff new,
    /// broadcast it. Same in-lock broadcast discipline as
    /// [`Self::append_and_broadcast_command`].
    fn append_and_broadcast_node_event(&self, event_id: EventId, draft: EventDraft) -> Result<AppendOutcome> {
        let events = &self.events;
        self.with_hub(|hub| {
            let outcome = hub.append_node_event(event_id, draft)?;
            if outcome.created {
                if let Some(event) = fetch_event(hub, outcome.sequence)? {
                    let _ = events.send(event);
                }
            }
            Ok(outcome)
        })
    }

    /// The ascending durable event tail with `sequence > after`. The durable
    /// log is the authority a lagging client resynchronizes against when its
    /// live broadcast receiver overflows.
    fn events_after(&self, after: i64) -> Result<Vec<Event>> {
        self.with_hub(|hub| hub.events_after(after))
    }

    /// Read the durable event tail for an in-process client such as Telegram.
    pub fn read_events_after(&self, after: i64) -> Result<Vec<Event>> {
        self.events_after(after)
    }

    pub fn read_recent_events(&self, limit: usize) -> Result<Vec<Event>> {
        self.with_hub(|hub| hub.recent_events(limit))
    }

    pub fn task_events(&self, task_id: TaskId) -> Result<Vec<Event>> {
        self.with_hub(|hub| hub.task_events_tail(&task_id, 200))
    }

    /// Apply and route one command from an in-process client.
    pub fn submit_command(&self, command: ClientCommand) -> HubToClient {
        let effect = self.apply_command(command);
        if let Some((node_id, work)) = effect.dispatch.clone() {
            self.route_to_node(&node_id, work);
        }
        effect.receipt
    }

    /// List known nodes. The live socket registry overrides stale database
    /// connection state after a hub restart or an unclean node exit.
    pub fn list_nodes(&self) -> Result<Vec<NodeView>> {
        let connected = self.nodes.lock().unwrap_or_else(|p| p.into_inner());
        self.with_hub(|hub| {
            hub.list_nodes().map(|nodes| {
                nodes
                    .into_iter()
                    .map(|node| NodeView {
                        status: if connected.contains_key(&node.id) {
                            "connected".to_string()
                        } else {
                            "disconnected".to_string()
                        },
                        id: node.id.to_string(),
                        label: node.label,
                        software_version: node.software_version,
                        capabilities: node.capabilities,
                        last_seen_sequence: node.last_seen_sequence,
                    })
                    .collect()
            })
        })
    }

    /// Select a currently connected node that advertises `engine` and has not
    /// opted out of task dispatch. A preferred target is an eligibility
    /// constraint, not a hint: the hub never silently sends explicitly
    /// targeted work somewhere else.
    fn eligible_execution_node(
        &self,
        engine: &str,
        preferred: Option<&NodeId>,
    ) -> std::result::Result<NodeId, ProtocolError> {
        let connected = self.nodes.lock().unwrap_or_else(|p| p.into_inner());
        let nodes = self
            .with_hub(|hub| hub.list_nodes())
            .map_err(|_| ProtocolError::InvalidRequest {
                message: "cannot read execution-node registry".to_string(),
            })?;
        nodes
            .into_iter()
            .find(|node| {
                preferred.is_none_or(|wanted| wanted == &node.id)
                    && connected.contains_key(&node.id)
                    && node_accepts_engine(&node.capabilities, engine)
            })
            .map(|node| node.id)
            .ok_or_else(|| ProtocolError::NoEligibleNode {
                engine: engine.to_string(),
                node_id: preferred.cloned(),
            })
    }

    /// Resolve a worker target through the same active/capability scheduler
    /// enforced by [`ClientCommand::StartRun`]. In-process hub clients such as
    /// Claire use this before creating a task so an unschedulable request does
    /// not leave an orphan open task.
    pub fn schedule_execution_node(&self, engine: &str, preferred: Option<&NodeId>) -> Result<NodeId> {
        self.eligible_execution_node(engine, preferred)
            .map_err(|error| Error::msg(error.to_string()))
    }

    /// Create, start, and prompt a worker while node membership is stable.
    ///
    /// The durable node dispatches remain replayable if the socket dies after
    /// this critical section. Holding the same gate used by register/remove
    /// closes the former check-then-act window that could create an open task
    /// and then reject its run solely because the selected node disappeared.
    #[allow(clippy::too_many_arguments)]
    pub fn create_worker_task(
        &self,
        title: String,
        prompt: String,
        assistant_channel: Option<String>,
        preferred_node: Option<NodeId>,
        engine: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
        workspace_path: Option<String>,
    ) -> Result<(TaskId, RunId)> {
        validate_run_configuration(
            &engine,
            model.as_deref(),
            reasoning_effort.as_deref(),
            None,
            workspace_path.as_deref(),
        )
        .map_err(Error::msg)?;
        let _gate = self.execution_gate.lock().unwrap_or_else(|p| p.into_inner());
        let node_id = self
            .eligible_execution_node(&engine, preferred_node.as_ref())
            .map_err(|error| Error::msg(error.to_string()))?;

        let created = self.apply_command_inner(
            ClientCommand::CreateTask {
                command_id: CommandId::new(),
                title,
            },
            None,
        );
        let task_event = self.event_from_effect(created)?;
        let task_id = task_event.task_id;
        if let Some(channel) = assistant_channel.as_deref() {
            self.with_hub(|hub| hub.register_assistant_worker(channel, task_id))?;
        }

        let started = self.apply_command_inner(
            ClientCommand::StartRun {
                command_id: CommandId::new(),
                task_id,
                node_id: Some(node_id.clone()),
                engine,
                model,
                reasoning_effort,
                system_prompt: None,
                access_policy: stackhour_domain::AccessPolicy::Supervised,
                workspace_path,
            },
            Some(node_id),
        );
        let run_event = self.event_from_effect_without_routing(&started)?;
        let run_id = run_event
            .run_id
            .ok_or_else(|| Error::msg("hub did not assign a worker run"))?;
        self.route_effect(started)?;

        let prompted = self.apply_command_inner(
            ClientCommand::SendUserMessage {
                command_id: CommandId::new(),
                task_id,
                run_id: Some(run_id),
                text: prompt,
                client_message_id: CommandId::new().to_string(),
            },
            None,
        );
        self.route_effect(prompted)?;
        Ok((task_id, run_id))
    }

    fn event_from_effect(&self, effect: CommandEffect) -> Result<Event> {
        let sequence = self.route_effect(effect)?;
        self.read_events_after(sequence - 1)?
            .into_iter()
            .find(|event| event.sequence == sequence)
            .ok_or_else(|| Error::msg("accepted command event is missing"))
    }

    fn event_from_effect_without_routing(&self, effect: &CommandEffect) -> Result<Event> {
        let sequence = match &effect.receipt {
            HubToClient::CommandReceipt {
                accepted: true,
                assigned_sequence: Some(sequence),
                ..
            } => *sequence,
            HubToClient::CommandReceipt { error, .. } => {
                return Err(Error::msg(format!("hub rejected command: {error:?}")));
            }
            _ => return Err(Error::msg("hub returned an invalid command receipt")),
        };
        self.read_events_after(sequence - 1)?
            .into_iter()
            .find(|event| event.sequence == sequence)
            .ok_or_else(|| Error::msg("accepted command event is missing"))
    }

    fn route_effect(&self, effect: CommandEffect) -> Result<i64> {
        if let Some((node_id, work)) = effect.dispatch {
            self.route_to_node(&node_id, work);
        }
        match effect.receipt {
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

    /// The current head sequence and the ascending catch-up tail after `after`.
    fn snapshot(&self, after: i64) -> Result<(i64, Vec<Event>)> {
        self.with_hub(|hub| {
            let all = hub.events_after(0)?;
            let head = all.last().map(|e| e.sequence).unwrap_or(0);
            let catch_up = all.into_iter().filter(|e| e.sequence > after).collect();
            Ok((head, catch_up))
        })
    }

    // --- client command application ---------------------------------------

    /// Turn a [`ClientCommand`] into its durable [`EventDraft`], append it
    /// (broadcasting when new), and produce the receipt plus any node dispatch.
    ///
    /// Dispatch is emitted only for a genuinely-new command: a replayed command
    /// returns the original receipt and dispatches nothing, so a client retry is
    /// a true no-op beyond re-acknowledgement.
    fn apply_command(&self, cmd: ClientCommand) -> CommandEffect {
        let _gate = matches!(cmd, ClientCommand::StartRun { .. })
            .then(|| self.execution_gate.lock().unwrap_or_else(|p| p.into_inner()));
        self.apply_command_inner(cmd, None)
    }

    fn apply_command_inner(&self, cmd: ClientCommand, reserved_node: Option<NodeId>) -> CommandEffect {
        let command_id = cmd.command_id();
        if let Ok(Some(outcome)) = self.with_hub(|hub| hub.command_outcome(&command_id)) {
            return CommandEffect {
                receipt: HubToClient::accepted(command_id, outcome.sequence, outcome.event_id),
                dispatch: None,
            };
        }
        match cmd {
            ClientCommand::CreateTask { title, .. } => {
                // Mint the task id up front so the durable `Task` row and the
                // `task.created` event carry exactly one shared id.
                let task = Task {
                    id: TaskId::new(),
                    title: title.clone(),
                    status: TaskStatus::Open,
                    created_at: Utc::now(),
                };
                let draft = EventDraft::new(EventKind::TaskCreated, task.id, hub_node())
                    .with_payload(json!({ "title": title }));
                self.finish_with_entity(command_id, draft, None, EntityWrite::Task(task))
            }

            ClientCommand::SendUserMessage {
                task_id,
                run_id,
                text,
                client_message_id,
                ..
            } => {
                // Resolve the owning node: prefer the run's node, fall back to
                // the task's most recent node.
                let node = run_id
                    .and_then(|r| self.node_for_run(&r))
                    .or_else(|| self.node_for_task(&task_id));
                if node.as_ref().is_some_and(|node| node == &hub_node()) {
                    return CommandEffect {
                        receipt: HubToClient::rejected(
                            command_id,
                            ProtocolError::InvalidRequest {
                                message:
                                    "hub-local assistant messages must be submitted by their owning supervisor"
                                        .to_string(),
                            },
                        ),
                        dispatch: None,
                    };
                }
                let dispatch = match (node.clone(), run_id) {
                    (Some(n), Some(r)) => Some((
                        n,
                        HubToNode::DispatchCommand {
                            command_id,
                            expires_at: None,
                            work: NodeWork::SendPrompt {
                                task_id,
                                run_id: r,
                                text: text.clone(),
                                client_message_id: client_message_id.clone(),
                            },
                        },
                    )),
                    _ => None,
                };
                let mut draft =
                    EventDraft::new(EventKind::MessageUser, task_id, node.unwrap_or_else(hub_node))
                        .with_payload(json!({ "text": text, "client_message_id": client_message_id }));
                draft.run_id = run_id;
                self.finish(command_id, draft, dispatch)
            }

            ClientCommand::StartRun {
                task_id,
                node_id,
                engine,
                model,
                reasoning_effort,
                system_prompt,
                access_policy,
                workspace_path,
                ..
            } => {
                if let Err(message) = validate_run_configuration(
                    &engine,
                    model.as_deref(),
                    reasoning_effort.as_deref(),
                    system_prompt.as_deref(),
                    workspace_path.as_deref(),
                ) {
                    return CommandEffect {
                        receipt: HubToClient::rejected(command_id, ProtocolError::InvalidRequest { message }),
                        dispatch: None,
                    };
                }
                let node_id = match reserved_node
                    .map(Ok)
                    .unwrap_or_else(|| self.eligible_execution_node(&engine, node_id.as_ref()))
                {
                    Ok(node_id) => node_id,
                    Err(error) => {
                        return CommandEffect {
                            receipt: HubToClient::rejected(command_id, error),
                            dispatch: None,
                        };
                    }
                };
                let run_id = RunId::new();
                // The durable `Run` row shares the id the hub stamps on the
                // `run.started` event and dispatches to the node.
                let run = Run {
                    id: run_id,
                    task_id,
                    node_id: node_id.clone(),
                    engine: engine.clone(),
                    model: model.clone(),
                    reasoning_effort: reasoning_effort.clone(),
                    system_prompt: system_prompt.clone(),
                    access_policy,
                    workspace_path: workspace_path.clone(),
                    status: RunStatus::Started,
                    started_at: Utc::now(),
                };
                let dispatch = Some((
                    node_id.clone(),
                    HubToNode::DispatchCommand {
                        command_id,
                        expires_at: None,
                        work: NodeWork::StartRun {
                            run_id,
                            task_id,
                            engine: engine.clone(),
                            model: model.clone(),
                            reasoning_effort: reasoning_effort.clone(),
                            system_prompt,
                            access_policy,
                            workspace_path: workspace_path.clone(),
                        },
                    },
                ));
                let draft = EventDraft::new(EventKind::RunStarted, task_id, node_id.clone())
                    .with_run(run_id)
                    .with_payload(json!({
                        "engine": engine,
                        "model": model,
                        "reasoning_effort": reasoning_effort,
                        "access_policy": access_policy,
                        "workspace_path": workspace_path,
                    }));
                let effect = self.finish_with_entity(command_id, draft, dispatch, EntityWrite::Run(run));
                // Only remember the binding for a genuinely-started run.
                if effect.dispatch.is_some() {
                    self.remember_run(run_id, node_id, task_id);
                }
                effect
            }

            ClientCommand::InterruptRun { run_id, .. } => {
                let binding = self.run_binding(&run_id);
                if binding.as_ref().is_some_and(|(node, _)| node == &hub_node()) {
                    return CommandEffect {
                        receipt: HubToClient::rejected(
                            command_id,
                            ProtocolError::InvalidRequest {
                                message:
                                    "hub-local assistant runs must be stopped by their owning supervisor"
                                        .to_string(),
                            },
                        ),
                        dispatch: None,
                    };
                }
                let (node, task_id) = match binding {
                    Some((n, t)) => (Some(n), t),
                    None => (None, system_task()),
                };
                let dispatch = node.clone().map(|n| {
                    (
                        n,
                        HubToNode::DispatchCommand {
                            command_id,
                            expires_at: None,
                            work: NodeWork::InterruptRun { run_id },
                        },
                    )
                });
                let draft =
                    EventDraft::new(EventKind::RunInterrupted, task_id, node.unwrap_or_else(hub_node))
                        .with_run(run_id);
                self.finish(command_id, draft, dispatch)
            }

            ClientCommand::ResolveApproval {
                approval_id,
                decision,
                actor,
                ..
            } => {
                // Resolve the durable `Approval` first (idempotent — a late or
                // duplicate decision returns the first one), so the entity, not
                // just an event, carries the decision and its resolving actor.
                // The `approval.resolved` event is then stamped with the
                // approval's real task/run and the effective decision/actor. An
                // unknown approval is rejected rather than recording a phantom
                // event against a sentinel task. Applying the decision to the
                // node's pending engine request (an `ApprovalDecision` dispatch)
                // is the Phase-2 slice, so no node work is routed here.
                let now = Utc::now();
                match self.with_hub(|hub| hub.resolve_approval(&approval_id, decision, &actor, now)) {
                    Ok(approval) => {
                        let mut draft =
                            EventDraft::new(EventKind::ApprovalResolved, approval.task_id, hub_node())
                                .with_payload(json!({
                                    "approval_id": approval_id,
                                    "decision": approval.decision,
                                    "actor": approval.resolved_by,
                                }));
                        draft.run_id = Some(approval.run_id);
                        self.finish(command_id, draft, None)
                    }
                    Err(_) => CommandEffect {
                        receipt: HubToClient::rejected(
                            command_id,
                            ProtocolError::UnknownApproval { approval_id },
                        ),
                        dispatch: None,
                    },
                }
            }
        }
    }

    /// Append the draft and assemble the [`CommandEffect`], persisting no
    /// durable entity. On the (unexpected) event of a durable-store failure, the
    /// client still receives a non-accepting receipt rather than silence.
    fn finish(
        &self,
        command_id: CommandId,
        draft: EventDraft,
        dispatch: Option<(NodeId, HubToNode)>,
    ) -> CommandEffect {
        self.finish_with_entity(command_id, draft, dispatch, EntityWrite::None)
    }

    /// Like [`Self::finish`], but also persists a durable entity (`Task`/`Run`)
    /// the command mints — but only when the command genuinely created a new
    /// event, so a replay does not insert a second row (see
    /// [`Self::append_and_broadcast_command`]).
    fn finish_with_entity(
        &self,
        command_id: CommandId,
        draft: EventDraft,
        dispatch: Option<(NodeId, HubToNode)>,
        entity: EntityWrite,
    ) -> CommandEffect {
        match self.append_and_broadcast_command(command_id, draft, entity, dispatch.as_ref()) {
            Ok(outcome) => CommandEffect {
                receipt: HubToClient::accepted(command_id, outcome.sequence, outcome.event_id),
                // Suppress dispatch for a replayed (already-applied) command.
                dispatch: if outcome.created { dispatch } else { None },
            },
            Err(_) => CommandEffect {
                receipt: HubToClient::CommandReceipt {
                    command_id,
                    accepted: false,
                    assigned_sequence: None,
                    event_id: None,
                    error: None,
                },
                dispatch: None,
            },
        }
    }

    // --- node handshake ---------------------------------------------------

    /// Validate a [`NodeHello`]: constant-time token check first, then protocol
    /// version negotiation. A bad token is [`ProtocolError::Unauthenticated`];
    /// a good token with the wrong version is
    /// [`ProtocolError::VersionMismatch`].
    fn negotiate_node(&self, hello: &NodeHello) -> HubWelcome {
        if !constant_time_eq::constant_time_eq(hello.token.as_bytes(), self.node_secret.as_bytes()) {
            HubWelcome::reject(ProtocolError::Unauthenticated)
        } else {
            HubWelcome::negotiate(hello.protocol_version)
        }
    }

    fn accepts_client(&self, token: Option<&str>) -> bool {
        self.client_secret.is_empty()
            || token.is_some_and(|candidate| {
                constant_time_eq::constant_time_eq(candidate.as_bytes(), self.client_secret.as_bytes())
            })
    }

    // --- node registry + routing ------------------------------------------

    fn register_node(&self, node_id: NodeId, tx: mpsc::UnboundedSender<HubToNode>) -> u64 {
        let _gate = self.execution_gate.lock().unwrap_or_else(|p| p.into_inner());
        let connection_id = self.next_node_connection_id.fetch_add(1, Ordering::Relaxed);
        self.nodes.lock().unwrap_or_else(|p| p.into_inner()).insert(
            node_id,
            NodeConnection {
                id: connection_id,
                sender: tx,
            },
        );
        connection_id
    }

    fn unregister_node(&self, node_id: &NodeId, connection_id: u64) -> bool {
        let _gate = self.execution_gate.lock().unwrap_or_else(|p| p.into_inner());
        let mut nodes = self.nodes.lock().unwrap_or_else(|p| p.into_inner());
        let owns_slot = nodes
            .get(node_id)
            .is_some_and(|current| current.id == connection_id);
        if owns_slot {
            nodes.remove(node_id);
        }
        owns_slot
    }

    /// Route a hub→node message to the named node's write loop. Returns whether
    /// a connected node received it; an unknown or disconnected node is a silent
    /// drop (Phase-1 has no offline queue).
    fn route_to_node(&self, node_id: &NodeId, msg: HubToNode) -> bool {
        let guard = self.nodes.lock().unwrap_or_else(|p| p.into_inner());
        match guard.get(node_id) {
            Some(connection) => connection.sender.send(msg).is_ok(),
            None => false,
        }
    }

    fn replay_pending(&self, node_id: &NodeId) {
        let pending = self
            .with_hub(|hub| hub.pending_dispatches(node_id))
            .unwrap_or_default();
        for item in pending {
            self.route_to_node(node_id, item.message);
        }
    }

    fn acknowledge_dispatch_from(&self, node_id: &NodeId, command_id: &CommandId) {
        let _ = self.with_hub(|hub| hub.acknowledge_dispatch_from(command_id, node_id));
    }

    fn remember_run(&self, run_id: RunId, node_id: NodeId, task_id: TaskId) {
        let mut r = self.routing.lock().unwrap_or_else(|p| p.into_inner());
        r.run.insert(run_id, (node_id.clone(), task_id));
        r.task.insert(task_id, node_id);
    }

    fn run_binding(&self, run_id: &RunId) -> Option<(NodeId, TaskId)> {
        self.run_binding_checked(run_id).ok().flatten()
    }

    fn run_binding_checked(&self, run_id: &RunId) -> Result<Option<(NodeId, TaskId)>> {
        let cached = self
            .routing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .run
            .get(run_id)
            .cloned();
        if cached.is_some() {
            return Ok(cached);
        }
        let Some(run) = self.with_hub(|hub| hub.get_run(run_id))? else {
            return Ok(None);
        };
        let binding = (run.node_id.clone(), run.task_id);
        self.remember_run(run.id, run.node_id, run.task_id);
        Ok(Some(binding))
    }

    fn node_for_run(&self, run_id: &RunId) -> Option<NodeId> {
        self.run_binding(run_id).map(|(n, _)| n)
    }

    fn node_for_task(&self, task_id: &TaskId) -> Option<NodeId> {
        self.routing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .task
            .get(task_id)
            .cloned()
    }

    fn accepts_node_event(&self, authenticated_node: &NodeId, draft: &EventDraft) -> Result<bool> {
        if &draft.node_id != authenticated_node {
            return Ok(false);
        }
        let Some(run_id) = draft.run_id else {
            return Ok(false);
        };
        Ok(self
            .run_binding_checked(&run_id)?
            .is_some_and(|(node_id, task_id)| node_id == *authenticated_node && task_id == draft.task_id))
    }

    // --- node lifecycle events --------------------------------------------

    /// Record a node's arrival: upsert its identity snapshot as `Connected`,
    /// persisting the last-seen cursor the node advertised in its
    /// [`NodeHello::resume_after_sequence`], and append + broadcast a
    /// `node.connected` event.
    ///
    /// The node dials in with `resume_after_sequence` set to the highest hub
    /// sequence it had processed (Phase-1 nodes send `None` — an honest fresh
    /// start — because the node link does not echo hub sequences to the node, so
    /// there is no cursor it can honestly claim). Whatever it advertises is
    /// stored as the durable last-seen cursor. There is no node-facing event
    /// replay in the frozen Phase-1 transport: the hub does not stream the
    /// durable log back down the node link, and hub-side UUID de-duplication of
    /// node events makes any replay after a reconnect harmless, so "catching the
    /// node up" is exactly recording this cursor.
    fn on_node_connected(&self, hello: &NodeHello) {
        let node = Node {
            id: hello.node_id.clone(),
            label: hello.node_id.to_string(),
            status: ConnectionStatus::Connected,
            software_version: hello.software_version.clone(),
            capabilities: hello.capabilities.clone(),
            last_seen_sequence: hello.resume_after_sequence,
        };
        let _ = self.with_hub(|hub| hub.upsert_node(&node));
        let draft = EventDraft::new(EventKind::NodeConnected, system_task(), hello.node_id.clone())
            .with_payload(json!({
                "software_version": hello.software_version,
                "capabilities": hello.capabilities,
            }));
        let _ = self.append_and_broadcast_node_event(EventId::new(), draft);
    }

    /// Record a node's departure: flip its stored status to `Disconnected`
    /// (preserving the rest of its identity) and append + broadcast a
    /// `node.disconnected` event.
    fn on_node_disconnected(&self, node_id: &NodeId) {
        let _ = self.with_hub(|hub| {
            if let Some(mut node) = hub.get_node(node_id)? {
                node.status = ConnectionStatus::Disconnected;
                hub.upsert_node(&node)?;
            }
            Ok(())
        });
        let draft = EventDraft::new(EventKind::NodeDisconnected, system_task(), node_id.clone());
        let _ = self.append_and_broadcast_node_event(EventId::new(), draft);
    }
}

fn validate_run_configuration(
    engine: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    system_prompt: Option<&str>,
    workspace_path: Option<&str>,
) -> std::result::Result<(), String> {
    validate_identifier("engine", engine)?;
    if let Some(model) = model {
        validate_identifier("model", model)?;
    }
    if let Some(effort) = reasoning_effort {
        if !matches!(effort, "low" | "medium" | "high") {
            return Err("reasoning_effort must be low, medium, or high".to_string());
        }
    }
    if let Some(prompt) = system_prompt {
        if prompt.is_empty() || prompt.len() > 128 * 1024 || prompt.contains('\0') {
            return Err("system_prompt is empty, too large, or contains NUL".to_string());
        }
    }
    if let Some(path) = workspace_path {
        if !Path::new(path).is_absolute() || path.contains('\0') {
            return Err("workspace_path must be an absolute path".to_string());
        }
    }
    Ok(())
}

fn node_accepts_engine(capabilities: &Value, engine: &str) -> bool {
    if capabilities.get("accepts_tasks").and_then(Value::as_bool) == Some(false) {
        return false;
    }
    capabilities
        .get("engines")
        .and_then(Value::as_array)
        .is_some_and(|engines| engines.iter().any(|value| value.as_str() == Some(engine)))
        || capabilities.get(engine).and_then(Value::as_bool) == Some(true)
}

/// The result of applying one [`ClientCommand`]: the receipt to return to the
/// issuing client, and any hub→node work to route. The resulting event, when
/// new, has already been broadcast to all subscribers.
struct CommandEffect {
    receipt: HubToClient,
    dispatch: Option<(NodeId, HubToNode)>,
}

/// Fetch the single event with this exact sequence from the store.
fn fetch_event(hub: &Hub, sequence: i64) -> Result<Option<Event>> {
    Ok(hub
        .events_after(sequence - 1)?
        .into_iter()
        .find(|e| e.sequence == sequence))
}

// ===========================================================================
// Router + server
// ===========================================================================

/// Build the hub's [`axum`] router over a shared [`HubState`]. Exposes the two
/// frozen endpoints; nothing else.
pub fn router(state: Arc<HubState>) -> Router {
    Router::new()
        .route("/", get(control_panel))
        .route("/health", get(health))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/tasks/start", post(start_task))
        .route(
            "/v1/settings/assistant",
            get(get_assistant_settings).put(put_assistant_settings),
        )
        .route(
            "/v1/settings/update",
            get(get_update_settings).put(put_update_settings),
        )
        .route("/v1/admin/update", get(check_update).post(apply_update))
        .route("/v1/admin/install", post(install_node))
        .route("/v1/node/connect", get(node_connect))
        .route("/v1/client/connect", get(client_connect))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct StartTaskRequest {
    title: String,
    prompt: String,
    node_id: Option<NodeId>,
    engine: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    workspace_path: Option<String>,
}

async fn start_task(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(request): Json<StartTaskRequest>,
) -> Response {
    if !state.accepts_client(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid client token"})),
        )
            .into_response();
    }
    match state.create_worker_task(
        request.title,
        request.prompt,
        state.active_assistant_channel().ok().flatten(),
        request.node_id,
        request.engine,
        request.model,
        request.reasoning_effort,
        request.workspace_path,
    ) {
        Ok((task_id, run_id)) => Json(json!({"task_id": task_id, "run_id": run_id})).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(json!({"error": error.message()}))).into_response(),
    }
}

async fn get_assistant_settings(State(state): State<Arc<HubState>>, headers: HeaderMap) -> Response {
    if !state.accepts_client(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid client token"})),
        )
            .into_response();
    }
    match state.assistant_settings() {
        Ok(settings) => Json(json!({"assistant": settings})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.message()})),
        )
            .into_response(),
    }
}

async fn put_assistant_settings(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(settings): Json<AssistantSettings>,
) -> Response {
    if !state.accepts_client(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid client token"})),
        )
            .into_response();
    }
    match state.save_assistant_settings(&settings) {
        Ok(()) => Json(json!({"ok": true, "assistant": settings})).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error.message()}))).into_response(),
    }
}

async fn get_update_settings(State(state): State<Arc<HubState>>, headers: HeaderMap) -> Response {
    if !state.accepts_client(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid client token"})),
        )
            .into_response();
    }
    match state.update_settings() {
        Ok(settings) => Json(json!({
            "updates": settings,
            "current_version": stackhour_core::VERSION,
        }))
        .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.message()})),
        )
            .into_response(),
    }
}

async fn put_update_settings(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(settings): Json<UpdateSettings>,
) -> Response {
    if !state.accepts_client(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid client token"})),
        )
            .into_response();
    }
    match state.save_update_settings(&settings) {
        Ok(()) => Json(json!({"ok": true, "updates": settings})).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error.message()}))).into_response(),
    }
}

async fn check_update(State(state): State<Arc<HubState>>, headers: HeaderMap) -> Response {
    if !state.accepts_client(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid client token"})),
        )
            .into_response();
    }
    match tokio::task::spawn_blocking(check_official_update).await {
        Ok(Ok(info)) => Json(json!({"update": info})).into_response(),
        Ok(Err(error)) => (StatusCode::BAD_GATEWAY, Json(json!({"error": error.message()}))).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("update check failed: {error}")})),
        )
            .into_response(),
    }
}

#[derive(Clone, Debug, Deserialize)]
struct UpdateRequest {
    #[serde(default)]
    include_nodes: Option<bool>,
}

async fn apply_update(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(request): Json<UpdateRequest>,
) -> Response {
    if !state.accepts_client(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid client token"})),
        )
            .into_response();
    }
    let include_nodes = request
        .include_nodes
        .or_else(|| {
            state
                .update_settings()
                .ok()
                .map(|settings| settings.include_nodes)
        })
        .unwrap_or(true);
    match tokio::task::spawn_blocking(move || trigger_update(&state, include_nodes)).await {
        Ok(Ok(result)) => (StatusCode::ACCEPTED, Json(result)).into_response(),
        Ok(Err(error)) => (StatusCode::BAD_GATEWAY, Json(json!({"error": error.message()}))).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("update request failed: {error}")})),
        )
            .into_response(),
    }
}

fn check_official_update() -> Result<UpdateInfo> {
    let executable =
        std::env::current_exe().map_err(|error| Error::msg(format!("cannot locate stackhour: {error}")))?;
    let output = Command::new(executable)
        .args(["control", "update", "--check", "--json"])
        .output()
        .map_err(|error| Error::msg(format!("cannot start update check: {error}")))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::msg(if error.is_empty() {
            "update check failed".to_string()
        } else {
            error
        }));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| Error::msg(format!("update check returned invalid data: {error}")))
}

fn trigger_update(state: &HubState, include_nodes: bool) -> Result<Value> {
    let info = check_official_update()?;
    if !info.update_available {
        return Ok(json!({
            "ok": true,
            "scheduled": false,
            "message": format!("Stackhour {} is already current.", info.current_version),
            "update": info,
        }));
    }
    let nodes = if include_nodes {
        state.dispatch_update_to_nodes(&info.latest_version)
    } else {
        0
    };
    schedule_local_update(&info.latest_version)?;
    Ok(json!({
        "ok": true,
        "scheduled": true,
        "nodes_scheduled": nodes,
        "message": format!("Stackhour {} update scheduled.", info.latest_version),
        "update": info,
    }))
}

fn schedule_local_update(version: &str) -> Result<()> {
    if version.is_empty()
        || version.len() > 80
        || version
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+')))
    {
        return Err(Error::msg("official update version is invalid"));
    }
    let executable =
        std::env::current_exe().map_err(|error| Error::msg(format!("cannot locate stackhour: {error}")))?;
    let target = format!("--target-version={version}");
    let status = if cfg!(target_os = "linux") {
        Command::new("systemd-run")
            .args([
                "--user",
                "--collect",
                "--quiet",
                "--unit=stackhour-control-update-hub",
            ])
            .arg(executable)
            .args(["control", "update", "--role=all", &target])
            .status()
    } else if cfg!(target_os = "macos") {
        let label = format!("stackhour-control-update-hub-{}", std::process::id());
        Command::new("launchctl")
            .args(["submit", "-l", &label, "--"])
            .arg(executable)
            .args(["control", "update", "--role=all", &target])
            .status()
    } else {
        return Err(Error::msg("in-app updates require Linux or macOS"));
    }
    .map_err(|error| Error::msg(format!("cannot schedule update: {error}")))?;
    if !status.success() {
        return Err(Error::msg(format!(
            "cannot schedule update: service manager exited {status}"
        )));
    }
    Ok(())
}

async fn health() -> impl IntoResponse {
    "ok"
}

async fn control_panel() -> Html<&'static str> {
    Html(CONTROL_PANEL)
}

const CONTROL_PANEL: &str = include_str!("../assets/control.html");

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
}

async fn list_nodes(State(state): State<Arc<HubState>>, headers: HeaderMap) -> Response {
    if !state.accepts_client(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid client token"})),
        )
            .into_response();
    }
    match state.list_nodes() {
        Ok(nodes) => Json(json!({"nodes": nodes})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.message()})),
        )
            .into_response(),
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InstallMode {
    Local,
    Ssh,
}

#[derive(Clone, Debug, Deserialize)]
struct InstallRequest {
    mode: InstallMode,
    node_id: String,
    hub_url: String,
    workspace: Option<String>,
    claude_bin: Option<String>,
    codex_bin: Option<String>,
    host: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity: Option<String>,
}

fn safe_identifier(label: &str, value: &str) -> std::result::Result<(), String> {
    if value.trim().is_empty()
        || value.len() > 200
        || value
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\'' | '"' | '`' | '$' | ';' | '|' | '&'))
    {
        return Err(format!("{label} contains unsafe characters"));
    }
    Ok(())
}

fn push_option(args: &mut Vec<String>, name: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        args.push(format!("--{name}={value}"));
    }
}

fn install_args(request: &InstallRequest) -> std::result::Result<Vec<String>, String> {
    safe_identifier("node id", &request.node_id)?;
    if !request.hub_url.starts_with("ws://") && !request.hub_url.starts_with("wss://") {
        return Err("hub URL must use ws or wss".to_string());
    }
    let mut args = vec![
        "control".to_string(),
        "install".to_string(),
        match request.mode {
            InstallMode::Local => "node".to_string(),
            InstallMode::Ssh => "ssh".to_string(),
        },
        format!("--hub-url={}", request.hub_url),
        format!("--id={}", request.node_id),
    ];
    push_option(&mut args, "workspace", request.workspace.as_deref());
    push_option(&mut args, "claude-bin", request.claude_bin.as_deref());
    push_option(&mut args, "codex-bin", request.codex_bin.as_deref());
    if matches!(request.mode, InstallMode::Ssh) {
        let host = request
            .host
            .as_deref()
            .ok_or_else(|| "SSH host is required".to_string())?;
        safe_identifier("SSH host", host)?;
        args.push(format!("--host={host}"));
        if let Some(user) = request.user.as_deref() {
            safe_identifier("SSH user", user)?;
            args.push(format!("--user={user}"));
        }
        if let Some(port) = request.port {
            if port == 0 {
                return Err("SSH port must be between 1 and 65535".to_string());
            }
            args.push(format!("--port={port}"));
        }
        push_option(&mut args, "identity", request.identity.as_deref());
    }
    Ok(args)
}

async fn install_node(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(request): Json<InstallRequest>,
) -> Response {
    if !state.accepts_client(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid client token"})),
        )
            .into_response();
    }
    let args = match install_args(&request) {
        Ok(args) => args,
        Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response(),
    };
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("cannot locate stackhour: {error}")})),
            )
                .into_response()
        }
    };
    let node_secret = state.node_secret.clone();
    let result = tokio::task::spawn_blocking(move || {
        Command::new(executable)
            .args(args)
            .env("STACKHOUR_CONTROL_NODE_TOKEN", node_secret)
            .output()
    })
    .await;
    match result {
        Ok(Ok(output)) if output.status.success() => {
            let message = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Json(json!({"ok": true, "message": message})).into_response()
        }
        Ok(Ok(output)) => {
            let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": if error.is_empty() { "installation failed" } else { &error }})),
            )
                .into_response()
        }
        Ok(Err(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("cannot start installer: {error}")})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("installer task failed: {error}")})),
        )
            .into_response(),
    }
}

/// Serve the hub on an already-bound listener until the process ends.
pub async fn serve(state: Arc<HubState>, listener: TcpListener) -> Result<()> {
    let updater = tokio::spawn(automatic_update_loop(state.clone()));
    let result = axum::serve(listener, router(state))
        .await
        .map_err(|e| Error::msg(e.to_string()));
    updater.abort();
    result
}

async fn automatic_update_loop(state: Arc<HubState>) {
    loop {
        let settings = state.update_settings().unwrap_or_default();
        if settings.automatic {
            let update_state = state.clone();
            let include_nodes = settings.include_nodes;
            let _ = tokio::task::spawn_blocking(move || trigger_update(&update_state, include_nodes)).await;
            tokio::time::sleep(Duration::from_secs(u64::from(settings.interval_hours) * 60 * 60)).await;
        } else {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

/// Bind `addr`, learn the actual [`SocketAddr`] (so callers can pass port 0),
/// and drive the server on a background task. Returns the bound address and the
/// task handle.
pub async fn spawn(state: Arc<HubState>, addr: impl ToSocketAddrs) -> Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let _ = serve(state, listener).await;
    });
    Ok((local, handle))
}

async fn node_connect(ws: WebSocketUpgrade, State(state): State<Arc<HubState>>) -> Response {
    ws.on_upgrade(move |socket| handle_node(socket, state))
}

async fn client_connect(ws: WebSocketUpgrade, State(state): State<Arc<HubState>>) -> Response {
    ws.on_upgrade(move |socket| handle_client(socket, state))
}

// ===========================================================================
// Frame helpers
// ===========================================================================

/// Serialize a protocol value into a single JSON text frame. Protocol types are
/// infallible to serialize.
fn to_frame<T: Serialize>(value: &T) -> Message {
    Message::Text(
        serde_json::to_string(value)
            .expect("protocol value serializes")
            .into(),
    )
}

/// Parse a JSON text frame into a protocol value, or `None` if it is not a text
/// frame or does not deserialize.
fn parse_text<T: DeserializeOwned>(msg: &Message) -> Option<T> {
    match msg {
        Message::Text(t) => serde_json::from_str(t.as_str()).ok(),
        _ => None,
    }
}

/// Read the next text frame from a split stream and parse it as `T`, skipping
/// control ping/pong frames. `None` on close, a non-text frame, or a parse
/// failure — the caller treats any of these as a failed opening handshake.
async fn recv_json<T: DeserializeOwned>(stream: &mut SplitStream<WebSocket>) -> Option<T> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(t.as_str()).ok(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            _ => return None,
        }
    }
}

/// A heartbeat/liveness ticker whose *first* tick is delayed by one interval,
/// so a fresh connection does not emit a heartbeat before any real traffic.
fn heartbeat_ticker() -> tokio::time::Interval {
    interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL)
}

// ===========================================================================
// Client link
// ===========================================================================

/// Drive one client connection: `Subscribe` → `SubscribeAck` → ascending
/// catch-up → live delivery, while accepting `ClientCommand`s and exchanging
/// heartbeats.
async fn handle_client(socket: WebSocket, state: Arc<HubState>) {
    let (mut sink, mut stream) = socket.split();

    // Subscribe to the live bus *before* taking the catch-up snapshot, so an
    // event appended during catch-up is buffered here and cannot be lost. The
    // `sequence > head` filter below drops the ones already sent as catch-up,
    // so there is neither a gap nor a duplicate.
    let mut live = state.events.subscribe();

    let subscribe: Subscribe = match recv_json(&mut stream).await {
        Some(s) => s,
        None => return,
    };
    if !state.accepts_client(subscribe.token.as_deref()) {
        let _ = sink.send(Message::Close(None)).await;
        return;
    }
    let after = subscribe.after_sequence.unwrap_or(0);

    let (head, catch_up) = match state.snapshot(after) {
        Ok(x) => x,
        Err(_) => return,
    };

    let ack = HubToClient::SubscribeAck {
        after_sequence: subscribe.after_sequence,
        head_sequence: head,
    };
    if sink.send(to_frame(&ack)).await.is_err() {
        return;
    }
    for event in catch_up {
        if sink
            .send(to_frame(&HubToClient::EventDelivery { event }))
            .await
            .is_err()
        {
            return;
        }
    }

    let mut ticker = heartbeat_ticker();
    let mut last_seen = Instant::now();
    // The highest sequence delivered to this client so far. Catch-up already
    // sent everything through `head`, so live delivery resumes strictly after
    // it, and a resync (below) re-reads the durable log from exactly here.
    let mut last_delivered = head;

    'client: loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(msg)) => {
                        last_seen = Instant::now();
                        if matches!(msg, Message::Close(_)) {
                            break;
                        }
                        if let Some(cmd) = parse_text::<ClientCommand>(&msg) {
                            let effect = state.apply_command(cmd);
                            if sink.send(to_frame(&effect.receipt)).await.is_err() {
                                break;
                            }
                            if let Some((node_id, work)) = effect.dispatch {
                                state.route_to_node(&node_id, work);
                            }
                        }
                    }
                    _ => break,
                }
            }
            event = live.recv() => {
                match event {
                    Ok(event) => {
                        // Skip anything already delivered (catch-up or a prior
                        // resync); deliver the rest in strict sequence order.
                        if event.sequence > last_delivered {
                            last_delivered = event.sequence;
                            if sink
                                .send(to_frame(&HubToClient::EventDelivery { event }))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // RESYNC: this client fell behind the bounded live bus,
                        // so the broadcast dropped events for it. The durable log
                        // is the authority — re-read the tail after the last
                        // sequence we delivered, stream the gap in order, and
                        // resume live delivery. The client never keeps a hole.
                        match state.events_after(last_delivered) {
                            Ok(missed) => {
                                for event in missed {
                                    if event.sequence <= last_delivered {
                                        continue;
                                    }
                                    last_delivered = event.sequence;
                                    if sink
                                        .send(to_frame(&HubToClient::EventDelivery { event }))
                                        .await
                                        .is_err()
                                    {
                                        break 'client;
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = ticker.tick() => {
                if last_seen.elapsed() > LIVENESS_TIMEOUT {
                    break;
                }
                if sink.send(to_frame(&HubToClient::Heartbeat)).await.is_err() {
                    break;
                }
            }
        }
    }
}

// ===========================================================================
// Node link
// ===========================================================================

/// Drive one node connection: validate the [`NodeHello`], register the node's
/// sender, record `node.connected`, then relay dispatched work to the node and
/// its events back into the durable log until the socket drops.
async fn handle_node(socket: WebSocket, state: Arc<HubState>) {
    let (mut sink, mut stream) = socket.split();

    let hello: NodeHello = match recv_json(&mut stream).await {
        Some(h) => h,
        None => return,
    };

    let welcome = state.negotiate_node(&hello);
    let accepted = welcome.accepted;
    if sink.send(to_frame(&welcome)).await.is_err() {
        return;
    }
    if !accepted {
        let _ = sink.send(Message::Close(None)).await;
        return;
    }

    let node_id = hello.node_id.clone();
    let (tx, mut node_rx) = mpsc::unbounded_channel::<HubToNode>();
    let connection_id = state.register_node(node_id.clone(), tx);
    state.on_node_connected(&hello);
    state.replay_pending(&node_id);

    let mut ticker = heartbeat_ticker();
    let mut last_seen = Instant::now();

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(msg)) => {
                        last_seen = Instant::now();
                        if matches!(msg, Message::Close(_)) {
                            break;
                        }
                        if let Some(from_node) = parse_text::<NodeToHub>(&msg) {
                            match from_node {
                                NodeToHub::NodeEvent { event_id, draft } => {
                                    match state.accepts_node_event(&node_id, &draft) {
                                        Ok(false) => {
                                            eprintln!(
                                                "control hub rejected invalid event {event_id} from {node_id}"
                                            );
                                            state.route_to_node(
                                                &node_id,
                                                HubToNode::EventAck { event_id },
                                            );
                                        }
                                        Ok(true) => {
                                            if state
                                                .append_and_broadcast_node_event(event_id, draft)
                                                .is_ok()
                                            {
                                                state.route_to_node(
                                                    &node_id,
                                                    HubToNode::EventAck { event_id },
                                                );
                                            }
                                        }
                                        Err(error) => {
                                            eprintln!(
                                                "control hub could not validate event {event_id} from {node_id}: {}",
                                                error.message()
                                            );
                                        }
                                    }
                                }
                                NodeToHub::CommandAck { command_id } => {
                                    state.acknowledge_dispatch_from(&node_id, &command_id);
                                }
                                NodeToHub::Heartbeat => {}
                            }
                        }
                    }
                    _ => break,
                }
            }
            outbound = node_rx.recv() => {
                match outbound {
                    Some(work) => {
                        if sink.send(to_frame(&work)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = ticker.tick() => {
                if last_seen.elapsed() > LIVENESS_TIMEOUT {
                    break;
                }
                if sink.send(to_frame(&HubToNode::Heartbeat)).await.is_err() {
                    break;
                }
            }
        }
    }

    if state.unregister_node(&node_id, connection_id) {
        state.on_node_disconnected(&node_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use stackhour_domain::{AccessPolicy, Decision, WIRE_PROTOCOL_VERSION};
    use tower::ServiceExt;

    fn state() -> Arc<HubState> {
        HubState::in_memory("s3cret").expect("in-memory hub")
    }

    // --- token / version negotiation --------------------------------------

    #[test]
    fn good_token_and_version_is_accepted() {
        let s = state();
        let welcome = s.negotiate_node(&hello("laptop", "s3cret", WIRE_PROTOCOL_VERSION));
        assert!(welcome.accepted);
        assert_eq!(welcome.error, None);
    }

    #[test]
    fn bad_token_is_unauthenticated() {
        let s = state();
        let welcome = s.negotiate_node(&hello("laptop", "wrong", WIRE_PROTOCOL_VERSION));
        assert!(!welcome.accepted);
        assert_eq!(welcome.error, Some(ProtocolError::Unauthenticated));
    }

    #[test]
    fn good_token_wrong_version_is_version_mismatch() {
        let s = state();
        let bad = WIRE_PROTOCOL_VERSION + 1;
        let welcome = s.negotiate_node(&hello("laptop", "s3cret", bad));
        assert!(!welcome.accepted);
        assert_eq!(
            welcome.error,
            Some(ProtocolError::VersionMismatch {
                client: bad,
                hub: WIRE_PROTOCOL_VERSION,
            })
        );
    }

    fn hello(node: &str, token: &str, version: i64) -> NodeHello {
        NodeHello {
            node_id: NodeId::from(node),
            token: token.to_string(),
            software_version: "0.1.0".to_string(),
            protocol_version: version,
            capabilities: json!({ "acp": true }),
            resume_after_sequence: None,
        }
    }

    fn connect_test_node(
        state: &HubState,
        node: &str,
        capabilities: Value,
    ) -> mpsc::UnboundedReceiver<HubToNode> {
        let mut hello = hello(node, "s3cret", WIRE_PROTOCOL_VERSION);
        hello.capabilities = capabilities;
        let (sender, receiver) = mpsc::unbounded_channel();
        state.register_node(hello.node_id.clone(), sender);
        state.on_node_connected(&hello);
        receiver
    }

    // --- command application ----------------------------------------------

    #[test]
    fn create_task_appends_one_event_at_sequence_one() {
        let s = state();
        let effect = s.apply_command(ClientCommand::CreateTask {
            command_id: CommandId::new(),
            title: "build the thing".to_string(),
        });
        match effect.receipt {
            HubToClient::CommandReceipt {
                accepted,
                assigned_sequence,
                ..
            } => {
                assert!(accepted);
                assert_eq!(assigned_sequence, Some(1));
            }
            other => panic!("wrong receipt: {other:?}"),
        }
        // The event landed durably, exactly once.
        let (head, all) = s.snapshot(0).expect("snapshot");
        assert_eq!(head, 1);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].kind, EventKind::TaskCreated);
    }

    #[test]
    fn replaying_a_command_id_reuses_the_sequence_and_dispatches_nothing() {
        let s = state();
        let cmd = CommandId::new();
        let make = || ClientCommand::CreateTask {
            command_id: cmd,
            title: "t".to_string(),
        };

        let first = s.apply_command(make());
        let second = s.apply_command(make());

        let seq = |r: &HubToClient| match r {
            HubToClient::CommandReceipt {
                assigned_sequence, ..
            } => *assigned_sequence,
            _ => None,
        };
        assert_eq!(seq(&first.receipt), Some(1));
        assert_eq!(seq(&second.receipt), Some(1));

        // Only one durable event exists after the replay.
        let (head, all) = s.snapshot(0).expect("snapshot");
        assert_eq!(head, 1);
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn replaying_start_run_does_not_recheck_ephemeral_node_eligibility() {
        let state = state();
        let node_id = NodeId::from("laptop");
        let hello = NodeHello {
            node_id: node_id.clone(),
            token: "s3cret".to_string(),
            software_version: "test".to_string(),
            protocol_version: WIRE_PROTOCOL_VERSION,
            capabilities: json!({"engines": ["claude"]}),
            resume_after_sequence: None,
        };
        let (sender, _receiver) = mpsc::unbounded_channel();
        let connection_id = state.register_node(node_id.clone(), sender);
        state.on_node_connected(&hello);
        let command_id = CommandId::new();
        let command = || ClientCommand::StartRun {
            command_id,
            task_id: TaskId::new(),
            node_id: Some(node_id.clone()),
            engine: "claude".to_string(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            access_policy: AccessPolicy::Supervised,
            workspace_path: None,
        };

        let first = state.apply_command(command());
        assert!(first.dispatch.is_some());
        assert!(state.unregister_node(&node_id, connection_id));
        let replay = state.apply_command(command());

        assert!(replay.dispatch.is_none());
        assert!(matches!(
            replay.receipt,
            HubToClient::CommandReceipt {
                accepted: true,
                assigned_sequence: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn stale_socket_cannot_unregister_a_newer_connection() {
        let state = state();
        let node_id = NodeId::from("laptop");
        let (old_sender, _old_receiver) = mpsc::unbounded_channel();
        let (new_sender, _new_receiver) = mpsc::unbounded_channel();
        let old_connection = state.register_node(node_id.clone(), old_sender);
        state.register_node(node_id.clone(), new_sender);

        assert!(!state.unregister_node(&node_id, old_connection));
        assert!(state.nodes.lock().unwrap().contains_key(&node_id));
    }

    #[test]
    fn start_run_routes_work_and_remembers_the_binding() {
        let s = state();
        let node = NodeId::from("laptop");
        let _node_rx = connect_test_node(&s, "laptop", json!({"engines": ["acp"]}));
        let task = TaskId::new();
        let effect = s.apply_command(ClientCommand::StartRun {
            command_id: CommandId::new(),
            task_id: task,
            node_id: Some(node.clone()),
            engine: "acp".to_string(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            access_policy: AccessPolicy::Supervised,
            workspace_path: Some("/w".to_string()),
        });
        // Dispatch targets the named node with a StartRun work item…
        let (dest, work) = effect.dispatch.expect("start_run dispatches");
        assert_eq!(dest, node);
        let run_id = match work {
            HubToNode::DispatchCommand {
                work: NodeWork::StartRun { run_id, .. },
                ..
            } => run_id,
            other => panic!("wrong work: {other:?}"),
        };
        // …and the run→node binding is remembered for later interrupts.
        assert_eq!(s.node_for_run(&run_id), Some(node.clone()));
        assert_eq!(s.node_for_task(&task), Some(node));
    }

    #[test]
    fn start_run_without_a_target_selects_an_eligible_active_node() {
        let state = state();
        let _alpha = connect_test_node(
            &state,
            "alpha",
            json!({"engines": ["claude"], "accepts_tasks": false}),
        );
        let _beta = connect_test_node(&state, "beta", json!({"engines": ["codex", "claude"]}));
        let _gamma = connect_test_node(&state, "gamma", json!({"engines": ["claude"]}));

        let effect = state.apply_command(ClientCommand::StartRun {
            command_id: CommandId::new(),
            task_id: TaskId::new(),
            node_id: None,
            engine: "claude".to_string(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            access_policy: AccessPolicy::Automatic,
            workspace_path: None,
        });

        let (node, _) = effect.dispatch.expect("an eligible node receives the run");
        assert_eq!(node, NodeId::from("beta"));
    }

    #[test]
    fn hub_local_run_rejects_generic_interrupt_without_recording_a_false_stop() {
        let state = state();
        let task_id = TaskId::new();
        let run_id = state
            .start_hub_assistant_run(
                task_id,
                "claude",
                None,
                Some("high".to_string()),
                "system".to_string(),
                None,
            )
            .unwrap();
        let before = state.read_events_after(0).unwrap().len();

        let effect = state.apply_command(ClientCommand::InterruptRun {
            command_id: CommandId::new(),
            run_id,
        });

        assert!(matches!(
            effect.receipt,
            HubToClient::CommandReceipt {
                accepted: false,
                error: Some(ProtocolError::InvalidRequest { .. }),
                ..
            }
        ));
        assert_eq!(state.read_events_after(0).unwrap().len(), before);
    }

    #[test]
    fn node_events_must_match_the_authenticated_node_and_durable_run_binding() {
        let state = state();
        let _receiver = connect_test_node(&state, "alpha", json!({"engines": ["claude"]}));
        let task_id = TaskId::new();
        let effect = state.apply_command(ClientCommand::StartRun {
            command_id: CommandId::new(),
            task_id,
            node_id: Some(NodeId::from("alpha")),
            engine: "claude".to_string(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            access_policy: AccessPolicy::Supervised,
            workspace_path: None,
        });
        let run_id = match effect.dispatch.unwrap().1 {
            HubToNode::DispatchCommand {
                work: NodeWork::StartRun { run_id, .. },
                ..
            } => run_id,
            other => panic!("unexpected dispatch: {other:?}"),
        };
        let valid = EventDraft::new(EventKind::RunCompleted, task_id, NodeId::from("alpha")).with_run(run_id);
        let wrong_connection = EventDraft {
            node_id: NodeId::from("beta"),
            ..valid.clone()
        };
        let wrong_task = EventDraft {
            task_id: TaskId::new(),
            ..valid.clone()
        };

        assert!(state.accepts_node_event(&NodeId::from("alpha"), &valid).unwrap());
        assert!(!state
            .accepts_node_event(&NodeId::from("alpha"), &wrong_connection)
            .unwrap());
        assert!(!state
            .accepts_node_event(&NodeId::from("alpha"), &wrong_task)
            .unwrap());
    }

    #[test]
    fn start_run_rejects_offline_or_incompatible_targets_without_an_event() {
        let state = state();
        let _node = connect_test_node(&state, "online", json!({"engines": ["codex"]}));
        for node_id in [NodeId::from("offline"), NodeId::from("online")] {
            let before = state.read_events_after(0).unwrap().len();
            let effect = state.apply_command(ClientCommand::StartRun {
                command_id: CommandId::new(),
                task_id: TaskId::new(),
                node_id: Some(node_id.clone()),
                engine: "claude".to_string(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                access_policy: AccessPolicy::Automatic,
                workspace_path: None,
            });
            assert!(effect.dispatch.is_none());
            assert!(matches!(
                effect.receipt,
                HubToClient::CommandReceipt {
                    accepted: false,
                    error: Some(ProtocolError::NoEligibleNode { .. }),
                    ..
                }
            ));
            assert_eq!(state.read_events_after(0).unwrap().len(), before);
        }
    }

    #[test]
    fn create_task_persists_a_durable_task_row() {
        let s = state();
        let effect = s.apply_command(ClientCommand::CreateTask {
            command_id: CommandId::new(),
            title: "build the thing".to_string(),
        });
        // The event names a task id; the durable Task row must exist under it.
        let task_id = s
            .snapshot(0)
            .expect("snapshot")
            .1
            .into_iter()
            .find(|e| e.kind == EventKind::TaskCreated)
            .expect("task.created")
            .task_id;
        let task = s
            .with_hub(|hub| hub.get_task(&task_id))
            .unwrap()
            .expect("durable task row exists");
        assert_eq!(task.title, "build the thing");
        assert_eq!(task.status, TaskStatus::Open);
        // Sanity: the receipt was accepting.
        match effect.receipt {
            HubToClient::CommandReceipt { accepted, .. } => assert!(accepted),
            other => panic!("wrong receipt: {other:?}"),
        }
    }

    #[test]
    fn replaying_create_task_does_not_mint_a_second_row() {
        let s = state();
        let cmd = CommandId::new();
        let make = || ClientCommand::CreateTask {
            command_id: cmd,
            title: "once".to_string(),
        };
        s.apply_command(make());
        s.apply_command(make());
        // One event, and exactly one durable task row — the replay minted
        // neither a second event nor an orphan task.
        let (_, all) = s.snapshot(0).expect("snapshot");
        let created: Vec<_> = all.iter().filter(|e| e.kind == EventKind::TaskCreated).collect();
        assert_eq!(created.len(), 1);
        assert!(s
            .with_hub(|hub| hub.get_task(&created[0].task_id))
            .unwrap()
            .is_some());
    }

    #[test]
    fn start_run_persists_a_durable_run_row() {
        let s = state();
        let node = NodeId::from("laptop");
        let _node_rx = connect_test_node(&s, "laptop", json!({"engines": ["acp"]}));
        let task = TaskId::new();
        let effect = s.apply_command(ClientCommand::StartRun {
            command_id: CommandId::new(),
            task_id: task,
            node_id: Some(node.clone()),
            engine: "acp".to_string(),
            model: Some("m".to_string()),
            reasoning_effort: Some("high".to_string()),
            system_prompt: Some("system".to_string()),
            access_policy: AccessPolicy::Supervised,
            workspace_path: Some("/w".to_string()),
        });
        let run_id = match effect.dispatch.expect("dispatch").1 {
            HubToNode::DispatchCommand {
                work: NodeWork::StartRun { run_id, .. },
                ..
            } => run_id,
            other => panic!("wrong work: {other:?}"),
        };
        let run = s
            .with_hub(|hub| hub.get_run(&run_id))
            .unwrap()
            .expect("durable run row exists");
        assert_eq!(run.task_id, task);
        assert_eq!(run.node_id, node);
        assert_eq!(run.model.as_deref(), Some("m"));
        assert_eq!(run.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(run.system_prompt.as_deref(), Some("system"));
        assert_eq!(run.access_policy, AccessPolicy::Supervised);
        assert_eq!(run.status, stackhour_domain::RunStatus::Started);
    }

    #[test]
    fn assistant_settings_and_session_survive_database_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hub.db");
        let session = AssistantSession {
            task_id: TaskId::new(),
            run_id: Some(RunId::new()),
            engine: "codex".to_string(),
            provider_session_id: Some("provider-thread".to_string()),
            pending_wake_event_id: None,
            pending_follow_up: None,
            action_follow_up_in_progress: false,
        };
        {
            let state = HubState::open(&path, "secret").unwrap();
            let settings = AssistantSettings {
                engine: "codex".to_string(),
                codex_model: Some("gpt-5.6-luna".to_string()),
                ..AssistantSettings::default()
            };
            state.save_assistant_settings(&settings).unwrap();
            state.save_assistant_session("telegram.1", &session).unwrap();
        }
        let reopened = HubState::open(&path, "secret").unwrap();
        assert_eq!(
            reopened.assistant_settings().unwrap().codex_model.as_deref(),
            Some("gpt-5.6-luna")
        );
        assert_eq!(reopened.assistant_session("telegram.1").unwrap(), Some(session));
    }

    #[test]
    fn assistant_settings_reject_unsafe_models_and_relative_memory_commands() {
        let state = state();
        let mut settings = AssistantSettings {
            codex_model: Some("luna; touch /tmp/x".to_string()),
            ..AssistantSettings::default()
        };
        assert!(state.save_assistant_settings(&settings).is_err());

        settings.codex_model = Some("gpt-5.6-luna".to_string());
        settings.memory_enabled = true;
        settings.memory_command = Some(".optmem/memo".to_string());
        assert!(state.save_assistant_settings(&settings).is_err());
    }

    #[test]
    fn update_settings_are_durable_and_bounded() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hub.db");
        {
            let state = HubState::open(&path, "secret").unwrap();
            state
                .save_update_settings(&UpdateSettings {
                    automatic: true,
                    interval_hours: 6,
                    include_nodes: false,
                })
                .unwrap();
            assert!(state
                .save_update_settings(&UpdateSettings {
                    automatic: true,
                    interval_hours: 0,
                    include_nodes: true,
                })
                .is_err());
        }
        let reopened = HubState::open(&path, "secret").unwrap();
        assert_eq!(
            reopened.update_settings().unwrap(),
            UpdateSettings {
                automatic: true,
                interval_hours: 6,
                include_nodes: false,
            }
        );
    }

    #[test]
    fn invalid_run_configuration_is_rejected_without_an_event_or_dispatch() {
        let state = state();
        let effect = state.apply_command(ClientCommand::StartRun {
            command_id: CommandId::new(),
            task_id: TaskId::new(),
            node_id: Some(NodeId::from("local")),
            engine: "codex".to_string(),
            model: Some("luna;bad".to_string()),
            reasoning_effort: Some("extreme".to_string()),
            system_prompt: None,
            access_policy: AccessPolicy::Supervised,
            workspace_path: None,
        });
        assert!(effect.dispatch.is_none());
        assert!(matches!(
            effect.receipt,
            HubToClient::CommandReceipt {
                accepted: false,
                error: Some(ProtocolError::InvalidRequest { .. }),
                ..
            }
        ));
        assert!(state.read_events_after(0).unwrap().is_empty());
    }

    #[test]
    fn resolve_approval_updates_the_durable_entity_and_records_an_event() {
        let s = state();
        // Seed a task, run, and pending approval directly in the store.
        let (approval_id, task_id, run_id) = s
            .with_hub(|hub| {
                let task = hub.create_task("t")?;
                let run =
                    hub.start_run(&task.id, &NodeId::from("laptop"), "acp", AccessPolicy::Supervised)?;
                let approval = hub.request_approval(
                    &run.id,
                    &task.id,
                    "call-1",
                    "write /etc/hosts",
                    &["allow".to_string(), "deny".to_string()],
                    None,
                )?;
                Ok((approval.id, task.id, run.id))
            })
            .expect("seed approval");

        let effect = s.apply_command(ClientCommand::ResolveApproval {
            command_id: CommandId::new(),
            approval_id,
            decision: Decision::Allowed,
            actor: "nikita".to_string(),
        });
        assert!(effect.dispatch.is_none());
        match effect.receipt {
            HubToClient::CommandReceipt { accepted, .. } => assert!(accepted),
            other => panic!("wrong receipt: {other:?}"),
        }

        // The durable approval now carries the decision and the resolving actor.
        let approval = s
            .with_hub(|hub| hub.get_approval(&approval_id))
            .unwrap()
            .expect("approval row");
        assert_eq!(approval.decision, Decision::Allowed);
        assert_eq!(approval.resolved_by.as_deref(), Some("nikita"));

        // Exactly one approval.resolved event, stamped with the real task/run.
        let (_, all) = s.snapshot(0).expect("snapshot");
        let resolved: Vec<_> = all
            .iter()
            .filter(|e| e.kind == EventKind::ApprovalResolved)
            .collect();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].task_id, task_id);
        assert_eq!(resolved[0].run_id, Some(run_id));
    }

    #[test]
    fn resolving_an_unknown_approval_is_rejected_without_an_event() {
        let s = state();
        let effect = s.apply_command(ClientCommand::ResolveApproval {
            command_id: CommandId::new(),
            approval_id: stackhour_domain::ApprovalId::new(),
            decision: Decision::Allowed,
            actor: "nikita".to_string(),
        });
        assert!(effect.dispatch.is_none());
        match effect.receipt {
            HubToClient::CommandReceipt { accepted, error, .. } => {
                assert!(!accepted);
                assert!(matches!(error, Some(ProtocolError::UnknownApproval { .. })));
            }
            other => panic!("wrong receipt: {other:?}"),
        }
        // No phantom event was recorded against a sentinel task.
        let (_, all) = s.snapshot(0).expect("snapshot");
        assert!(all.is_empty());
    }

    #[test]
    fn secured_hub_accepts_only_the_configured_client_token() {
        let state = HubState::in_memory_secured("node", "client-secret").unwrap();
        assert!(state.accepts_client(Some("client-secret")));
        assert!(!state.accepts_client(Some("wrong")));
        assert!(!state.accepts_client(None));
    }

    #[test]
    fn empty_client_secret_keeps_local_compatibility_mode() {
        let state = HubState::in_memory("node").unwrap();
        assert!(state.accepts_client(None));
        assert!(state.accepts_client(Some("anything")));
    }

    #[test]
    fn panel_has_node_setup_and_no_external_assets() {
        assert!(CONTROL_PANEL.contains("Add a machine"));
        assert!(CONTROL_PANEL.contains("/v1/admin/install"));
        assert!(CONTROL_PANEL.contains("/v1/nodes"));
        assert!(CONTROL_PANEL.contains("Claire always runs on this hub"));
        assert!(!CONTROL_PANEL.contains("assistant-node"));
        assert!(!CONTROL_PANEL.contains("https://"));
        assert!(!CONTROL_PANEL.contains("<script src="));
    }

    fn local_install_request() -> InstallRequest {
        InstallRequest {
            mode: InstallMode::Local,
            node_id: "coordinator".to_string(),
            hub_url: "wss://control.example.com/v1/node/connect".to_string(),
            workspace: Some("/srv/work".to_string()),
            claude_bin: Some("/usr/local/bin/claude".to_string()),
            codex_bin: Some("codex".to_string()),
            host: None,
            user: None,
            port: None,
            identity: None,
        }
    }

    #[test]
    fn local_install_arguments_do_not_contain_a_node_token() {
        let args = install_args(&local_install_request()).unwrap();
        assert_eq!(&args[..3], ["control", "install", "node"]);
        assert!(args.contains(&"--id=coordinator".to_string()));
        assert!(args.contains(&"--workspace=/srv/work".to_string()));
        assert!(!args.iter().any(|arg| arg.contains("token")));
    }

    #[test]
    fn ssh_install_arguments_are_structured_and_validated() {
        let mut request = local_install_request();
        request.mode = InstallMode::Ssh;
        request.host = Some("devbox".to_string());
        request.user = Some("nikita".to_string());
        request.port = Some(2222);
        request.identity = Some("/home/nikita/.ssh/devbox".to_string());
        let args = install_args(&request).unwrap();
        assert_eq!(&args[..3], ["control", "install", "ssh"]);
        assert!(args.contains(&"--host=devbox".to_string()));
        assert!(args.contains(&"--user=nikita".to_string()));
        assert!(args.contains(&"--port=2222".to_string()));

        request.host = Some("devbox; reboot".to_string());
        assert!(install_args(&request).is_err());
    }

    #[test]
    fn install_request_requires_a_websocket_hub_url() {
        let mut request = local_install_request();
        request.hub_url = "https://control.example.com".to_string();
        assert!(install_args(&request).is_err());
    }

    #[tokio::test]
    async fn node_api_requires_the_client_token() {
        let app = router(HubState::in_memory_secured("node", "client").unwrap());
        let response = app
            .oneshot(Request::builder().uri("/v1/nodes").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn assistant_settings_api_requires_auth_and_persists_valid_changes() {
        let state = HubState::in_memory_secured("node", "client").unwrap();
        let unauthorized = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/v1/settings/assistant")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let settings = AssistantSettings {
            engine: "codex".to_string(),
            codex_model: Some("gpt-5.6-luna".to_string()),
            ..AssistantSettings::default()
        };
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/settings/assistant")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer client")
                    .body(Body::from(serde_json::to_vec(&settings).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state.assistant_settings().unwrap().codex_model.as_deref(),
            Some("gpt-5.6-luna")
        );
    }

    #[tokio::test]
    async fn control_panel_tasks_wake_the_active_hub_assistant() {
        let state = HubState::in_memory_secured("node", "client").unwrap();
        state.set_active_assistant_channel("telegram.1").unwrap();
        let mut work = connect_test_node(&state, "worker", json!({"engines": ["codex"]}));
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/tasks/start")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer client")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "title": "Panel task",
                            "prompt": "Do it",
                            "engine": "codex"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let (task_id, run_id) = match work.recv().await.unwrap() {
            HubToNode::DispatchCommand {
                work: NodeWork::StartRun { task_id, run_id, .. },
                ..
            } => (task_id, run_id),
            other => panic!("wrong work: {other:?}"),
        };
        state
            .append_and_broadcast_node_event(
                EventId::new(),
                EventDraft::new(EventKind::RunCompleted, task_id, NodeId::from("worker")).with_run(run_id),
            )
            .unwrap();

        assert_eq!(state.pending_assistant_wakes("telegram.1").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_settings_api_requires_auth_and_persists_policy() {
        let state = HubState::in_memory_secured("node", "client").unwrap();
        let unauthorized = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/v1/settings/update")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let settings = UpdateSettings {
            automatic: true,
            interval_hours: 12,
            include_nodes: true,
        };
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/settings/update")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer client")
                    .body(Body::from(serde_json::to_vec(&settings).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.update_settings().unwrap(), settings);
    }

    #[tokio::test]
    async fn update_admin_api_rejects_bad_auth_before_network_or_processes() {
        for method in ["GET", "POST"] {
            let response = router(HubState::in_memory_secured("node", "client").unwrap())
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/v1/admin/update")
                        .header("content-type", "application/json")
                        .body(if method == "POST" {
                            Body::from("{}")
                        } else {
                            Body::empty()
                        })
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn node_api_returns_live_node_state() {
        let state = HubState::in_memory_secured("node", "client").unwrap();
        let hello = hello("devbox", "node", WIRE_PROTOCOL_VERSION);
        let (tx, _rx) = mpsc::unbounded_channel();
        state.register_node(hello.node_id.clone(), tx);
        state.on_node_connected(&hello);
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/nodes")
                    .header("authorization", "Bearer client")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["nodes"][0]["id"], "devbox");
        assert_eq!(value["nodes"][0]["status"], "connected");
    }

    #[tokio::test]
    async fn install_api_rejects_bad_auth_before_it_starts_a_process() {
        let body = serde_json::to_vec(&json!({
            "mode": "local",
            "node_id": "coordinator",
            "hub_url": "ws://127.0.0.1:4050/v1/node/connect"
        }))
        .unwrap();
        let response = router(HubState::in_memory_secured("node", "client").unwrap())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/admin/install")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer wrong")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn unacknowledged_node_work_is_durable() {
        let state = state();
        let command = CommandId::new();
        let task = TaskId::new();
        let node = NodeId::from("offline");
        let _node_rx = connect_test_node(&state, "offline", json!({"engines": ["claude"]}));
        state.submit_command(ClientCommand::StartRun {
            command_id: command,
            task_id: task,
            node_id: Some(node.clone()),
            engine: "claude".to_string(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            access_policy: AccessPolicy::Automatic,
            workspace_path: None,
        });
        let pending = state.with_hub(|hub| hub.pending_dispatches(&node)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].command_id, command);
    }

    #[test]
    fn acknowledged_node_work_is_not_replayed() {
        let state = state();
        let command = CommandId::new();
        let node = NodeId::from("offline");
        let _node_rx = connect_test_node(&state, "offline", json!({"engines": ["claude"]}));
        state.submit_command(ClientCommand::StartRun {
            command_id: command,
            task_id: TaskId::new(),
            node_id: Some(node.clone()),
            engine: "claude".to_string(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            access_policy: AccessPolicy::Automatic,
            workspace_path: None,
        });
        state.acknowledge_dispatch_from(&NodeId::from("other"), &command);
        assert_eq!(
            state.with_hub(|hub| hub.pending_dispatches(&node)).unwrap().len(),
            1,
            "another authenticated node cannot retire this dispatch"
        );
        state.acknowledge_dispatch_from(&node, &command);
        assert!(state
            .with_hub(|hub| hub.pending_dispatches(&node))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn run_routing_is_restored_after_hub_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hub.db");
        let run_id = {
            let state = HubState::open(&path, "secret").unwrap();
            let _node_rx = connect_test_node(&state, "laptop", json!({"engines": ["codex"]}));
            state.submit_command(ClientCommand::StartRun {
                command_id: CommandId::new(),
                task_id: TaskId::new(),
                node_id: Some(NodeId::from("laptop")),
                engine: "codex".to_string(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                access_policy: AccessPolicy::Automatic,
                workspace_path: None,
            });
            state
                .read_events_after(0)
                .unwrap()
                .into_iter()
                .find_map(|event| event.run_id)
                .unwrap()
        };
        let state = HubState::open(&path, "secret").unwrap();
        let receipt = state.submit_command(ClientCommand::InterruptRun {
            command_id: CommandId::new(),
            run_id,
        });
        assert!(matches!(
            receipt,
            HubToClient::CommandReceipt { accepted: true, .. }
        ));
        let pending = state
            .with_hub(|hub| hub.pending_dispatches(&NodeId::from("laptop")))
            .unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|d| {
            matches!(
                d.message,
                HubToNode::DispatchCommand {
                    work: NodeWork::InterruptRun { .. },
                    ..
                }
            )
        }));
    }
}
