//! End-to-end transport tests for the Phase-1 control plane.
//!
//! Each test stands up a **real** [`stackhour_hub`] server on an ephemeral
//! loopback port, spawns a **real** [`stackhour_node`] client dialing that hub,
//! and opens raw `tokio-tungstenite` CLIENT sockets to `/v1/client/connect` that
//! send [`Subscribe`] then [`ClientCommand`] frames and read [`HubToClient`]
//! frames — one JSON value per frame, exactly as the frozen transport specifies.
//!
//! The node runs its Phase-1 [`StubEngine`] (no ACP, no subprocess): a dispatched
//! `StartRun` streams `run.started -> delta -> delta -> assistant.completed ->
//! run.completed`, and an interrupt cuts that short with a `run.interrupted`.
//!
//! These cover the "Acceptance tests" subset of the design doc that is provable
//! at the transport layer in Phase 1. Durable approval-across-disconnect is a
//! Phase-2 slice and is intentionally absent.
//!
//! Every `.await` on a socket is wrapped in a [`tokio::time::timeout`] so a
//! contract regression fails loudly with a clear message instead of hanging the
//! test binary.

#![allow(clippy::bool_assert_comparison)]

use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as TMessage;

use stackhour_domain::{
    AccessPolicy, ClientCommand, CommandId, Event, EventKind, Hub, HubToClient, NodeId, RunId, Subscribe,
    TaskId,
};
use stackhour_node::{run_with_engine, shutdown, NodeConfig, ShutdownHandle, StubEngine};
use std::sync::Arc;

// ===========================================================================
// support: harness + client helpers, shared by every test below
// ===========================================================================

/// The shared node secret and the node identity every test uses.
const SECRET: &str = "e2e-secret";
const NODE_ID: &str = "laptop";

/// Every socket await is bounded by this. A hang is a bug, not a slow machine.
const OP_TIMEOUT: Duration = Duration::from_secs(5);

/// A short window used to prove the *absence* of a further frame (no duplicate
/// delivery, no trailing delta). Long enough to be reliable on a busy CI box,
/// short enough to keep the suite quick.
const IDLE: Duration = Duration::from_millis(400);

/// A raw client socket to the hub's `/v1/client/connect` route.
type ClientWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Await `f`, failing the test with `label` if it does not resolve in time.
async fn within<F: Future>(label: &str, f: F) -> F::Output {
    match tokio::time::timeout(OP_TIMEOUT, f).await {
        Ok(v) => v,
        Err(_) => panic!("timed out after {OP_TIMEOUT:?}: {label}"),
    }
}

/// Start an in-memory hub on `127.0.0.1:0` and return its bound address plus the
/// server task handle (kept alive by the caller for the test's duration).
async fn start_hub_in_memory() -> (SocketAddr, JoinHandle<()>) {
    let state = stackhour_hub::HubState::in_memory(SECRET).expect("in-memory hub");
    stackhour_hub::spawn(state, "127.0.0.1:0")
        .await
        .expect("spawn hub")
}

/// Start a hub backed by an on-disk SQLite file at `path` (so a test can open a
/// second connection to the same database and inspect durable rows).
async fn start_hub_on_disk(path: &Path) -> (SocketAddr, JoinHandle<()>) {
    let state = stackhour_hub::HubState::open(path, SECRET).expect("on-disk hub");
    stackhour_hub::spawn(state, "127.0.0.1:0")
        .await
        .expect("spawn hub")
}

/// A node config pointed at `addr`, with far-out heartbeats (so no keep-alive
/// frame interleaves with the short event streams) and tight reconnect backoff
/// (so a dropped node redials within milliseconds).
fn node_config(addr: SocketAddr) -> NodeConfig {
    NodeConfig::new(
        format!("ws://{addr}/v1/node/connect"),
        NodeId::from(NODE_ID),
        SECRET,
    )
    .with_heartbeat(Duration::from_secs(30), Duration::from_secs(60))
    .with_backoff(Duration::from_millis(20), Duration::from_millis(80))
}

