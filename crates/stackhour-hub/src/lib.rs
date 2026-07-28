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
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, Duration, Instant};

use chrono::Utc;
use stackhour_core::{Error, Result};
use stackhour_domain::{
    AppendOutcome, ClientCommand, EntityWrite, Event, EventDraft, EventKind, Hub, HubToClient, HubToNode,
    HubWelcome, NodeHello, NodeId, NodeToHub, NodeWork, ProtocolError, RunId, Subscribe, TaskId,
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
    nodes: Mutex<HashMap<NodeId, mpsc::UnboundedSender<HubToNode>>>,
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
        let command_id = cmd.command_id();
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
                access_policy,
                workspace_path,
                ..
            } => {
                let run_id = RunId::new();
                // The durable `Run` row shares the id the hub stamps on the
                // `run.started` event and dispatches to the node.
                let run = Run {
                    id: run_id,
                    task_id,
                    node_id: node_id.clone(),
                    engine: engine.clone(),
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
                            access_policy,
                            workspace_path: workspace_path.clone(),
                        },
                    },
                ));
                let draft = EventDraft::new(EventKind::RunStarted, task_id, node_id.clone())
                    .with_run(run_id)
                    .with_payload(json!({
                        "engine": engine,
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

    fn register_node(&self, node_id: NodeId, tx: mpsc::UnboundedSender<HubToNode>) {
        self.nodes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(node_id, tx);
    }

    fn unregister_node(&self, node_id: &NodeId) {
        self.nodes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(node_id);
    }

    /// Route a hub→node message to the named node's write loop. Returns whether
    /// a connected node received it; an unknown or disconnected node is a silent
    /// drop (Phase-1 has no offline queue).
    fn route_to_node(&self, node_id: &NodeId, msg: HubToNode) -> bool {
        let guard = self.nodes.lock().unwrap_or_else(|p| p.into_inner());
        match guard.get(node_id) {
            Some(tx) => tx.send(msg).is_ok(),
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

    fn acknowledge_dispatch(&self, command_id: &CommandId) {
        let _ = self.with_hub(|hub| hub.acknowledge_dispatch(command_id));
    }

    fn remember_run(&self, run_id: RunId, node_id: NodeId, task_id: TaskId) {
        let mut r = self.routing.lock().unwrap_or_else(|p| p.into_inner());
        r.run.insert(run_id, (node_id.clone(), task_id));
        r.task.insert(task_id, node_id);
    }

    fn run_binding(&self, run_id: &RunId) -> Option<(NodeId, TaskId)> {
        let cached = self
            .routing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .run
            .get(run_id)
            .cloned();
        if cached.is_some() {
            return cached;
        }
        let run = self.with_hub(|hub| hub.get_run(run_id)).ok().flatten()?;
        let binding = (run.node_id.clone(), run.task_id);
        self.remember_run(run.id, run.node_id, run.task_id);
        Some(binding)
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
        .route("/v1/admin/install", post(install_node))
        .route("/v1/node/connect", get(node_connect))
        .route("/v1/client/connect", get(client_connect))
        .with_state(state)
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
    axum::serve(listener, router(state))
        .await
        .map_err(|e| Error::msg(e.to_string()))
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
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
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
    state.register_node(node_id.clone(), tx);
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
                                    let _ = state.append_and_broadcast_node_event(event_id, draft);
                                }
                                NodeToHub::CommandAck { command_id } => {
                                    state.acknowledge_dispatch(&command_id);
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

    state.unregister_node(&node_id);
    state.on_node_disconnected(&node_id);
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
    fn start_run_routes_work_and_remembers_the_binding() {
        let s = state();
        let node = NodeId::from("laptop");
        let task = TaskId::new();
        let effect = s.apply_command(ClientCommand::StartRun {
            command_id: CommandId::new(),
            task_id: task,
            node_id: node.clone(),
            engine: "acp".to_string(),
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
        let task = TaskId::new();
        let effect = s.apply_command(ClientCommand::StartRun {
            command_id: CommandId::new(),
            task_id: task,
            node_id: node.clone(),
            engine: "acp".to_string(),
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
        assert_eq!(run.access_policy, AccessPolicy::Supervised);
        assert_eq!(run.status, stackhour_domain::RunStatus::Started);
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
    fn disconnected_node_work_is_queued() {
        let state = state();
        let command = CommandId::new();
        let task = TaskId::new();
        let node = NodeId::from("offline");
        state.submit_command(ClientCommand::StartRun {
            command_id: command,
            task_id: task,
            node_id: node.clone(),
            engine: "claude".to_string(),
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
        state.submit_command(ClientCommand::StartRun {
            command_id: command,
            task_id: TaskId::new(),
            node_id: node.clone(),
            engine: "claude".to_string(),
            access_policy: AccessPolicy::Automatic,
            workspace_path: None,
        });
        state.acknowledge_dispatch(&command);
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
            state.submit_command(ClientCommand::StartRun {
                command_id: CommandId::new(),
                task_id: TaskId::new(),
                node_id: NodeId::from("laptop"),
                engine: "codex".to_string(),
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
