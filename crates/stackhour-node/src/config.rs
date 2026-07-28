//! Node configuration and the cooperative shutdown signal.
//!
//! [`NodeConfig`] is the whole knob set the supervisor needs: where to dial,
//! who it claims to be, and the timing of heartbeats and reconnect backoff.
//! Production defaults are baked into [`NodeConfig::new`]; tests shrink the
//! timers so the durable loop is exercised in milliseconds.

use serde_json::{json, Value};
use stackhour_domain::ids::NodeId;
use std::time::Duration;
use tokio::sync::watch;

/// How the node connects, identifies itself, and paces its keep-alive and
/// reconnect behaviour.
///
/// Construct with [`NodeConfig::new`] and adjust with the `with_*` builders.
/// Only `hub_url`, `node_id`, and `token` have no sensible default.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// The hub's node-link WebSocket URL, e.g. `ws://127.0.0.1:8080/v1/node/connect`.
    pub hub_url: String,
    /// This node's stable, self-chosen identity (constant across reconnects).
    pub node_id: NodeId,
    /// The opaque credential presented in [`stackhour_domain::protocol::NodeHello`].
    pub token: String,
    /// The software version advertised in the handshake.
    pub software_version: String,
    /// The capability snapshot advertised in the handshake (engines/ACP surfaces).
    pub capabilities: Value,
    /// How often the node emits a `Heartbeat` frame.
    pub heartbeat_interval: Duration,
    /// How long the node tolerates silence from the hub before treating the
    /// link as stale and disconnecting (missed heartbeats / dead socket).
    pub heartbeat_timeout: Duration,
    /// The first backoff step; each reconnect doubles it up to `backoff_max`.
    pub backoff_base: Duration,
    /// The ceiling on reconnect backoff (the doc's connectivity-aware cap).
    pub backoff_max: Duration,
}

impl NodeConfig {
    /// A config with production timing defaults: a 10s heartbeat, a 30s
    /// staleness timeout, and reconnect backoff from 500ms doubling to the
    /// doc's 16s cap.
    pub fn new(hub_url: impl Into<String>, node_id: NodeId, token: impl Into<String>) -> Self {
        NodeConfig {
            hub_url: hub_url.into(),
            node_id,
            token: token.into(),
            software_version: stackhour_core::build_info().to_string(),
            capabilities: json!({ "engines": ["claude", "codex"] }),
            heartbeat_interval: Duration::from_secs(10),
            heartbeat_timeout: Duration::from_secs(30),
            backoff_base: Duration::from_millis(500),
            backoff_max: Duration::from_secs(16),
        }
    }

    /// Set the advertised capability snapshot.
    pub fn with_capabilities(mut self, capabilities: Value) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Set the advertised software version.
    pub fn with_software_version(mut self, version: impl Into<String>) -> Self {
        self.software_version = version.into();
        self
    }

    /// Set the heartbeat interval and the matching staleness timeout.
    pub fn with_heartbeat(mut self, interval: Duration, timeout: Duration) -> Self {
        self.heartbeat_interval = interval;
        self.heartbeat_timeout = timeout;
        self
    }

    /// Set the reconnect backoff base and cap.
    pub fn with_backoff(mut self, base: Duration, max: Duration) -> Self {
        self.backoff_base = base;
        self.backoff_max = max;
        self
    }
}

/// The trigger half of a cooperative shutdown. Call [`ShutdownHandle::shutdown`]
/// (or drop it) to ask a running node to stop after its current session.
#[derive(Debug)]
pub struct ShutdownHandle {
    tx: watch::Sender<bool>,
}

/// The observer half handed to [`crate::run`]. Cheap to clone; every clone
/// observes the same trigger.
#[derive(Clone, Debug)]
pub struct ShutdownSignal {
    rx: watch::Receiver<bool>,
}

/// Create a linked [`ShutdownHandle`] / [`ShutdownSignal`] pair.
pub fn shutdown() -> (ShutdownHandle, ShutdownSignal) {
    let (tx, rx) = watch::channel(false);
    (ShutdownHandle { tx }, ShutdownSignal { rx })
}

impl ShutdownHandle {
    /// Request shutdown. Idempotent; safe to call more than once.
    pub fn shutdown(&self) {
        let _ = self.tx.send(true);
    }
}

impl Drop for ShutdownHandle {
    fn drop(&mut self) {
        // Dropping the last handle also stops the node, so a caller that forgets
        // the trigger does not leak an eternal reconnect loop.
        let _ = self.tx.send(true);
    }
}

impl ShutdownSignal {
    /// Resolve once shutdown has been requested. Returns immediately if it
    /// already has. Safe to call repeatedly from `select!`.
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        if *rx.borrow() {
            return;
        }
        // The only transition is false -> true; either a change or a closed
        // channel means "stop".
        let _ = rx.changed().await;
    }
}