/// Spawn a real node dialing `addr`. The returned [`ShutdownHandle`] stops it
/// (dropping it also stops it), and the [`JoinHandle`] resolves when it does.
/// The node's `run` result is discarded — the test observes the node through the
/// hub's event stream, not its return value.
fn spawn_node(addr: SocketAddr) -> (ShutdownHandle, JoinHandle<()>) {
    let (handle, signal) = shutdown();
    let task = tokio::spawn(async move {
        let _ = run_with_engine(node_config(addr), Arc::new(StubEngine::new()), signal).await;
    });
    (handle, task)
}

/// Open and return a raw client socket to the hub.
async fn connect_client(addr: SocketAddr) -> ClientWs {
    let url = format!("ws://{addr}/v1/client/connect");
    let (ws, _resp) = within(
        "client ws connect",
        tokio_tungstenite::connect_async(url.as_str()),
    )
    .await
    .expect("client connect");
    ws
}

/// Send a `Subscribe{after_sequence}` and consume the `SubscribeAck`, returning
/// the hub's announced head sequence. Catch-up deliveries (if any) follow.
async fn subscribe(ws: &mut ClientWs, after_sequence: Option<i64>) -> i64 {
    let text = serde_json::to_string(&Subscribe {
        token: None,
        after_sequence,
    })
    .expect("serialize subscribe");
    within("send subscribe", ws.send(TMessage::Text(text)))
        .await
        .expect("send subscribe");
    match next_frame(ws).await {
        HubToClient::SubscribeAck { head_sequence, .. } => head_sequence,
        other => panic!("expected SubscribeAck, got {other:?}"),
    }
}

/// Send one client command frame.
async fn send_command(ws: &mut ClientWs, cmd: &ClientCommand) {
    let text = serde_json::to_string(cmd).expect("serialize command");
    within("send command", ws.send(TMessage::Text(text)))
        .await
        .expect("send command");
}

/// The next `HubToClient` frame, bounded by [`OP_TIMEOUT`]; panics on
/// close/timeout so a stalled contract fails the test.
async fn next_frame(ws: &mut ClientWs) -> HubToClient {
    frame_within(ws, OP_TIMEOUT)
        .await
        .expect("a HubToClient frame within the timeout")
}

/// The next frame, or `None` if none arrives within `dur` (used both to bound
/// normal reads and to prove the *absence* of a frame). Control ping/pong frames
/// are skipped; a close or ws error yields `None`.
async fn frame_within(ws: &mut ClientWs, dur: Duration) -> Option<HubToClient> {
    loop {
        match tokio::time::timeout(dur, ws.next()).await {
            Ok(Some(Ok(TMessage::Text(t)))) => {
                return Some(serde_json::from_str(&t).expect("parse HubToClient"));
            }
            Ok(Some(Ok(TMessage::Ping(_)))) | Ok(Some(Ok(TMessage::Pong(_)))) => continue,
            Ok(Some(Ok(TMessage::Close(_)))) | Ok(None) => return None,
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) => return None,
            Err(_elapsed) => return None,
        }
    }
}

/// The next durable [`Event`] delivery, skipping receipts, acks, and heartbeats.
async fn next_event(ws: &mut ClientWs) -> Event {
    loop {
        match next_frame(ws).await {
            HubToClient::EventDelivery { event } => return event,
            HubToClient::CommandReceipt { .. }
            | HubToClient::SubscribeAck { .. }
            | HubToClient::Heartbeat => continue,
        }
    }
}

/// Collect event deliveries until (and including) the first one that satisfies
/// `done`. Every read is individually bounded, so a stream that never reaches
/// the predicate fails with a clear timeout rather than hanging.
async fn collect_events_until(ws: &mut ClientWs, mut done: impl FnMut(&Event) -> bool) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let event = next_event(ws).await;
        let stop = done(&event);
        events.push(event);
        if stop {
            return events;
        }
    }
}

