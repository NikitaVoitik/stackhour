//! Smoke tests for the node over a real WebSocket against a *fake* hub.
//!
//! A tiny axum ws server stands in for `stackhour-hub`: it reads the node's
//! [`NodeHello`], answers with a [`HubWelcome`], and (on the happy path)
//! dispatches a `StartRun`, then records every [`NodeToHub`] frame the node
//! sends back. The node dials it with the crate's real [`run`], so the whole
//! frozen transport — connect, handshake, dispatch, ack, event streaming — is
//! exercised end to end. Every await is wrapped in a timeout so a hang fails
//! the test instead of blocking forever.
//!
//! These are deliberately just two: a happy path and a rejection. The thorough
//! transport suite — reconnect-with-resume, interrupt mid-stream, staleness —
//! is a separate workflow.

use axum::{
    extract::{
        ws::{Message as AxMsg, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

use stackhour_domain::entities::AccessPolicy;
use stackhour_domain::ids::{CommandId, NodeId, RunId, TaskId};
use stackhour_domain::protocol::{
    HubToNode, HubWelcome, NodeHello, NodeToHub, NodeWork, ProtocolError, WIRE_PROTOCOL_VERSION,
};
use stackhour_domain::EventKind;
use stackhour_node::{run, run_with_engine, shutdown, NodeConfig, StubEngine};
use std::sync::Arc;

/// Nothing in these tests should take more than a beat; a hang is a bug.
async fn within<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), f)
        .await
        .expect("operation timed out")
}

/// What the fake hub should do once the node dials in.
#[derive(Clone)]
struct FakeHub {
    /// Accept the handshake, or reject it with `reject_error`.
    accept: bool,
    /// The rejection reason, sent when `accept` is false.
    reject_error: Option<ProtocolError>,
    /// A `StartRun` to dispatch after accepting, if any.
    dispatch: Option<(CommandId, RunId, TaskId)>,
    /// Every steady-state frame the node sends back lands here.
    frames: mpsc::UnboundedSender<NodeToHub>,
    /// The opening `NodeHello` lands here.
    hello: mpsc::UnboundedSender<NodeHello>,
}

/// The `/v1/node/connect` upgrade handler.
async fn connect(State(hub): State<FakeHub>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| serve(socket, hub))
}

/// Drive one accepted-or-rejected node session on the fake hub.
async fn serve(mut socket: WebSocket, hub: FakeHub) {
    // First frame in: the NodeHello.
    let hello: NodeHello = match next_text(&mut socket).await {
        Some(text) => serde_json::from_str(&text).expect("first frame is a NodeHello"),
        None => return,
    };
    let _ = hub.hello.send(hello);

    // First frame out: the HubWelcome. A rejection ends the session.
    if !hub.accept {
        let reason = hub.reject_error.clone().expect("a rejection carries a reason");
        send_json(&mut socket, &HubWelcome::reject(reason)).await;
        return;
    }
    send_json(&mut socket, &HubWelcome::accept()).await;

    // Optionally dispatch a run, then relay whatever the node sends.
    if let Some((command_id, run_id, task_id)) = hub.dispatch {
        let dispatch = HubToNode::DispatchCommand {
            command_id,
            expires_at: None,
            work: NodeWork::StartRun {
                run_id,
                task_id,
                engine: "stub".to_string(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                access_policy: AccessPolicy::Automatic,
                workspace_path: None,
            },
        };
        send_json(&mut socket, &dispatch).await;
    }

    while let Some(text) = next_text(&mut socket).await {
        if let Ok(msg) = serde_json::from_str::<NodeToHub>(&text) {
            if hub.frames.send(msg).is_err() {
                break;
            }
        }
    }
}

/// Next text frame from the ws, skipping control frames; `None` on close.
async fn next_text(socket: &mut WebSocket) -> Option<String> {
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            AxMsg::Text(t) => return Some(t.to_string()),
            AxMsg::Close(_) => return None,
            _ => continue,
        }
    }
    None
}

/// Send one JSON protocol value as a text frame.
async fn send_json<T: serde::Serialize + Sync>(socket: &mut WebSocket, value: &T) {
    let text = serde_json::to_string(value).expect("serialize");
    let _ = socket.send(AxMsg::Text(text.into())).await;
}

