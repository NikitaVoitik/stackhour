//! The wire-message contract between clients, the hub, and execution nodes.
//!
//! These are the pure, serde-round-trippable message types that let
//! `stackhour-hub` and `stackhour-node` be built independently. There is no
//! transport, no async, and no I/O here — only the tagged unions and structs
//! that both sides agree on. Every enum is `#[serde(tag = "type",
//! rename_all = "snake_case")]`, so each message carries a stable,
//! human-readable discriminant.
//!
//! The shape follows the architecture doc's
//! `docs/architecture/remote-agent-control-plane.md`:
//!
//! - **"Build now"** — only the Phase-1 verbs and events: create task, send
//!   message, start/interrupt run, resolve approval, and streaming assistant
//!   output. No terminals, worktrees, diffs, subagents, or native engines.
//! - **"Target topology"** — clients and nodes never talk directly; both speak
//!   to one hub. Nodes dial *out* (see [`NodeHello`]), so the hub queues work
//!   with explicit expiry and cancellation ([`HubToNode::DispatchCommand`],
//!   [`HubToNode::CancelCommand`]).
//! - **"Reconnect and idempotency rules"** — a [`ClientCommand`] is idempotent
//!   by its [`CommandId`]; a node event is deduplicated by its [`EventId`]
//!   *before* the hub assigns the one global sequence; clients resume with
//!   `after_sequence` ([`Subscribe`]) and nodes with `resume_after_sequence`
//!   ([`NodeHello`]).
//! - **"Initial event vocabulary"** — the payloads reuse [`crate::event`]'s
//!   [`Event`]/[`EventDraft`] rather than re-modelling history here.
//! - **"ACP mapping"** — the node normalizes ACP traffic into those durable
//!   events at its boundary; this protocol only moves the normalized result.
//!
//! Capability negotiation is required from v1: the handshake rejects a version
//! mismatch (see [`HubWelcome::negotiate`]).

use crate::entities::{AccessPolicy, Decision};
use crate::event::{Event, EventDraft, PROTOCOL_VERSION};
use crate::ids::{ApprovalId, CommandId, EventId, NodeId, RunId, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// The single protocol version this build speaks on the wire.
///
/// It is not a second magic number: it is exactly [`PROTOCOL_VERSION`] (the
/// value stamped on every durable event), and it keeps that value's `i64` type
/// so the handshake never casts across the durable/wire boundary. Bump the one
/// constant in [`crate::event`] and both move together.
pub const WIRE_PROTOCOL_VERSION: i64 = PROTOCOL_VERSION;

// ===========================================================================
// Client <-> Hub
// ===========================================================================

/// A mutation a client asks the hub to perform. Every variant carries a stable
/// [`CommandId`]: replaying the same id is a no-op that returns the original
/// [`HubToClient::CommandReceipt`] rather than producing a second effect.
///
/// This is the Phase-1 verb set only — create a task, send a user message,
/// start or interrupt a run, and resolve an approval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    /// Create a new durable task.
    CreateTask {
        /// Idempotency key.
        command_id: CommandId,
        /// User intent / title.
        title: String,
    },
    /// Add a user message to a task's timeline. `run_id` targets a specific
    /// execution attempt when one is active; `client_message_id` lets the
    /// optimistic client reconcile the echoed durable event.
    SendUserMessage {
        /// Idempotency key.
        command_id: CommandId,
        /// The task the message belongs to.
        task_id: TaskId,
        /// The run it steers, when one is active.
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
        /// The message text.
        text: String,
        /// Client-chosen id used to reconcile the optimistic echo.
        client_message_id: String,
    },
    /// Start a run: one execution attempt on one node with one engine and
    /// access policy.
    StartRun {
        /// Idempotency key.
        command_id: CommandId,
        /// The task to attempt.
        task_id: TaskId,
        /// The node that should execute it.
        node_id: NodeId,
        /// The engine/agent label (e.g. an ACP agent name).
        engine: String,
        /// The access policy the run executes under.
        access_policy: AccessPolicy,
        /// An optional node-local checkout path for the run.
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace_path: Option<String>,
    },
    /// Interrupt an in-flight run.
    InterruptRun {
        /// Idempotency key.
        command_id: CommandId,
        /// The run to interrupt.
        run_id: RunId,
    },
    /// Resolve a durable approval. A late or duplicate decision is idempotent.
    ResolveApproval {
        /// Idempotency key.
        command_id: CommandId,
        /// The approval being decided.
        approval_id: ApprovalId,
        /// The decision.
        decision: Decision,
        /// Who decided (e.g. a Telegram user or web session).
        actor: String,
    },
}

