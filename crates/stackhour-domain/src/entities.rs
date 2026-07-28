//! The five durable core entities of the control plane.
//!
//! `Node`, `Task`, `Run`, and `Approval` live here; `Event` (the ordered
//! history row) lives in [`crate::event`]. These are the deliberately small
//! "Initial core entities" from the architecture doc — no `Project`,
//! `Checkout`, `Artifact`, or `TerminalSession` yet.
//!
//! Each enum stores as a short lowercase token (`as_str`) and parses back
//! (`from_db`); the string forms are the on-disk contract for the `store`
//! layer.

use crate::ids::{ApprovalId, NodeId, RunId, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stackhour_core::{Error, Result};

/// Small helper: reject an unknown enum token read back from the DB.
fn unknown(kind: &str, got: &str) -> Error {
    Error::msg(format!("unknown {kind}: {got}"))
}

// --- Node ------------------------------------------------------------------

/// Whether a node's outbound connection to the hub is currently up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// The node's stream is live.
    Connected,
    /// The node is unreachable (asleep, NAT, offline).
    Disconnected,
}

impl ConnectionStatus {
    /// On-disk token.
    pub fn as_str(&self) -> &'static str {
        match self {
            ConnectionStatus::Connected => "connected",
            ConnectionStatus::Disconnected => "disconnected",
        }
    }

    /// Parse an on-disk token.
    pub fn from_db(s: &str) -> Result<Self> {
        match s {
            "connected" => Ok(ConnectionStatus::Connected),
            "disconnected" => Ok(ConnectionStatus::Disconnected),
            other => Err(unknown("connection status", other)),
        }
    }
}

/// An execution location: a laptop or remote-development box that dials out to
/// the hub. Owns local checkouts, Git, terminals, and engine processes; the hub
/// only holds this identity snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Stable, caller-chosen identity, constant across reconnects.
    pub id: NodeId,
    /// Human label for display.
    pub label: String,
    /// Current connection status.
    pub status: ConnectionStatus,
    /// The node's reported software version.
    pub software_version: String,
    /// A small capability snapshot negotiated with the node.
    pub capabilities: Value,
    /// The highest event sequence the node has been caught up to, if any.
    pub last_seen_sequence: Option<i64>,
}

// --- Task ------------------------------------------------------------------

/// Lifecycle of a durable task, independent of any engine process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    /// Created, not yet running.
    Open,
    /// A run is executing.
    Running,
    /// Finished successfully.
    Completed,
    /// Finished in failure.
    Failed,
    /// Interrupted by a client.
    Interrupted,
}

impl TaskStatus {
    /// On-disk token.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Open => "open",
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Interrupted => "interrupted",
        }
    }

    /// Parse an on-disk token.
    pub fn from_db(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(TaskStatus::Open),
            "running" => Ok(TaskStatus::Running),
            "completed" => Ok(TaskStatus::Completed),
            "failed" => Ok(TaskStatus::Failed),
            "interrupted" => Ok(TaskStatus::Interrupted),
            other => Err(unknown("task status", other)),
        }
    }
}

/// The user-visible, durable unit of work shared by every client.
#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    /// Durable identity.
    pub id: TaskId,
    /// User intent / title.
    pub title: String,
    /// Lifecycle status.
    pub status: TaskStatus,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

// --- Run -------------------------------------------------------------------

/// The access policy a run executes under. Starts minimal; richer per-capability
/// grants come after the approval loop is proven.
///
/// The serde spelling is deliberately the same `snake_case` token as
/// [`AccessPolicy::as_str`], so a policy written to the DB and one carried on the
/// wire ([`crate::protocol`]) read identically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessPolicy {
    /// Every sensitive operation needs an approval.
    Supervised,
    /// Pre-authorized operations run without prompting.
    Automatic,
    /// No approval gating (trusted checkout).
    FullAccess,
}

