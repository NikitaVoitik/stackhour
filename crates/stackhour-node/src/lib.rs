//! stackhour-node — the execution-node client of the remote-agent control plane.
//!
//! A node dials **out** to the hub over a WebSocket, authenticates with a
//! [`NodeHello`], and then supervises work the hub dispatches. This crate is the
//! client half of the frozen node transport that `stackhour-hub` serves; the two
//! share only [`stackhour_domain::protocol`], never a socket type.
//!
//! ## What this is
//!
//! A durable connection loop that can drive the production
//! [`CliEngine`](crate::CliEngine) or the deterministic [`StubEngine`] used by
//! transport tests. The CLI engine starts installed Claude and Codex programs.
//! ACP remains a later adapter behind the same [`Engine`] trait.
//!
//! ## The frozen node transport
//!
//! - The node dials `GET /v1/node/connect`. Its **first** ws text frame is a
//!   JSON [`NodeHello`]; the hub's **first** frame is a [`HubWelcome`]. If the
//!   welcome is not accepted, [`run`] stops and surfaces the [`ProtocolError`]
//!   rather than reconnecting — a version mismatch or bad credential is terminal.
//! - Every ws message is exactly one JSON-serialized [`protocol`] value as a
//!   text frame: one frame, one message.
//! - After the handshake the node runs a receive loop over
//!   [`HubToNode`] and a send path emitting [`NodeToHub`], plus a heartbeat.
//! - A disconnect (ws error/close, or silence past
//!   [`NodeConfig::heartbeat_timeout`]) triggers reconnect with bounded,
//!   jittered [`backoff`] capped at 16s, resuming with `resume_after_sequence`.
//!
//! ## Concurrency
//!
//! The socket's read and write halves are split ([`futures_util`]). A single
//! writer task owns the sink; the receive loop, the heartbeat, and the engine's
//! streaming tasks all feed it through a channel. No lock is ever held across a
//! socket await.
//!
//! [`protocol`]: stackhour_domain::protocol

mod backoff;
mod config;
mod engine;
mod process;

use futures_util::{SinkExt, StreamExt};
use stackhour_core::{Error, Result};
use stackhour_domain::protocol::{HubToNode, HubWelcome, NodeHello, ProtocolError, WIRE_PROTOCOL_VERSION};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

pub use config::{shutdown, NodeConfig, ShutdownHandle, ShutdownSignal};
pub use engine::{CliEngine, CliEngineConfig, Engine, EngineOutbox, StubEngine};
pub use process::{spawn_engine, RunRequest, RunResult, RunningJob};

use engine::Outgoing;

/// Connect to the hub and supervise installed Claude and Codex CLI programs
/// until [`ShutdownSignal`] fires.
///
/// Returns `Ok(())` on a clean shutdown, or `Err` if the hub terminally
/// rejected the handshake (surfacing the [`ProtocolError`]). Transient
/// disconnects never return — they reconnect with backoff.
pub async fn run(config: NodeConfig, shutdown: ShutdownSignal) -> Result<()> {
    run_with_engine(config, Arc::new(CliEngine::default()), shutdown).await
}