impl ClientCommand {
    /// The idempotency key attached to this command, whatever its variant.
    pub fn command_id(&self) -> CommandId {
        match self {
            ClientCommand::CreateTask { command_id, .. }
            | ClientCommand::SendUserMessage { command_id, .. }
            | ClientCommand::StartRun { command_id, .. }
            | ClientCommand::InterruptRun { command_id, .. }
            | ClientCommand::ResolveApproval { command_id, .. } => *command_id,
        }
    }
}

/// A client's request to receive the task event stream, resuming after a
/// cursor. `after_sequence = None` asks for the whole log from the start; the
/// hub replies with catch-up [`HubToClient::EventDelivery`] rows up to its head
/// (announced by [`HubToClient::SubscribeAck`]) and then switches to live
/// delivery without a gap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subscribe {
    /// The client credential. A local hub can leave this empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The last hub sequence the client already has, or `None` for a full
    /// replay. Omitted from the wire form when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_sequence: Option<i64>,
}

/// A hub-to-client message: an idempotent receipt, a durable event delivery,
/// the subscription acknowledgement, or a liveness pong.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubToClient {
    /// The outcome of a [`ClientCommand`]. On success it carries the assigned
    /// sequence and durable [`EventId`]; on rejection it carries a
    /// [`ProtocolError`]. Replaying the command returns the same receipt.
    CommandReceipt {
        /// The command this answers.
        command_id: CommandId,
        /// Whether the command was accepted.
        accepted: bool,
        /// The hub sequence assigned to the resulting event, when accepted.
        #[serde(skip_serializing_if = "Option::is_none")]
        assigned_sequence: Option<i64>,
        /// The durable event id, when accepted.
        #[serde(skip_serializing_if = "Option::is_none")]
        event_id: Option<EventId>,
        /// Why the command was rejected, when not accepted.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<ProtocolError>,
    },
    /// A durable [`Event`] with its hub-assigned sequence, delivered during
    /// `after_sequence` catch-up or live.
    EventDelivery {
        /// The sequenced durable event.
        event: Event,
    },
    /// Acknowledges a [`Subscribe`]: catch-up rows up to `head_sequence`
    /// precede or accompany this, after which delivery is live.
    SubscribeAck {
        /// The cursor the client asked to resume after (echoed).
        #[serde(skip_serializing_if = "Option::is_none")]
        after_sequence: Option<i64>,
        /// The hub's current head sequence.
        head_sequence: i64,
    },
    /// Liveness pong.
    Heartbeat,
}

impl HubToClient {
    /// An accepting receipt for a command that produced one durable event.
    pub fn accepted(command_id: CommandId, assigned_sequence: i64, event_id: EventId) -> Self {
        HubToClient::CommandReceipt {
            command_id,
            accepted: true,
            assigned_sequence: Some(assigned_sequence),
            event_id: Some(event_id),
            error: None,
        }
    }

    /// A rejecting receipt carrying the reason.
    pub fn rejected(command_id: CommandId, error: ProtocolError) -> Self {
        HubToClient::CommandReceipt {
            command_id,
            accepted: false,
            assigned_sequence: None,
            event_id: None,
            error: Some(error),
        }
    }
}

// ===========================================================================
// Node <-> Hub  (the outbound authenticated node link)
// ===========================================================================

/// A node's opening authentication + capability message. The node dials out,
/// so this is the first frame on a fresh connection. `resume_after_sequence`
/// asks the hub to redeliver dispatched work the node may have missed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeHello {
    /// The node's stable, self-chosen identity.
    pub node_id: NodeId,
    /// An opaque credential/token proving the node may connect.
    pub token: String,
    /// The node's reported software version.
    pub software_version: String,
    /// The protocol version the node speaks, for negotiation.
    pub protocol_version: i64,
    /// A capability snapshot (e.g. which engines/ACP surfaces it can service).
    pub capabilities: Value,
    /// The last hub sequence the node had processed, or `None` for a fresh
    /// start. Omitted from the wire form when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_after_sequence: Option<i64>,
}