/// Create a task on `ws` and return its durable id (read back from the
/// `task.created` delivery). Assumes `ws` is subscribed and otherwise idle.
async fn create_task(ws: &mut ClientWs, title: &str) -> TaskId {
    send_command(
        ws,
        &ClientCommand::CreateTask {
            command_id: CommandId::new(),
            title: title.to_string(),
        },
    )
    .await;
    let created = collect_events_until(ws, |e| e.kind == EventKind::TaskCreated).await;
    created.last().expect("at least one event").task_id
}

/// A `StartRun` command targeting this suite's node with the stub engine.
fn start_run(task_id: TaskId) -> ClientCommand {
    ClientCommand::StartRun {
        command_id: CommandId::new(),
        task_id,
        node_id: NodeId::from(NODE_ID),
        engine: "stub".to_string(),
        access_policy: AccessPolicy::Automatic,
        workspace_path: None,
    }
}

/// Drive `ws` until the node's `node.connected` event is observed, returning the
/// events seen up to and including it. Because a fresh client subscribes with a
/// cursor, this is delivered whether the node connected before or after the
/// subscribe (catch-up or live).
async fn await_node_connected(ws: &mut ClientWs) -> Vec<Event> {
    collect_events_until(ws, |e| e.kind == EventKind::NodeConnected).await
}

/// Assert an event list is what a single continuous subscription must see: a
/// strictly ascending, gapless run of sequences with no duplicated sequence or
/// event id. This is the "each event exactly once, none missing" invariant.
fn assert_gapless_unique_ordered(events: &[Event]) {
    assert!(!events.is_empty(), "expected a non-empty event list");
    for pair in events.windows(2) {
        assert!(
            pair[1].sequence > pair[0].sequence,
            "sequences must strictly ascend: {} then {}",
            pair[0].sequence,
            pair[1].sequence,
        );
    }
    let seqs: BTreeSet<i64> = events.iter().map(|e| e.sequence).collect();
    assert_eq!(seqs.len(), events.len(), "duplicate sequence delivered");
    let first = *seqs.iter().next().unwrap();
    let last = *seqs.iter().next_back().unwrap();
    assert_eq!(
        last - first + 1,
        events.len() as i64,
        "gap in the delivered sequence range {first}..={last}",
    );
    let ids: HashSet<_> = events.iter().map(|e| e.event_id).collect();
    assert_eq!(ids.len(), events.len(), "duplicate event id delivered");
}

/// The identity triple used to compare what two clients saw.
fn identity(events: &[Event]) -> Vec<(i64, EventKind, stackhour_domain::EventId)> {
    events.iter().map(|e| (e.sequence, e.kind, e.event_id)).collect()
}

/// Drive one full task+run to completion on `ws` (freshly subscribed and
/// otherwise idle) with a live node attached: wait for the node to register,
/// create a task, start a run, and collect every event through `run.completed`.
/// Returns the accumulated history plus the task and run ids.
async fn drive_full_run(ws: &mut ClientWs, title: &str) -> (Vec<Event>, TaskId, RunId) {
    // Wait for the node so `StartRun` has somewhere to dispatch.
    let mut history = await_node_connected(ws).await;

    send_command(
        ws,
        &ClientCommand::CreateTask {
            command_id: CommandId::new(),
            title: title.to_string(),
        },
    )
    .await;
    let created = collect_events_until(ws, |e| e.kind == EventKind::TaskCreated).await;
    let task_id = created.last().expect("task.created").task_id;
    history.extend(created);

    send_command(ws, &start_run(task_id)).await;
    let tail = collect_events_until(ws, |e| e.kind == EventKind::RunCompleted).await;
    let run_id = tail
        .iter()
        .find_map(|e| e.run_id)
        .expect("a run id in the run stream");
    history.extend(tail);

    (history, task_id, run_id)
}

// ===========================================================================
// 1. two clients share one history
// ===========================================================================