impl AccessPolicy {
    /// On-disk token.
    pub fn as_str(&self) -> &'static str {
        match self {
            AccessPolicy::Supervised => "supervised",
            AccessPolicy::Automatic => "automatic",
            AccessPolicy::FullAccess => "full_access",
        }
    }

    /// Parse an on-disk token.
    pub fn from_db(s: &str) -> Result<Self> {
        match s {
            "supervised" => Ok(AccessPolicy::Supervised),
            "automatic" => Ok(AccessPolicy::Automatic),
            "full_access" => Ok(AccessPolicy::FullAccess),
            other => Err(unknown("access policy", other)),
        }
    }
}

/// Lifecycle of one execution attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    /// Executing.
    Started,
    /// Interrupted by a client.
    Interrupted,
    /// Finished successfully.
    Completed,
    /// Finished in failure.
    Failed,
}

impl RunStatus {
    /// On-disk token.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Started => "started",
            RunStatus::Interrupted => "interrupted",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
        }
    }

    /// Parse an on-disk token.
    pub fn from_db(s: &str) -> Result<Self> {
        match s {
            "started" => Ok(RunStatus::Started),
            "interrupted" => Ok(RunStatus::Interrupted),
            "completed" => Ok(RunStatus::Completed),
            "failed" => Ok(RunStatus::Failed),
            other => Err(unknown("run status", other)),
        }
    }
}

/// One attempt on one node with one configured engine and access policy.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// Durable identity.
    pub id: RunId,
    /// The task this run attempts.
    pub task_id: TaskId,
    /// The node executing the run.
    pub node_id: NodeId,
    /// The engine/agent label (e.g. an ACP agent name).
    pub engine: String,
    /// The access policy in force for this run.
    pub access_policy: AccessPolicy,
    /// The workspace/checkout path the run executes against, if configured. The
    /// architecture doc's "a run may carry a configured workspace path without
    /// introducing first-class Project and Checkout tables" — the checkout
    /// dimension lives here on the durable run, not only in an event payload.
    pub workspace_path: Option<String>,
    /// Lifecycle status.
    pub status: RunStatus,
    /// Start time.
    pub started_at: DateTime<Utc>,
}

// --- Approval --------------------------------------------------------------

/// An approval's decision state.
///
/// The serde spelling matches [`Decision::as_str`] so the wire form
/// ([`crate::protocol`]) and the DB token are one and the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Awaiting a decision from any authorized client.
    Pending,
    /// Granted.
    Allowed,
    /// Refused.
    Denied,
}

impl Decision {
    /// On-disk token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Pending => "pending",
            Decision::Allowed => "allowed",
            Decision::Denied => "denied",
        }
    }

    /// Parse an on-disk token.
    pub fn from_db(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Decision::Pending),
            "allowed" => Ok(Decision::Allowed),
            "denied" => Ok(Decision::Denied),
            other => Err(unknown("decision", other)),
        }
    }
}

/// A durable, actionable approval request. Persisted *before* it is shown in
/// any client, so a decision from Telegram or the web page resolves the same
/// entity — and a late or duplicate decision is a no-op.
#[derive(Clone, Debug, PartialEq)]
pub struct Approval {
    /// Durable identity.
    pub id: ApprovalId,
    /// The run whose engine requested it.
    pub run_id: RunId,
    /// The task the run belongs to.
    pub task_id: TaskId,
    /// The engine's tool-call id this approval gates.
    pub tool_call_id: String,
    /// The requested scope (e.g. a command or path summary).
    pub scope: String,
    /// The options offered to the decider.
    pub options: Vec<String>,
    /// When the request stops being actionable, if ever.
    pub expires_at: Option<DateTime<Utc>>,
    /// The current decision.
    pub decision: Decision,
    /// The actor that resolved it, if resolved.
    pub resolved_by: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Resolution time, if resolved.
    pub resolved_at: Option<DateTime<Utc>>,
}

impl Approval {
    /// Whether this approval is still awaiting a decision.
    pub fn is_pending(&self) -> bool {
        self.decision == Decision::Pending
    }

    /// Whether this approval has passed its expiry while still pending. A
    /// resolved approval is never reported as expired — its decision already
    /// stands.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.is_pending() && matches!(self.expires_at, Some(exp) if now >= exp)
    }
}