/// The hub's answer to a [`NodeHello`]. Always states the hub's protocol
/// version; on rejection it carries the reason (a version mismatch or a failed
/// credential).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HubWelcome {
    /// Whether the node was accepted.
    pub accepted: bool,
    /// The hub's protocol version.
    pub protocol_version: i64,
    /// Why the node was rejected, when not accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
}

impl HubWelcome {
    /// Accept the node at this build's [`WIRE_PROTOCOL_VERSION`].
    pub fn accept() -> Self {
        HubWelcome {
            accepted: true,
            protocol_version: WIRE_PROTOCOL_VERSION,
            error: None,
        }
    }

    /// Reject the node, stating this build's version and the reason.
    pub fn reject(error: ProtocolError) -> Self {
        HubWelcome {
            accepted: false,
            protocol_version: WIRE_PROTOCOL_VERSION,
            error: Some(error),
        }
    }

    /// Negotiate against a node's advertised protocol version: accept when it
    /// matches [`WIRE_PROTOCOL_VERSION`], otherwise reject with
    /// [`ProtocolError::VersionMismatch`]. Capability negotiation is required
    /// from the first protocol version.
    pub fn negotiate(node_version: i64) -> Self {
        if node_version == WIRE_PROTOCOL_VERSION {
            HubWelcome::accept()
        } else {
            HubWelcome::reject(ProtocolError::VersionMismatch {
                client: node_version,
                hub: WIRE_PROTOCOL_VERSION,
            })
        }
    }
}

/// A steady-state message from a node to the hub.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeToHub {
    /// A node-originated event. It carries a stable [`EventId`] so the hub can
    /// deduplicate a retried delivery *before* assigning the one global
    /// sequence; the [`EventDraft`] holds everything else.
    NodeEvent {
        /// Dedup key, minted by the node.
        event_id: EventId,
        /// The event before the hub sequences it.
        draft: EventDraft,
    },
    /// The node accepted a dispatched command. A retry reuses the same
    /// [`CommandId`] rather than creating a second effect.
    CommandAck {
        /// The dispatched command being acknowledged.
        command_id: CommandId,
    },
    /// Liveness.
    Heartbeat,
}

/// A steady-state message from the hub to a node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubToNode {
    /// Work the node must perform, correlated by [`CommandId`] (the node
    /// answers with [`NodeToHub::CommandAck`]). `expires_at` bounds how long
    /// the hub expects the work to matter: a node dequeuing it after that
    /// instant should drop it rather than execute stale work.
    DispatchCommand {
        /// Correlates the dispatch with its ack and any resulting events.
        command_id: CommandId,
        /// When the work stops being valid, if ever.
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<DateTime<Utc>>,
        /// The actual work to perform.
        work: NodeWork,
    },
    /// A durable approval decision the node must apply to the pending engine
    /// request it gates. Verified against the same run/request before it is
    /// answered.
    ApprovalDecision {
        /// The approval being applied.
        approval_id: ApprovalId,
        /// The decision.
        decision: Decision,
        /// Who decided.
        actor: String,
    },
    /// Withdraw previously dispatched work — a queued command the client
    /// cancelled, or an interrupt reaching the node.
    CancelCommand {
        /// The dispatch to cancel.
        command_id: CommandId,
    },
    /// Liveness.
    Heartbeat,
}

/// The concrete work carried by a [`HubToNode::DispatchCommand`]. Phase-1
/// scope: start a run, send a prompt, or interrupt a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeWork {
    /// Begin an execution attempt: open the engine/session for this run.
    StartRun {
        /// The durable run identity assigned by the hub.
        run_id: RunId,
        /// The task the run attempts.
        task_id: TaskId,
        /// The engine/agent to launch.
        engine: String,
        /// The access policy to enforce.
        access_policy: AccessPolicy,
        /// The node-local checkout path, when configured.
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace_path: Option<String>,
    },
    /// Deliver a user prompt to the engine for an active run.
    SendPrompt {
        /// The task the prompt belongs to.
        task_id: TaskId,
        /// The run to prompt.
        run_id: RunId,
        /// The prompt text.
        text: String,
        /// The client message id, for echo reconciliation.
        client_message_id: String,
    },
    /// Interrupt the engine's current turn for a run.
    InterruptRun {
        /// The run to interrupt.
        run_id: RunId,
    },
}