/// Client A drives a task to completion live; client B connects AFTER with
/// `Subscribe{after_sequence: 0}` and catches up the FULL history in sequence
/// order, gapless and dup-free — the identical history A watched live.
#[tokio::test]
async fn two_clients_share_one_history() {
    let (addr, _hub) = start_hub_in_memory().await;
    let (_node, _node_task) = spawn_node(addr);

    let mut a = connect_client(addr).await;
    assert_eq!(subscribe(&mut a, Some(0)).await, 0, "fresh hub head is 0");
    let (a_history, _task_id, _run_id) = drive_full_run(&mut a, "shared history").await;
    assert_gapless_unique_ordered(&a_history);

    // B joins afterwards and catches the same history up from the start.
    let mut b = connect_client(addr).await;
    subscribe(&mut b, Some(0)).await;
    let b_history = collect_events_until(&mut b, |e| e.kind == EventKind::RunCompleted).await;
    assert_gapless_unique_ordered(&b_history);
    assert_eq!(
        identity(&a_history),
        identity(&b_history),
        "B must catch up the identical history A saw live",
    );
}

// ===========================================================================
// 2. sleep during streaming, reconnect, no duplicates
// ===========================================================================

/// A node drops mid-life (a "sleep"), the hub records the disconnect, a node of
/// the same identity reconnects, and a second run completes — all while a
/// continuously-subscribed client's stream stays gapless and duplicate-free.
///
/// Phase-1 note: the node does not replay its own events on reconnect (its
/// resume cursor is honestly `None`; hub-side UUID dedup is what would absorb a
/// replay). True resend-dedup is exercised once the resume-cursor slice lands.
#[tokio::test]
async fn sleep_during_stream_then_reconnect_without_dupes() {
    let (addr, _hub) = start_hub_in_memory().await;

    let mut a = connect_client(addr).await;
    subscribe(&mut a, Some(0)).await;

    // First node: run a task to completion.
    let (node1, _t1) = spawn_node(addr);
    let (mut history, _task_id, _run_id) = drive_full_run(&mut a, "before sleep").await;

    // The node "sleeps": drop it and observe the hub record the disconnect.
    drop(node1);
    let down = collect_events_until(&mut a, |e| e.kind == EventKind::NodeDisconnected).await;
    history.extend(down);

    // A node of the same identity wakes up and a fresh run completes.
    let (_node2, _t2) = spawn_node(addr);
    let (after, _t2id, _r2) = drive_full_run(&mut a, "after wake").await;
    history.extend(after);

    // The one continuous subscription saw every event exactly once, no gaps.
    assert_gapless_unique_ordered(&history);
}

// ===========================================================================
// 3. idempotent client command
// ===========================================================================

/// Replaying the same `command_id` mints exactly one `task.created`, and both
/// receipts carry the identical assigned sequence.
#[tokio::test]
async fn idempotent_create_task() {
    let (addr, _hub) = start_hub_in_memory().await;
    let mut a = connect_client(addr).await;
    subscribe(&mut a, Some(0)).await;

    let command_id = CommandId::new();
    let cmd = ClientCommand::CreateTask {
        command_id,
        title: "once".to_string(),
    };
    send_command(&mut a, &cmd).await;
    send_command(&mut a, &cmd).await;

    let mut receipt_seqs = Vec::new();
    let mut created = 0;
    while receipt_seqs.len() < 2 {
        match next_frame(&mut a).await {
            HubToClient::CommandReceipt {
                command_id: cid,
                accepted,
                assigned_sequence,
                ..
            } if cid == command_id => {
                assert!(accepted, "receipt must be accepted");
                receipt_seqs.push(assigned_sequence.expect("accepted receipt carries a sequence"));
            }
            HubToClient::EventDelivery { event } if event.kind == EventKind::TaskCreated => {
                created += 1;
            }
            _ => {}
        }
    }
    // Prove the absence of a second task within a quiet window.
    while let Some(frame) = frame_within(&mut a, IDLE).await {
        if let HubToClient::EventDelivery { event } = frame {
            if event.kind == EventKind::TaskCreated {
                created += 1;
            }
        }
    }
    assert_eq!(created, 1, "the replayed command_id must not mint a second task");
    assert_eq!(
        receipt_seqs[0], receipt_seqs[1],
        "both receipts must share the one assigned sequence",
    );
}