/// Bind the fake hub on an ephemeral port and return its address.
async fn spawn_hub(hub: FakeHub) -> SocketAddr {
    let app = Router::new()
        .route("/v1/node/connect", get(connect))
        .with_state(hub);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

/// A node config pointed at `addr`, with heartbeats pushed far out so no
/// keep-alive frame interleaves with the short event stream under test.
fn config_for(addr: SocketAddr) -> NodeConfig {
    NodeConfig::new(
        format!("ws://{addr}/v1/node/connect"),
        NodeId::from("test-node"),
        "token",
    )
    .with_heartbeat(Duration::from_secs(30), Duration::from_secs(60))
}

/// Happy path: the node handshakes, and a dispatched `StartRun` yields a
/// `CommandAck` plus the ordered `run.started -> deltas -> assistant.completed
/// -> run.completed` events, all bound to the dispatched run and task.
#[tokio::test]
async fn handshake_then_dispatch_streams_ordered_events_and_an_ack() {
    let (frames_tx, mut frames_rx) = mpsc::unbounded_channel();
    let (hello_tx, mut hello_rx) = mpsc::unbounded_channel();
    let command_id = CommandId::new();
    let run_id = RunId::new();
    let task_id = TaskId::new();

    let addr = spawn_hub(FakeHub {
        accept: true,
        reject_error: None,
        dispatch: Some((command_id, run_id, task_id)),
        frames: frames_tx,
        hello: hello_tx,
    })
    .await;

    let (handle, signal) = shutdown();
    let node = tokio::spawn(run_with_engine(
        config_for(addr),
        Arc::new(StubEngine::new()),
        signal,
    ));

    // The opening NodeHello: right identity, and a fresh (no-resume) cursor.
    let hello = within(hello_rx.recv()).await.expect("NodeHello");
    assert_eq!(hello.node_id, NodeId::from("test-node"));
    assert_eq!(hello.token, "token");
    assert_eq!(hello.protocol_version, WIRE_PROTOCOL_VERSION);
    assert_eq!(hello.resume_after_sequence, None);

    // Collect steady-state frames until the run completes.
    let mut acked = None;
    let mut events = Vec::new();
    loop {
        match within(frames_rx.recv()).await.expect("node frame") {
            NodeToHub::CommandAck { command_id } => acked = Some(command_id),
            NodeToHub::NodeEvent { draft, .. } => {
                let done = draft.kind == EventKind::RunCompleted;
                events.push((draft.kind, draft.task_id, draft.run_id));
                if done {
                    break;
                }
            }
            NodeToHub::Heartbeat => {}
        }
    }

    // The dispatch was acknowledged by its command id.
    assert_eq!(acked, Some(command_id));

    // The exact ordered event vocabulary of a completed stub run.
    let kinds: Vec<EventKind> = events.iter().map(|(k, _, _)| *k).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::RunStarted,
            EventKind::MessageAssistantDelta,
            EventKind::MessageAssistantDelta,
            EventKind::MessageAssistantCompleted,
            EventKind::RunCompleted,
        ]
    );

    // Every event was scoped to the dispatched run and task.
    for (_, ev_task, ev_run) in &events {
        assert_eq!(*ev_task, task_id);
        assert_eq!(*ev_run, Some(run_id));
    }

    handle.shutdown();
    let _ = within(node).await;
}

/// Rejection: a `HubWelcome{accepted: false, VersionMismatch}` makes `run`
/// return the error promptly instead of reconnecting in a loop.
#[tokio::test]
async fn rejected_handshake_surfaces_the_error_without_looping() {
    let (frames_tx, _frames_rx) = mpsc::unbounded_channel();
    let (hello_tx, _hello_rx) = mpsc::unbounded_channel();

    let addr = spawn_hub(FakeHub {
        accept: false,
        reject_error: Some(ProtocolError::VersionMismatch {
            client: WIRE_PROTOCOL_VERSION + 1,
            hub: WIRE_PROTOCOL_VERSION,
        }),
        dispatch: None,
        frames: frames_tx,
        hello: hello_tx,
    })
    .await;

    let (_handle, signal) = shutdown();

    // A terminal rejection must resolve `run` quickly; the timeout would trip
    // if the node instead looped on backoff.
    let result = tokio::time::timeout(Duration::from_secs(5), run(config_for(addr), signal))
        .await
        .expect("run returned promptly rather than looping on backoff");

    let err = result.expect_err("a rejected handshake is terminal");
    assert!(
        err.to_string().contains("mismatch"),
        "error should surface the version mismatch, got: {err}"
    );
}

// covered by the transport-tests slice: reconnect with resume_after_sequence
// set to the highest event sequence the node has seen (drop the socket after a
// few events, accept the redial, assert the second NodeHello resumes).

// covered by the transport-tests slice: interrupt mid-stream via CancelCommand
// (and via a dispatched InterruptRun), asserting a prompt run.interrupted and
// no further events for that run.
