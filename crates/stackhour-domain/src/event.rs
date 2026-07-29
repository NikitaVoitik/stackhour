//! The stable event vocabulary and the durable event-log row.
//!
//! [`EventKind`] is *exactly* the doc's "Initial event vocabulary" — no
//! provider packets, no richer activity/plan/file-change types yet. Each
//! variant serializes to its exact dotted string (`task.created`,
//! `message.assistant.delta`, …) so the DB payload and any JSON projection use
//! one spelling.
//!
//! An [`Event`] is one row of ordered task history. A hub assigns its
//! `sequence` and `event_id` (for command-originated events) and its
//! `hub_received_at`; everything else comes from the [`EventDraft`] the caller
//! hands to [`crate::store::Hub`].

use crate::ids::{CommandId, EventId, NodeId, RunId, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol version stamped on every event this crate writes. Capability
/// negotiation across hub/node/client versions is required from the first
/// protocol version, so it is recorded per row rather than assumed.
pub const PROTOCOL_VERSION: i64 = 3;

/// The initial, deliberately small set of durable product events.
///
/// This is a stable *product* vocabulary, not a mirror of any engine's stream.
/// Raw provider payloads belong in a separate versioned diagnostic field; UI
/// projections must not depend on them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    /// A new durable task was created.
    #[serde(rename = "task.created")]
    TaskCreated,
    /// A run began executing on a node.
    #[serde(rename = "run.started")]
    RunStarted,
    /// A run was interrupted by a client command.
    #[serde(rename = "run.interrupted")]
    RunInterrupted,
    /// A run finished successfully.
    #[serde(rename = "run.completed")]
    RunCompleted,
    /// A run ended in failure.
    #[serde(rename = "run.failed")]
    RunFailed,
    /// A user message entered the timeline.
    #[serde(rename = "message.user")]
    MessageUser,
    /// A streaming chunk of an assistant message.
    #[serde(rename = "message.assistant.delta")]
    MessageAssistantDelta,
    /// A completed assistant message.
    #[serde(rename = "message.assistant.completed")]
    MessageAssistantCompleted,
    /// An approval was requested and must be durably visible.
    #[serde(rename = "approval.requested")]
    ApprovalRequested,
    /// An approval was resolved by an actor.
    #[serde(rename = "approval.resolved")]
    ApprovalResolved,
    /// A node's outbound connection came up.
    #[serde(rename = "node.connected")]
    NodeConnected,
    /// A node's outbound connection dropped.
    #[serde(rename = "node.disconnected")]
    NodeDisconnected,
}

impl EventKind {
    /// Every variant, in vocabulary order. The trust anchor for round-trip and
    /// exhaustiveness tests downstream.
    pub const ALL: [EventKind; 12] = [
        EventKind::TaskCreated,
        EventKind::RunStarted,
        EventKind::RunInterrupted,
        EventKind::RunCompleted,
        EventKind::RunFailed,
        EventKind::MessageUser,
        EventKind::MessageAssistantDelta,
        EventKind::MessageAssistantCompleted,
        EventKind::ApprovalRequested,
        EventKind::ApprovalResolved,
        EventKind::NodeConnected,
        EventKind::NodeDisconnected,
    ];

    /// The exact dotted string stored in the DB `kind` column. Kept in lockstep
    /// with the `serde(rename)` spellings (a unit test asserts they agree).
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::TaskCreated => "task.created",
            EventKind::RunStarted => "run.started",
            EventKind::RunInterrupted => "run.interrupted",
            EventKind::RunCompleted => "run.completed",
            EventKind::RunFailed => "run.failed",
            EventKind::MessageUser => "message.user",
            EventKind::MessageAssistantDelta => "message.assistant.delta",
            EventKind::MessageAssistantCompleted => "message.assistant.completed",
            EventKind::ApprovalRequested => "approval.requested",
            EventKind::ApprovalResolved => "approval.resolved",
            EventKind::NodeConnected => "node.connected",
            EventKind::NodeDisconnected => "node.disconnected",
        }
    }

    /// Parse the dotted string back to a variant, or `None` if unknown.
    pub fn from_dotted(s: &str) -> Option<EventKind> {
        EventKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// A caller-supplied event before the hub stamps it with a sequence (and, for
/// command-originated events, an `event_id`) and a receipt time.
///
/// `occurred_at` is the *source* time (when the fact happened at the node or
/// client); the hub records its own receipt time separately when it persists
/// the row.
///
/// It is serde-serializable because a node ships a draft to the hub inside
/// [`crate::protocol::NodeToHub::NodeEvent`] before the hub sequences it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventDraft {
    /// What happened.
    pub kind: EventKind,
    /// The task this event belongs to.
    pub task_id: TaskId,
    /// The run, when the event belongs to an execution attempt.
    pub run_id: Option<RunId>,
    /// The provider (ACP/Codex/Claude) session id, when known.
    pub provider_session_id: Option<String>,
    /// The node that produced or is associated with the event.
    pub node_id: NodeId,
    /// Protocol version of the producer. Defaults to [`PROTOCOL_VERSION`].
    pub protocol_version: i64,
    /// When the fact occurred at the source.
    pub occurred_at: DateTime<Utc>,
    /// A small typed/JSON payload appropriate to the kind.
    pub payload: Value,
}

impl EventDraft {
    /// A minimal draft: no run/provider-session, current source time, default
    /// protocol version, and a null payload. Set the remaining fields directly.
    pub fn new(kind: EventKind, task_id: TaskId, node_id: NodeId) -> Self {
        EventDraft {
            kind,
            task_id,
            run_id: None,
            provider_session_id: None,
            node_id,
            protocol_version: PROTOCOL_VERSION,
            occurred_at: Utc::now(),
            payload: Value::Null,
        }
    }

    /// Attach the owning run.
    pub fn with_run(mut self, run_id: RunId) -> Self {
        self.run_id = Some(run_id);
        self
    }

    /// Attach a payload.
    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }
}

/// One durable, ordered row of task history as stored by the hub.
///
/// `command_id` is present only for command-originated events (a client
/// mutation carrying an idempotency key); node-originated events have `None`.
/// `sequence` is the single hub-assigned global cursor — gapless and starting
/// at 1.
///
/// It is serde-serializable because the hub delivers it to clients inside
/// [`crate::protocol::HubToClient::EventDelivery`], both during `after_sequence`
/// catch-up and live.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The hub-assigned global sequence (gapless, starts at 1).
    pub sequence: i64,
    /// Stable unique id of this row.
    pub event_id: EventId,
    /// The command idempotency key, for command-originated events only.
    pub command_id: Option<CommandId>,
    /// What happened.
    pub kind: EventKind,
    /// The task this event belongs to.
    pub task_id: TaskId,
    /// The run, when the event belongs to an execution attempt.
    pub run_id: Option<RunId>,
    /// The provider session id, when known.
    pub provider_session_id: Option<String>,
    /// The associated node.
    pub node_id: NodeId,
    /// Protocol version of the producer.
    pub protocol_version: i64,
    /// Source time.
    pub occurred_at: DateTime<Utc>,
    /// Hub receipt time.
    pub hub_received_at: DateTime<Utc>,
    /// The event payload.
    pub payload: Value,
}