// ===========================================================================
// 4. interrupt from another client
// ===========================================================================

/// Client A starts a run; client B interrupts it; A observes `run.interrupted`
/// as the terminal state (the stub stops rather than completing).
#[tokio::test]
async fn interrupt_from_another_client() {
    let (addr, _hub) = start_hub_in_memory().await;
    let (_node, _node_task) = spawn_node(addr);

    // Both clients connect BEFORE the run starts, so B can interrupt with only a
    // single loopback round-trip of latency — well inside the stub's stream
    // window, keeping the test off the flaky knife-edge.
    let mut a = connect_client(addr).await;
    let mut b = connect_client(addr).await;
    subscribe(&mut a, Some(0)).await;
    subscribe(&mut b, Some(0)).await;
    await_node_connected(&mut a).await;
    let task_id = create_task(&mut a, "interrupt").await;
    send_command(&mut a, &start_run(task_id)).await;

    // B learns the run id from its own live stream and interrupts immediately.
    let started = collect_events_until(&mut b, |e| e.kind == EventKind::RunStarted).await;
    let run_id = started.iter().find_map(|e| e.run_id).expect("run id");
    send_command(
        &mut b,
        &ClientCommand::InterruptRun {
            command_id: CommandId::new(),
            run_id,
        },
    )
    .await;

    let tail = collect_events_until(&mut a, |e| {
        e.kind == EventKind::RunInterrupted || e.kind == EventKind::RunCompleted
    })
    .await;
    let terminal = tail.last().expect("a terminal run event").kind;
    assert_eq!(
        terminal,
        EventKind::RunInterrupted,
        "an interrupt from another client must interrupt the run, got {terminal:?}",
    );
}

// ===========================================================================
// 5. node-event fanout
// ===========================================================================

/// A node-originated event reaches every subscribed client, and both clients
/// observe the identical stream.
#[tokio::test]
async fn node_event_fanout() {
    let (addr, _hub) = start_hub_in_memory().await;
    let (_node, _node_task) = spawn_node(addr);

    let mut a = connect_client(addr).await;
    let mut b = connect_client(addr).await;
    subscribe(&mut a, Some(0)).await;
    subscribe(&mut b, Some(0)).await;

    let (a_history, _t, _r) = drive_full_run(&mut a, "fanout").await;
    let b_history = collect_events_until(&mut b, |e| e.kind == EventKind::RunCompleted).await;

    let node_kind = EventKind::MessageAssistantDelta;
    assert!(
        a_history.iter().any(|e| e.kind == node_kind),
        "client A missed the node-originated event",
    );
    assert!(
        b_history.iter().any(|e| e.kind == node_kind),
        "client B missed the node-originated event",
    );
    assert_eq!(
        identity(&a_history),
        identity(&b_history),
        "both clients must see the identical fanned-out stream",
    );
}

// ===========================================================================
// 6. durable entities exist
// ===========================================================================

/// After a task runs, the durable `tasks` and `runs` ROWS exist in the hub's
/// SQLite — not merely the events — readable through the domain layer.
#[tokio::test]
async fn durable_entities_exist() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("hub.db");
    let (addr, _hub) = start_hub_on_disk(&path).await;
    let (_node, _node_task) = spawn_node(addr);

    let mut a = connect_client(addr).await;
    subscribe(&mut a, Some(0)).await;
    let (_history, task_id, run_id) = drive_full_run(&mut a, "durable").await;

    // Open the same database through the domain layer and assert the rows.
    let hub = Hub::open(&path).expect("open hub db read-side");
    assert!(
        hub.get_task(&task_id).expect("query task").is_some(),
        "durable task row missing",
    );
    assert!(
        hub.get_run(&run_id).expect("query run").is_some(),
        "durable run row missing",
    );
}