// ===========================================================================
// Shared
// ===========================================================================

/// A protocol-level failure, carried inside a [`HubToClient::CommandReceipt`]
/// or a [`HubWelcome`]. This is the *wire* error vocabulary; it is distinct
/// from the crate's internal `stackhour_core::Error`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProtocolError {
    /// The two ends speak incompatible protocol versions. Rejected at the
    /// handshake ([`HubWelcome::negotiate`]).
    VersionMismatch {
        /// The version the connecting side advertised.
        client: i64,
        /// The version this hub speaks.
        hub: i64,
    },
    /// The credential presented in [`NodeHello`] was missing or invalid.
    Unauthenticated,
    /// A command referenced a task the hub does not know.
    UnknownTask {
        /// The unknown task.
        task_id: TaskId,
    },
    /// A command referenced a run the hub does not know.
    UnknownRun {
        /// The unknown run.
        run_id: RunId,
    },
    /// A command referenced an approval the hub does not know.
    UnknownApproval {
        /// The unknown approval.
        approval_id: ApprovalId,
    },
    /// The referenced work or approval is no longer actionable (past its
    /// expiry).
    Expired,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::VersionMismatch { client, hub } => {
                write!(f, "protocol version mismatch: client {client}, hub {hub}")
            }
            ProtocolError::Unauthenticated => f.write_str("unauthenticated node"),
            ProtocolError::UnknownTask { task_id } => write!(f, "unknown task: {task_id}"),
            ProtocolError::UnknownRun { run_id } => write!(f, "unknown run: {run_id}"),
            ProtocolError::UnknownApproval { approval_id } => {
                write!(f, "unknown approval: {approval_id}")
            }
            ProtocolError::Expired => f.write_str("request expired"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;
    use chrono::{TimeZone, Utc};
    use proptest::prelude::*;
    use serde_json::json;

    /// Serialize, read the `type` discriminant, then round-trip through
    /// `to_string`/`from_str` and assert value-equality.
    fn assert_tag_and_round_trip<T>(value: &T, expected_tag: &str)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let tag = serde_json::to_value(value).unwrap();
        assert_eq!(
            tag.get("type").and_then(Value::as_str),
            Some(expected_tag),
            "discriminant string is the stable, human-readable one",
        );
        let text = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&text).unwrap();
        assert_eq!(&back, value, "round-trip preserved the message");
    }

    fn sample_draft() -> EventDraft {
        let occurred = Utc.with_ymd_and_hms(2026, 7, 25, 12, 0, 0).unwrap();
        let mut d = EventDraft::new(
            EventKind::MessageAssistantDelta,
            TaskId::new(),
            NodeId::from("laptop"),
        )
        .with_run(RunId::new())
        .with_payload(json!({ "text": "hi" }));
        d.provider_session_id = Some("sess-1".to_string());
        d.occurred_at = occurred;
        d
    }

    fn sample_event() -> Event {
        let t = Utc.with_ymd_and_hms(2026, 7, 25, 12, 0, 0).unwrap();
        Event {
            sequence: 7,
            event_id: EventId::new(),
            command_id: Some(CommandId::new()),
            kind: EventKind::MessageUser,
            task_id: TaskId::new(),
            run_id: Some(RunId::new()),
            provider_session_id: Some("sess-1".to_string()),
            node_id: NodeId::from("laptop"),
            protocol_version: PROTOCOL_VERSION,
            occurred_at: t,
            hub_received_at: t,
            payload: json!({ "text": "hello" }),
        }
    }

    proptest! {
        #[test]
        fn subscribe_round_trips_arbitrary_tokens_and_cursors(
            token in proptest::option::of(any::<String>()),
            after_sequence in proptest::option::of(any::<i64>()),
        ) {
            let message = Subscribe {
                token,
                after_sequence,
            };
            let encoded = serde_json::to_vec(&message).expect("serialize");
            let decoded: Subscribe = serde_json::from_slice(&encoded).expect("deserialize");
            prop_assert_eq!(decoded, message);
        }

        #[test]
        fn create_task_round_trips_arbitrary_unicode_titles(title in any::<String>()) {
            let message = ClientCommand::CreateTask {
                command_id: CommandId::new(),
                title,
            };
            let encoded = serde_json::to_vec(&message).expect("serialize");
            let decoded: ClientCommand = serde_json::from_slice(&encoded).expect("deserialize");
            prop_assert_eq!(decoded, message);
        }

        #[test]
        fn malformed_bytes_never_panic_protocol_decoders(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let _ = serde_json::from_slice::<ClientCommand>(&data);
            let _ = serde_json::from_slice::<HubToClient>(&data);
            let _ = serde_json::from_slice::<NodeToHub>(&data);
            let _ = serde_json::from_slice::<HubToNode>(&data);
        }
    }

    // --- the wire version is exactly the durable protocol version ----------

    #[test]
    fn wire_protocol_version_tracks_the_event_protocol_version() {
        assert_eq!(WIRE_PROTOCOL_VERSION, PROTOCOL_VERSION);
        assert_eq!(WIRE_PROTOCOL_VERSION, 1);
    }

    // --- top-level message enums: tag + round-trip -------------------------

    #[test]
    fn client_command_variants_round_trip_with_stable_tags() {
        let cmd = ClientCommand::CreateTask {
            command_id: CommandId::new(),
            title: "build the thing".to_string(),
        };
        assert_tag_and_round_trip(&cmd, "create_task");

        assert_tag_and_round_trip(
            &ClientCommand::SendUserMessage {
                command_id: CommandId::new(),
                task_id: TaskId::new(),
                run_id: Some(RunId::new()),
                text: "go".to_string(),
                client_message_id: "cm-1".to_string(),
            },
            "send_user_message",
        );
        assert_tag_and_round_trip(
            &ClientCommand::StartRun {
                command_id: CommandId::new(),
                task_id: TaskId::new(),
                node_id: NodeId::from("laptop"),
                engine: "acp".to_string(),
                access_policy: AccessPolicy::Supervised,
                workspace_path: Some("/home/nikita/proj".to_string()),
            },
            "start_run",
        );
        assert_tag_and_round_trip(
            &ClientCommand::InterruptRun {
                command_id: CommandId::new(),
                run_id: RunId::new(),
            },
            "interrupt_run",
        );
        assert_tag_and_round_trip(
            &ClientCommand::ResolveApproval {
                command_id: CommandId::new(),
                approval_id: ApprovalId::new(),
                decision: Decision::Allowed,
                actor: "nikita".to_string(),
            },
            "resolve_approval",
        );
    }

    #[test]
    fn client_command_id_accessor_covers_every_variant() {
        let id = CommandId::new();
        let cmds = [
            ClientCommand::CreateTask {
                command_id: id,
                title: "t".to_string(),
            },
            ClientCommand::SendUserMessage {
                command_id: id,
                task_id: TaskId::new(),
                run_id: None,
                text: "x".to_string(),
                client_message_id: "c".to_string(),
            },
            ClientCommand::StartRun {
                command_id: id,
                task_id: TaskId::new(),
                node_id: NodeId::from("n"),
                engine: "acp".to_string(),
                access_policy: AccessPolicy::Automatic,
                workspace_path: None,
            },
            ClientCommand::InterruptRun {
                command_id: id,
                run_id: RunId::new(),
            },
            ClientCommand::ResolveApproval {
                command_id: id,
                approval_id: ApprovalId::new(),
                decision: Decision::Denied,
                actor: "a".to_string(),
            },
        ];
        for c in cmds {
            assert_eq!(c.command_id(), id);
        }
    }

    #[test]
    fn hub_to_client_variants_round_trip_with_stable_tags() {
        assert_tag_and_round_trip(
            &HubToClient::accepted(CommandId::new(), 5, EventId::new()),
            "command_receipt",
        );
        assert_tag_and_round_trip(
            &HubToClient::EventDelivery {
                event: sample_event(),
            },
            "event_delivery",
        );
        assert_tag_and_round_trip(
            &HubToClient::SubscribeAck {
                after_sequence: Some(4),
                head_sequence: 9,
            },
            "subscribe_ack",
        );
        assert_tag_and_round_trip(&HubToClient::Heartbeat, "heartbeat");
    }

    #[test]
    fn node_to_hub_variants_round_trip_with_stable_tags() {
        assert_tag_and_round_trip(
            &NodeToHub::NodeEvent {
                event_id: EventId::new(),
                draft: sample_draft(),
            },
            "node_event",
        );
        assert_tag_and_round_trip(
            &NodeToHub::CommandAck {
                command_id: CommandId::new(),
            },
            "command_ack",
        );
        assert_tag_and_round_trip(&NodeToHub::Heartbeat, "heartbeat");
    }

    #[test]
    fn hub_to_node_variants_round_trip_with_stable_tags() {
        let expires = Utc.with_ymd_and_hms(2026, 7, 25, 13, 0, 0).unwrap();
        assert_tag_and_round_trip(
            &HubToNode::DispatchCommand {
                command_id: CommandId::new(),
                expires_at: Some(expires),
                work: NodeWork::StartRun {
                    run_id: RunId::new(),
                    task_id: TaskId::new(),
                    engine: "acp".to_string(),
                    access_policy: AccessPolicy::Supervised,
                    workspace_path: Some("/w".to_string()),
                },
            },
            "dispatch_command",
        );
        assert_tag_and_round_trip(
            &HubToNode::ApprovalDecision {
                approval_id: ApprovalId::new(),
                decision: Decision::Allowed,
                actor: "nikita".to_string(),
            },
            "approval_decision",
        );
        assert_tag_and_round_trip(
            &HubToNode::CancelCommand {
                command_id: CommandId::new(),
            },
            "cancel_command",
        );
        assert_tag_and_round_trip(&HubToNode::Heartbeat, "heartbeat");
    }

    #[test]
    fn node_work_variants_round_trip_with_stable_tags() {
        assert_tag_and_round_trip(
            &NodeWork::StartRun {
                run_id: RunId::new(),
                task_id: TaskId::new(),
                engine: "acp".to_string(),
                access_policy: AccessPolicy::FullAccess,
                workspace_path: None,
            },
            "start_run",
        );
        assert_tag_and_round_trip(
            &NodeWork::SendPrompt {
                task_id: TaskId::new(),
                run_id: RunId::new(),
                text: "go".to_string(),
                client_message_id: "cm-1".to_string(),
            },
            "send_prompt",
        );
        assert_tag_and_round_trip(&NodeWork::InterruptRun { run_id: RunId::new() }, "interrupt_run");
    }

    #[test]
    fn protocol_error_variants_round_trip_with_stable_tags() {
        assert_tag_and_round_trip(
            &ProtocolError::VersionMismatch { client: 2, hub: 1 },
            "version_mismatch",
        );
        assert_tag_and_round_trip(&ProtocolError::Unauthenticated, "unauthenticated");
        assert_tag_and_round_trip(
            &ProtocolError::UnknownTask {
                task_id: TaskId::new(),
            },
            "unknown_task",
        );
        assert_tag_and_round_trip(&ProtocolError::Expired, "expired");
    }

    // --- the durable payloads survive the wire, DateTime included ----------

    #[test]
    fn event_delivery_preserves_the_full_durable_event() {
        let event = sample_event();
        let msg = HubToClient::EventDelivery { event: event.clone() };
        let back: HubToClient = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        match back {
            HubToClient::EventDelivery { event: got } => assert_eq!(got, event),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn node_event_preserves_the_draft() {
        let draft = sample_draft();
        let msg = NodeToHub::NodeEvent {
            event_id: EventId::new(),
            draft: draft.clone(),
        };
        let back: NodeToHub = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        match back {
            NodeToHub::NodeEvent { draft: got, .. } => assert_eq!(got, draft),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // --- shared enum tokens agree with their as_str DB spellings -----------

    #[test]
    fn decision_and_access_policy_wire_tokens_match_the_db_tokens() {
        for d in [Decision::Pending, Decision::Allowed, Decision::Denied] {
            assert_eq!(serde_json::to_value(d).unwrap(), json!(d.as_str()));
        }
        for p in [
            AccessPolicy::Supervised,
            AccessPolicy::Automatic,
            AccessPolicy::FullAccess,
        ] {
            assert_eq!(serde_json::to_value(p).unwrap(), json!(p.as_str()));
        }
    }

    // --- version-mismatch handshake ----------------------------------------

    #[test]
    fn matching_version_is_accepted() {
        let welcome = HubWelcome::negotiate(WIRE_PROTOCOL_VERSION);
        assert!(welcome.accepted);
        assert_eq!(welcome.protocol_version, WIRE_PROTOCOL_VERSION);
        assert_eq!(welcome.error, None);
    }

    #[test]
    fn mismatched_version_is_rejected_with_a_protocol_error() {
        let node_version = WIRE_PROTOCOL_VERSION + 1;
        let welcome = HubWelcome::negotiate(node_version);
        assert!(!welcome.accepted);
        assert_eq!(welcome.protocol_version, WIRE_PROTOCOL_VERSION);
        assert_eq!(
            welcome.error,
            Some(ProtocolError::VersionMismatch {
                client: node_version,
                hub: WIRE_PROTOCOL_VERSION,
            })
        );

        // And the rejection survives the wire.
        let back: HubWelcome = serde_json::from_str(&serde_json::to_string(&welcome).unwrap()).unwrap();
        assert_eq!(back, welcome);
    }

    // --- optionality: None is omitted, and everything still round-trips ----

    #[test]
    fn subscribe_omits_after_sequence_when_absent() {
        let none = Subscribe {
            token: None,
            after_sequence: None,
        };
        assert_eq!(serde_json::to_string(&none).unwrap(), "{}");
        let back: Subscribe = serde_json::from_str("{}").unwrap();
        assert_eq!(back, none);

        let some = Subscribe {
            token: None,
            after_sequence: Some(12),
        };
        let text = serde_json::to_string(&some).unwrap();
        assert!(text.contains("after_sequence"));
        assert_eq!(serde_json::from_str::<Subscribe>(&text).unwrap(), some);
    }

    #[test]
    fn node_hello_omits_resume_cursor_when_absent() {
        let hello = NodeHello {
            node_id: NodeId::from("laptop"),
            token: "opaque".to_string(),
            software_version: "0.1.0".to_string(),
            protocol_version: WIRE_PROTOCOL_VERSION,
            capabilities: json!({ "acp": true }),
            resume_after_sequence: None,
        };
        let value = serde_json::to_value(&hello).unwrap();
        assert!(value.get("resume_after_sequence").is_none());
        assert_eq!(
            serde_json::from_str::<NodeHello>(&serde_json::to_string(&hello).unwrap()).unwrap(),
            hello
        );

        let resuming = NodeHello {
            resume_after_sequence: Some(99),
            ..hello
        };
        let value = serde_json::to_value(&resuming).unwrap();
        assert_eq!(value.get("resume_after_sequence"), Some(&json!(99)));
    }

    #[test]
    fn accepting_receipt_carries_ids_and_a_rejecting_one_omits_them() {
        let cmd = CommandId::new();
        let ok = HubToClient::accepted(cmd, 5, EventId::new());
        let ok_value = serde_json::to_value(&ok).unwrap();
        assert_eq!(ok_value.get("assigned_sequence"), Some(&json!(5)));
        assert!(ok_value.get("event_id").is_some());
        assert!(ok_value.get("error").is_none());

        let bad = HubToClient::rejected(cmd, ProtocolError::Unauthenticated);
        let bad_value = serde_json::to_value(&bad).unwrap();
        assert!(bad_value.get("assigned_sequence").is_none());
        assert!(bad_value.get("event_id").is_none());
        assert_eq!(
            bad_value.get("error").and_then(|e| e.get("type")),
            Some(&json!("unauthenticated"))
        );

        // Both round-trip.
        for msg in [ok, bad] {
            let back: HubToClient = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn dispatch_command_omits_expiry_when_absent() {
        let msg = HubToNode::DispatchCommand {
            command_id: CommandId::new(),
            expires_at: None,
            work: NodeWork::InterruptRun { run_id: RunId::new() },
        };
        let value = serde_json::to_value(&msg).unwrap();
        assert!(value.get("expires_at").is_none());
        let back: HubToNode = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn protocol_error_displays_readably() {
        assert_eq!(
            ProtocolError::VersionMismatch { client: 2, hub: 1 }.to_string(),
            "protocol version mismatch: client 2, hub 1"
        );
        assert_eq!(ProtocolError::Unauthenticated.to_string(), "unauthenticated node");
    }
}