/// Like [`run`], but with a caller-supplied [`Engine`] — the seam the ACP SDK
/// adapter plugs into without touching the transport.
pub async fn run_with_engine(
    config: NodeConfig,
    engine: Arc<dyn Engine>,
    shutdown: ShutdownSignal,
) -> Result<()> {
    let mut attempt: u32 = 0;
    let outbox = EngineOutbox::persistent(config.node_id.clone());

    loop {
        match connect_once(&config, &engine, &outbox, &shutdown).await {
            SessionEnd::Shutdown => return Ok(()),
            SessionEnd::Rejected(err) => {
                return Err(Error::msg(format!("hub rejected node connection: {err}")));
            }
            SessionEnd::Disconnected { established } => {
                // A session that actually came up resets the backoff ramp; a
                // run of failed dials keeps ramping toward the cap.
                if established {
                    attempt = 0;
                }
                attempt = attempt.saturating_add(1);
                let delay = backoff::delay(attempt, config.backoff_base, config.backoff_max);
                tokio::select! {
                    () = shutdown.cancelled() => return Ok(()),
                    () = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

/// How one connection attempt ended.
enum SessionEnd {
    /// Shutdown was requested; the supervisor should stop cleanly.
    Shutdown,
    /// The hub refused the handshake. Terminal — do not reconnect.
    Rejected(ProtocolError),
    /// The link came up (or failed to) and then dropped. Reconnect with backoff;
    /// `established` records whether the handshake had succeeded.
    Disconnected { established: bool },
}

/// Dial the hub, perform the handshake, and — if accepted — run the receive /
/// send / heartbeat session until it disconnects or shutdown fires.
async fn connect_once(
    config: &NodeConfig,
    engine: &Arc<dyn Engine>,
    outbox: &EngineOutbox,
    shutdown: &ShutdownSignal,
) -> SessionEnd {
    let ws = tokio::select! {
        () = shutdown.cancelled() => return SessionEnd::Shutdown,
        dialed = connect_async(&config.hub_url) => match dialed {
            Ok((ws, _resp)) => ws,
            Err(_e) => return SessionEnd::Disconnected { established: false },
        },
    };
    let (mut write, mut read) = ws.split();

    // First frame out: the NodeHello. Phase 1 resumes with `None` — an honest
    // fresh start. The node's link never carries hub-assigned sequences (those
    // ride the client link), so it cannot advertise a real cursor; hub-side
    // UUID dedup of node events makes a replay after reconnect harmless. Wiring
    // a real cursor is the job of the slice that consumes echoed sequences.
    // See `EngineOutbox` in engine.rs for the same rationale.
    let hello = NodeHello {
        node_id: config.node_id.clone(),
        token: config.token.clone(),
        software_version: config.software_version.clone(),
        protocol_version: WIRE_PROTOCOL_VERSION,
        capabilities: config.capabilities.clone(),
        resume_after_sequence: None,
    };
    let hello_json = serde_json::to_string(&hello).expect("NodeHello serializes");
    if write.send(Message::text(hello_json)).await.is_err() {
        return SessionEnd::Disconnected { established: false };
    }

    // First frame in: the HubWelcome.
    let welcome_frame = tokio::select! {
        () = shutdown.cancelled() => return SessionEnd::Shutdown,
        frame = read.next() => frame,
    };
    let welcome: HubWelcome = match welcome_frame {
        Some(Ok(msg)) => match decode_text(msg_text(&msg)) {
            Some(Ok(w)) => w,
            // A non-text or unparsable first frame is a broken hub; retry.
            _ => return SessionEnd::Disconnected { established: false },
        },
        _ => return SessionEnd::Disconnected { established: false },
    };
    if !welcome.accepted {
        let err = welcome.error.unwrap_or(ProtocolError::Unauthenticated);
        return SessionEnd::Rejected(err);
    }

    // Handshake accepted — stand up the steady-state session.
    let (out_tx, out_rx) = mpsc::channel::<Outgoing>(64);
    let writer = tokio::spawn(writer_task(write, out_rx));
    let heartbeat = tokio::spawn(heartbeat_task(
        out_tx.clone(),
        outbox.clone(),
        config.heartbeat_interval,
    ));
    outbox.attach(out_tx.clone()).await;

    let end = tokio::select! {
        () = shutdown.cancelled() => SessionEnd::Shutdown,
        () = receive_loop(read, engine.clone(), outbox.clone(), out_tx.clone(), config.heartbeat_timeout) => {
            SessionEnd::Disconnected { established: true }
        }
    };

    // Tear the session down: drop the last sender so the writer drains, then
    // stop the helpers. In-flight engine streams see their sends fail and stop.
    heartbeat.abort();
    outbox.detach();
    drop(out_tx);
    writer.abort();
    end
}

/// Own the ws sink and serialize every queued frame onto it. Ends when the
/// channel closes (session teardown) or the socket errors.
async fn writer_task<S>(mut sink: S, mut rx: mpsc::Receiver<Outgoing>)
where
    S: futures_util::Sink<Message> + Unpin,
{
    while let Some(out) = rx.recv().await {
        let msg = match out {
            Outgoing::Protocol(p) => {
                Message::text(serde_json::to_string(&p).expect("protocol message serializes"))
            }
            Outgoing::Pong(payload) => Message::Pong(payload.into()),
        };
        if sink.send(msg).await.is_err() {
            break;
        }
    }
    let _ = sink.close().await;
}

/// Emit a `Heartbeat` on the configured interval until the link closes.
async fn heartbeat_task(tx: mpsc::Sender<Outgoing>, outbox: EngineOutbox, interval: std::time::Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; consume it so heartbeats start one full
    // interval in.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if tx
            .send(Outgoing::Protocol(
                stackhour_domain::protocol::NodeToHub::Heartbeat,
            ))
            .await
            .is_err()
        {
            break;
        }
        outbox.resend_pending().await;
    }
}

/// Read hub frames and drive the engine until the link drops or goes stale.
///
/// A read that does not arrive within `heartbeat_timeout` is treated as a dead
/// connection (missed heartbeats), matching the transport contract.
async fn receive_loop<R>(
    mut read: R,
    engine: Arc<dyn Engine>,
    outbox: EngineOutbox,
    out_tx: mpsc::Sender<Outgoing>,
    heartbeat_timeout: std::time::Duration,
) where
    R: futures_util::Stream<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    loop {
        let frame = match tokio::time::timeout(heartbeat_timeout, read.next()).await {
            Err(_elapsed) => break,           // stale: no traffic within the window
            Ok(None | Some(Err(_))) => break, // socket closed or ws-level error
            Ok(Some(Ok(msg))) => msg,
        };

        match frame {
            Message::Text(_) => {
                if let Some(Ok(hub_msg)) = decode_text::<HubToNode>(msg_text(&frame)) {
                    handle_hub_message(hub_msg, &engine, &outbox);
                }
                // A text frame that is not a valid HubToNode is ignored, not
                // fatal — a forward-compatible hub may add message types.
            }
            Message::Ping(payload) => {
                let _ = out_tx.send(Outgoing::Pong(payload.to_vec())).await;
            }
            Message::Close(_) => break,
            // Binary / Pong / raw frames carry no protocol payload here.
            _ => {}
        }
    }
}

/// Route one decoded hub message to the engine.
fn handle_hub_message(msg: HubToNode, engine: &Arc<dyn Engine>, outbox: &EngineOutbox) {
    match msg {
        HubToNode::DispatchCommand {
            command_id,
            expires_at,
            work,
        } => {
            // Drop stale work rather than execute it (the dispatch's expiry).
            // Compared in epoch seconds so the node needs no chrono dependency;
            // `timestamp()` is an inherent method on the domain's DateTime.
            if let Some(expires_at) = expires_at {
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if now_secs >= expires_at.timestamp() {
                    return;
                }
            }
            engine.dispatch(command_id, work, outbox.clone());
        }
        HubToNode::CancelCommand { command_id } => engine.cancel(command_id, outbox.clone()),
        HubToNode::EventAck { event_id } => outbox.acknowledge_event(event_id),
        // STUB: the stub engine gates no approvals, so a decision is a no-op in
        // Phase 1. The ACP adapter will verify and answer the pending request.
        HubToNode::ApprovalDecision { .. } | HubToNode::Heartbeat => {} // Inbound heartbeats only need to reset the staleness timer, which the
                                                                        // receive loop already does by observing any frame.
    }
}

/// Borrow a text message's body as `&str` (empty for non-text frames).
fn msg_text(msg: &Message) -> &str {
    match msg {
        Message::Text(t) => t,
        _ => "",
    }
}

/// Decode a JSON text body into a protocol value, or `None` if the frame was
/// not decodable text. The inner `Result` distinguishes "not text" from "text
/// that failed to parse".
fn decode_text<T: serde::de::DeserializeOwned>(text: &str) -> Option<serde_json::Result<T>> {
    if text.is_empty() {
        return None;
    }
    Some(serde_json::from_str::<T>(text))
}
