//! Smoke tests for the hub over a real WebSocket, using `tokio-tungstenite` as
//! the client. Each test binds the hub on an ephemeral port and drives the
//! frozen transport contract exactly as a client or node would.
//!
//! This is deliberately a thin smoke layer: it proves the two endpoints are
//! wired up end-to-end (a client command round-trips to a durable delivery, and
//! the node handshake accepts a good token and rejects a bad one). The
//! exhaustive transport-edge suite — second-client catch-up, idempotent replay,
//! node-event fan-out, version negotiation, heartbeats, lag/resync — is a
//! separate follow-up workflow and is marked out below where those cases go.

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::json;
use std::net::SocketAddr;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as TMessage;

use stackhour_domain::{
    ClientCommand, CommandId, EventKind, HubToClient, HubWelcome, NodeHello, NodeId, ProtocolError,
    Subscribe, WIRE_PROTOCOL_VERSION,
};
use stackhour_hub::HubState;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const SECRET: &str = "s3cret";

/// Every ws await is bounded by this, so a contract regression fails fast
/// instead of hanging the suite.
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Bind an in-memory hub on port 0 and return its address.
async fn start_hub() -> SocketAddr {
    let state = HubState::in_memory(SECRET).expect("hub");
    let (addr, _handle) = stackhour_hub::spawn(state, "127.0.0.1:0").await.expect("spawn");
    addr
}

async fn connect(addr: SocketAddr, path: &str) -> Ws {
    let url = format!("ws://{addr}{path}");
    let (ws, _resp) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .expect("ws connect");
    ws
}

async fn client(addr: SocketAddr) -> Ws {
    connect(addr, "/v1/client/connect").await
}

async fn node(addr: SocketAddr) -> Ws {
    connect(addr, "/v1/node/connect").await
}

async fn send_json<T: Serialize>(ws: &mut Ws, value: &T) {
    let text = serde_json::to_string(value).expect("serialize");
    ws.send(TMessage::Text(text)).await.expect("send");
}

/// Next text frame, skipping control frames; `None` on close/error/timeout.
async fn recv_text(ws: &mut Ws) -> Option<String> {
    loop {
        match tokio::time::timeout(RECV_TIMEOUT, ws.next()).await {
            Ok(Some(Ok(TMessage::Text(t)))) => return Some(t.as_str().to_string()),
            Ok(Some(Ok(TMessage::Ping(_)))) | Ok(Some(Ok(TMessage::Pong(_)))) => continue,
            Ok(Some(Ok(TMessage::Close(_)))) | Ok(None) => return None,
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) => return None,
            Err(_elapsed) => return None,
        }
    }
}

async fn recv_client(ws: &mut Ws) -> HubToClient {
    let text = recv_text(ws).await.expect("client frame");
    serde_json::from_str(&text).expect("parse HubToClient")
}

async fn recv_welcome(ws: &mut Ws) -> HubWelcome {
    let text = recv_text(ws).await.expect("welcome frame");
    serde_json::from_str(&text).expect("parse HubWelcome")
}

fn assert_receipt(msg: &HubToClient, expected_sequence: i64) {
    match msg {
        HubToClient::CommandReceipt {
            accepted,
            assigned_sequence,
            ..
        } => {
            assert!(accepted, "receipt should be accepted");
            assert_eq!(*assigned_sequence, Some(expected_sequence));
        }
        other => panic!("expected CommandReceipt, got {other:?}"),
    }
}

fn assert_delivery(msg: &HubToClient, kind: EventKind, sequence: i64) {
    match msg {
        HubToClient::EventDelivery { event } => {
            assert_eq!(event.kind, kind);
            assert_eq!(event.sequence, sequence);
        }
        other => panic!("expected EventDelivery, got {other:?}"),
    }
}

fn create_task(title: &str) -> ClientCommand {
    ClientCommand::CreateTask {
        command_id: CommandId::new(),
        title: title.to_string(),
    }
}

fn valid_hello(node_id: &str, version: i64) -> NodeHello {
    NodeHello {
        node_id: NodeId::from(node_id),
        token: SECRET.to_string(),
        software_version: "0.1.0".to_string(),
        protocol_version: version,
        capabilities: json!({ "acp": true }),
        resume_after_sequence: None,
    }
}

// ---------------------------------------------------------------------------
// Client link
// ---------------------------------------------------------------------------

/// A CreateTask command yields an accepted receipt at sequence 1 and a live
/// task.created delivery at sequence 1 (order between the two is not fixed).
#[tokio::test]
async fn create_task_produces_receipt_and_delivery() {
    let addr = start_hub().await;
    let mut c = client(addr).await;

    send_json(
        &mut c,
        &Subscribe {
            token: None,
            after_sequence: None,
        },
    )
    .await;
    match recv_client(&mut c).await {
        HubToClient::SubscribeAck { head_sequence, .. } => assert_eq!(head_sequence, 0),
        other => panic!("expected SubscribeAck, got {other:?}"),
    }

    send_json(&mut c, &create_task("build the thing")).await;

    // One receipt and one delivery, in either order.
    let a = recv_client(&mut c).await;
    let b = recv_client(&mut c).await;
    let (receipt, delivery) = match (&a, &b) {
        (HubToClient::CommandReceipt { .. }, _) => (&a, &b),
        (_, HubToClient::CommandReceipt { .. }) => (&b, &a),
        _ => panic!("expected one receipt among {a:?} and {b:?}"),
    };
    assert_receipt(receipt, 1);
    assert_delivery(delivery, EventKind::TaskCreated, 1);
}

// A second client subscribing with after_sequence:0 must replay the same
// durable task.created from the log (durable catch-up, not just live fan-out).
// covered by the transport-tests slice

// Replaying a command with the same command_id must create exactly one event;
// both receipts carry the same sequence and no second delivery is emitted.
// covered by the transport-tests slice

// ---------------------------------------------------------------------------
// Node link
// ---------------------------------------------------------------------------

/// A node presenting a valid NodeHello (good token, matching version) is
/// welcomed with an accepting HubWelcome.
#[tokio::test]
async fn valid_node_hello_is_welcomed() {
    let addr = start_hub().await;
    let mut n = node(addr).await;

    send_json(&mut n, &valid_hello("laptop", WIRE_PROTOCOL_VERSION)).await;

    let welcome = recv_welcome(&mut n).await;
    assert!(welcome.accepted, "a valid hello must be accepted");
    assert_eq!(welcome.error, None);
    assert_eq!(welcome.protocol_version, WIRE_PROTOCOL_VERSION);
}

/// A NodeHello with a bad token is rejected as Unauthenticated and the socket is
/// closed.
#[tokio::test]
async fn bad_token_is_rejected_and_closed() {
    let addr = start_hub().await;
    let mut n = node(addr).await;

    let mut hello = valid_hello("intruder", WIRE_PROTOCOL_VERSION);
    hello.token = "wrong".to_string();
    send_json(&mut n, &hello).await;

    let welcome = recv_welcome(&mut n).await;
    assert!(!welcome.accepted);
    assert_eq!(welcome.error, Some(ProtocolError::Unauthenticated));

    // The hub closes the connection after the rejecting welcome.
    assert!(recv_text(&mut n).await.is_none(), "socket should be closed");
}

// A NodeHello advertising an incompatible protocol version is rejected with a
// VersionMismatch error (the negotiation itself is unit-tested in the crate).
// covered by the transport-tests slice

// A node's NodeEvent must fan out to a subscribed client as the matching
// EventDelivery (node -> hub -> durable log -> live client).
// covered by the transport-tests slice
