//! stackhour-domain — the durable foundation of the remote-agent control plane.
//!
//! This crate is the dependency root for the future `stackhour-hub` and
//! `stackhour-node` crates. It owns the five core entities, the stable event
//! vocabulary, and the append-only SQLite event log with a single hub-assigned
//! sequence. Pure Rust + `rusqlite`; no async, no network.
//!
//! - [`entities`] — [`Node`], [`Task`], [`Run`], [`Approval`] and their status
//!   enums (the ordered history row [`Event`] lives in [`event`]).
//! - [`event`] — the [`EventKind`] vocabulary and the [`Event`]/[`EventDraft`]
//!   rows.
//! - [`ids`] — the newtype identifiers.
//! - [`store`] — the [`Hub`]: open/migrate, the two idempotent append paths,
//!   `after_sequence` catch-up, and the task/run/approval/node helpers.
//! - [`protocol`] — the pure, serde-round-trippable wire-message contract
//!   between clients, the hub, and execution nodes. Types only; the coupling
//!   point that lets `stackhour-hub` and `stackhour-node` be built in parallel.
//!
//! The durable identity hierarchy is `Task -> Run -> ProviderSession`; a
//! provider process can crash, reconnect, or be replaced without changing the
//! identity or history of the task.

pub mod entities;
pub mod event;
pub mod ids;
pub mod protocol;
pub mod store;

pub use entities::{
    AccessPolicy, Approval, ConnectionStatus, Decision, Node, Run, RunStatus, Task, TaskStatus,
};
pub use event::{Event, EventDraft, EventKind, PROTOCOL_VERSION};
pub use ids::{ApprovalId, CommandId, EventId, NodeId, RunId, TaskId};
pub use protocol::{
    ClientCommand, HubToClient, HubToNode, HubWelcome, NodeHello, NodeToHub, NodeWork, ProtocolError,
    Subscribe, WIRE_PROTOCOL_VERSION,
};
pub use store::{AppendOutcome, EntityWrite, Hub, PendingDispatch};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::json;

    fn draft(hub_task: TaskId, node: &str, kind: EventKind) -> EventDraft {
        EventDraft::new(kind, hub_task, NodeId::from(node))
    }

    // --- event vocabulary round-trip --------------------------------------

    #[test]
    fn every_event_kind_serializes_to_its_dotted_string_and_back() {
        let expected = [
            (EventKind::TaskCreated, "task.created"),
            (EventKind::RunStarted, "run.started"),
            (EventKind::RunInterrupted, "run.interrupted"),
            (EventKind::RunCompleted, "run.completed"),
            (EventKind::RunFailed, "run.failed"),
            (EventKind::MessageUser, "message.user"),
            (EventKind::MessageAssistantDelta, "message.assistant.delta"),
            (
                EventKind::MessageAssistantCompleted,
                "message.assistant.completed",
            ),
            (EventKind::ApprovalRequested, "approval.requested"),
            (EventKind::ApprovalResolved, "approval.resolved"),
            (EventKind::NodeConnected, "node.connected"),
            (EventKind::NodeDisconnected, "node.disconnected"),
        ];
        // The table covers exactly the vocabulary, in order.
        assert_eq!(expected.len(), EventKind::ALL.len());
        for ((kind, dotted), all) in expected.into_iter().zip(EventKind::ALL) {
            assert_eq!(kind, all);
            // serde spelling, as_str(), and from_dotted() must all agree.
            assert_eq!(kind.as_str(), dotted);
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(dotted));
            assert_eq!(EventKind::from_dotted(dotted), Some(kind));
            let back: EventKind = serde_json::from_value(json!(dotted)).unwrap();
            assert_eq!(back, kind);
        }
        assert_eq!(EventKind::from_dotted("nope.nope"), None);
    }

    #[test]
    fn nodes_are_listed_in_stable_id_order() {
        let mut hub = Hub::open_in_memory().unwrap();
        for id in ["zeta", "alpha"] {
            hub.upsert_node(&Node {
                id: NodeId::from(id),
                label: id.to_string(),
                status: ConnectionStatus::Disconnected,
                software_version: "0.1.0".to_string(),
                capabilities: json!({"engines": ["claude"]}),
                last_seen_sequence: None,
            })
            .unwrap();
        }
        let ids: Vec<_> = hub
            .list_nodes()
            .unwrap()
            .into_iter()
            .map(|node| node.id.to_string())
            .collect();
        assert_eq!(ids, ["alpha", "zeta"]);
    }

    // --- append + catch-up ------------------------------------------------

    #[test]
    fn append_then_events_after_zero_is_ascending_gapless_from_one() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        for i in 0..5 {
            let d = draft(task, "laptop", EventKind::MessageUser).with_payload(json!({ "i": i }));
            hub.append_command(CommandId::new(), d).unwrap();
        }
        let events = hub.events_after(0).unwrap();
        let seqs: Vec<i64> = events.iter().map(|e| e.sequence).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
        // Payload survived the round trip in order.
        assert_eq!(events[2].payload, json!({ "i": 2 }));
        // A command-originated event carries its command id.
        assert!(events[0].command_id.is_some());
    }

    #[test]
    fn wire_cursors_flow_to_and_from_the_durable_sequence_without_casts() {
        // The wire cursor/version fields share the durable layer's `i64`, so a
        // hub can feed a subscribe cursor straight into `events_after`, echo the
        // durable `Event.sequence`/`AppendOutcome.sequence` back onto the wire,
        // and negotiate against `PROTOCOL_VERSION` with no `as` cast anywhere.
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        let outcome = hub
            .append_command(CommandId::new(), draft(task, "laptop", EventKind::RunStarted))
            .unwrap();

        // AppendOutcome.sequence (i64) -> the receipt field (i64), no cast.
        let receipt = HubToClient::accepted(CommandId::new(), outcome.sequence, outcome.event_id);
        let assigned = match receipt {
            HubToClient::CommandReceipt {
                assigned_sequence, ..
            } => assigned_sequence,
            other => panic!("wrong variant: {other:?}"),
        };
        assert_eq!(assigned, Some(1));

        // A Subscribe cursor (i64) drives events_after (i64) directly.
        let sub = Subscribe {
            token: None,
            after_sequence: Some(0),
        };
        let events = hub.events_after(sub.after_sequence.unwrap()).unwrap();
        let head = events.last().unwrap().sequence; // Event.sequence: i64

        // The head sequence rides the wire and returns identical.
        let ack = HubToClient::SubscribeAck {
            after_sequence: sub.after_sequence,
            head_sequence: head,
        };
        assert_eq!(
            ack,
            HubToClient::SubscribeAck {
                after_sequence: Some(0),
                head_sequence: 1,
            }
        );

        // The version negotiated on the wire is exactly the durable constant.
        assert_eq!(WIRE_PROTOCOL_VERSION, PROTOCOL_VERSION);
        assert!(HubWelcome::negotiate(PROTOCOL_VERSION).accepted);
    }

    #[test]
    fn replaying_the_same_command_id_inserts_one_event_and_returns_same_result() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        let cmd = CommandId::new();

        let first = hub
            .append_command(cmd, draft(task, "laptop", EventKind::RunStarted))
            .unwrap();
        assert!(first.created);

        // Same command id, even a different draft, must not create a new event.
        let second = hub
            .append_command(cmd, draft(task, "laptop", EventKind::RunCompleted))
            .unwrap();
        assert!(!second.created);
        assert_eq!(first.sequence, second.sequence);
        assert_eq!(first.event_id, second.event_id);

        let events = hub.events_after(0).unwrap();
        assert_eq!(events.len(), 1);
        // The original draft won; the replay was ignored.
        assert_eq!(events[0].kind, EventKind::RunStarted);
    }

    #[test]
    fn repeated_node_event_id_does_not_create_a_second_row() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        let eid = EventId::new();

        let first = hub
            .append_node_event(eid, draft(task, "laptop", EventKind::NodeConnected))
            .unwrap();
        assert!(first.created);

        let second = hub
            .append_node_event(eid, draft(task, "laptop", EventKind::NodeDisconnected))
            .unwrap();
        assert!(!second.created);
        assert_eq!(first.sequence, second.sequence);
        assert_eq!(first.event_id, second.event_id);

        let events = hub.events_after(0).unwrap();
        assert_eq!(events.len(), 1);
        // Node events carry no command id.
        assert!(events[0].command_id.is_none());
        assert_eq!(events[0].kind, EventKind::NodeConnected);
    }

    #[test]
    fn commands_and_node_events_share_one_gapless_sequence() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        let a = hub
            .append_command(CommandId::new(), draft(task, "n", EventKind::MessageUser))
            .unwrap();
        let b = hub
            .append_node_event(EventId::new(), draft(task, "n", EventKind::MessageAssistantDelta))
            .unwrap();
        let c = hub
            .append_command(
                CommandId::new(),
                draft(task, "n", EventKind::MessageAssistantCompleted),
            )
            .unwrap();
        assert_eq!((a.sequence, b.sequence, c.sequence), (1, 2, 3));
    }

    #[test]
    fn events_after_k_returns_exactly_the_ascending_tail() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        for _ in 0..6 {
            hub.append_command(CommandId::new(), draft(task, "n", EventKind::MessageUser))
                .unwrap();
        }
        let tail = hub.events_after(4).unwrap();
        let seqs: Vec<i64> = tail.iter().map(|e| e.sequence).collect();
        assert_eq!(seqs, vec![5, 6]);
        // Boundary is strict (> k, not >= k) and the far tail is empty.
        assert_eq!(hub.events_after(6).unwrap().len(), 0);
        assert_eq!(hub.events_after(100).unwrap().len(), 0);
        // The whole log, still gapless.
        assert_eq!(
            hub.events_after(0)
                .unwrap()
                .iter()
                .map(|e| e.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6]
        );
    }

    #[test]
    fn event_round_trips_all_optional_fields() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        let run = RunId::new();
        let occurred = Utc.with_ymd_and_hms(2026, 7, 25, 12, 0, 0).unwrap();
        let mut d = draft(task, "laptop", EventKind::MessageAssistantDelta).with_run(run);
        d.provider_session_id = Some("sess-1".to_string());
        d.occurred_at = occurred;
        d.payload = json!({ "text": "hi" });
        hub.append_node_event(EventId::new(), d).unwrap();

        let e = &hub.events_after(0).unwrap()[0];
        assert_eq!(e.task_id, task);
        assert_eq!(e.run_id, Some(run));
        assert_eq!(e.provider_session_id.as_deref(), Some("sess-1"));
        assert_eq!(e.node_id, NodeId::from("laptop"));
        assert_eq!(e.protocol_version, PROTOCOL_VERSION);
        assert_eq!(e.occurred_at, occurred);
        assert_eq!(e.payload, json!({ "text": "hi" }));
    }

    // --- approvals --------------------------------------------------------

    #[test]
    fn approval_request_then_resolve_sets_decision_actor_and_time() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        let run = hub
            .start_run(&task, &NodeId::from("laptop"), "acp", AccessPolicy::Supervised)
            .unwrap();

        let approval = hub
            .request_approval(
                &run.id,
                &task,
                "call-1",
                "write /etc/hosts",
                &["allow".to_string(), "deny".to_string()],
                None,
            )
            .unwrap();
        assert!(approval.is_pending());
        assert_eq!(approval.options, vec!["allow", "deny"]);
        assert_eq!(approval.resolved_by, None);

        let now = Utc::now();
        let resolved = hub
            .resolve_approval(&approval.id, Decision::Allowed, "nikita", now)
            .unwrap();
        assert_eq!(resolved.decision, Decision::Allowed);
        assert_eq!(resolved.resolved_by.as_deref(), Some("nikita"));
        assert!(resolved.resolved_at.is_some());
        assert!(!resolved.is_pending());

        // Persisted, not just returned.
        let fetched = hub.get_approval(&approval.id).unwrap().unwrap();
        assert_eq!(fetched.decision, Decision::Allowed);
        assert_eq!(fetched.resolved_by.as_deref(), Some("nikita"));
    }

    #[test]
    fn second_resolve_is_a_noop_returning_the_first_decision() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        let run = hub
            .start_run(&task, &NodeId::from("laptop"), "acp", AccessPolicy::Supervised)
            .unwrap();
        let approval = hub
            .request_approval(&run.id, &task, "call-1", "rm -rf", &[], None)
            .unwrap();

        let t1 = Utc.with_ymd_and_hms(2026, 7, 25, 10, 0, 0).unwrap();
        let first = hub
            .resolve_approval(&approval.id, Decision::Denied, "alice", t1)
            .unwrap();
        assert_eq!(first.decision, Decision::Denied);

        // A late, conflicting decision from another client must not overwrite.
        let t2 = Utc.with_ymd_and_hms(2026, 7, 25, 10, 5, 0).unwrap();
        let second = hub
            .resolve_approval(&approval.id, Decision::Allowed, "bob", t2)
            .unwrap();
        assert_eq!(second.decision, Decision::Denied);
        assert_eq!(second.resolved_by.as_deref(), Some("alice"));
        assert_eq!(second.resolved_at, Some(t1));

        let fetched = hub.get_approval(&approval.id).unwrap().unwrap();
        assert_eq!(fetched.decision, Decision::Denied);
        assert_eq!(fetched.resolved_by.as_deref(), Some("alice"));
    }

    #[test]
    fn expired_pending_approval_is_reported_expired() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        let run = hub
            .start_run(&task, &NodeId::from("laptop"), "acp", AccessPolicy::Supervised)
            .unwrap();
        let expiry = Utc.with_ymd_and_hms(2026, 7, 25, 12, 0, 0).unwrap();
        let approval = hub
            .request_approval(&run.id, &task, "call-1", "scope", &[], Some(expiry))
            .unwrap();

        assert!(!approval.is_expired(expiry - Duration::seconds(1)));
        assert!(approval.is_expired(expiry)); // at-or-after
        assert!(approval.is_expired(expiry + Duration::seconds(1)));

        // A resolved approval is never "expired" — its decision already stands.
        let resolved = hub
            .resolve_approval(&approval.id, Decision::Allowed, "nikita", expiry)
            .unwrap();
        assert!(!resolved.is_expired(expiry + Duration::hours(1)));

        // An approval with no expiry never expires.
        let forever = hub
            .request_approval(&run.id, &task, "call-2", "scope", &[], None)
            .unwrap();
        assert!(!forever.is_expired(Utc::now()));
    }

    #[test]
    fn resolving_to_pending_is_rejected() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("t").unwrap().id;
        let run = hub
            .start_run(&task, &NodeId::from("l"), "acp", AccessPolicy::Supervised)
            .unwrap();
        let a = hub.request_approval(&run.id, &task, "c", "s", &[], None).unwrap();
        assert!(hub
            .resolve_approval(&a.id, Decision::Pending, "x", Utc::now())
            .is_err());
    }

    // --- task / run / node persistence ------------------------------------

    #[test]
    fn task_and_run_round_trip() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = hub.create_task("build the thing").unwrap();
        assert_eq!(task.status, TaskStatus::Open);
        let fetched = hub.get_task(&task.id).unwrap().unwrap();
        assert_eq!(fetched, task);

        let run = hub
            .start_run(
                &task.id,
                &NodeId::from("dev-box"),
                "codex-acp",
                AccessPolicy::Automatic,
            )
            .unwrap();
        assert_eq!(run.status, RunStatus::Started);
        let fetched_run = hub.get_run(&run.id).unwrap().unwrap();
        assert_eq!(fetched_run, run);
        assert_eq!(fetched_run.access_policy, AccessPolicy::Automatic);

        assert!(hub.get_task(&TaskId::new()).unwrap().is_none());
        assert!(hub.get_run(&RunId::new()).unwrap().is_none());
    }

    #[test]
    fn node_upsert_updates_identity_and_cursor() {
        let mut hub = Hub::open_in_memory().unwrap();
        let mut node = Node {
            id: NodeId::from("laptop"),
            label: "Nikita's laptop".to_string(),
            status: ConnectionStatus::Connected,
            software_version: "0.1.0".to_string(),
            capabilities: json!({ "acp": true }),
            last_seen_sequence: None,
        };
        hub.upsert_node(&node).unwrap();
        assert_eq!(hub.get_node(&node.id).unwrap().unwrap(), node);

        // Reconnect: status flips and the cursor advances.
        node.status = ConnectionStatus::Disconnected;
        node.last_seen_sequence = Some(42);
        hub.upsert_node(&node).unwrap();
        let fetched = hub.get_node(&node.id).unwrap().unwrap();
        assert_eq!(fetched.status, ConnectionStatus::Disconnected);
        assert_eq!(fetched.last_seen_sequence, Some(42));
        assert_eq!(fetched.capabilities, json!({ "acp": true }));

        assert!(hub.get_node(&NodeId::from("ghost")).unwrap().is_none());
    }

    // --- on-disk open / persistence ---------------------------------------

    #[test]
    fn on_disk_hub_persists_across_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested").join("hub.db");
        let (task_id, seq) = {
            let mut hub = Hub::open(&path).unwrap();
            let task = hub.create_task("persist me").unwrap();
            let out = hub
                .append_command(CommandId::new(), draft(task.id, "laptop", EventKind::TaskCreated))
                .unwrap();
            (task.id, out.sequence)
        };
        // Reopen the same file: schema is idempotent and data survives.
        let hub = Hub::open(&path).unwrap();
        assert_eq!(hub.get_task(&task_id).unwrap().unwrap().title, "persist me");
        let events = hub.events_after(0).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, seq);
        assert_eq!(events[0].task_id, task_id);
    }

    #[test]
    fn sequence_continues_after_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hub.db");
        let task_id;
        {
            let mut hub = Hub::open(&path).unwrap();
            let task = hub.create_task("t").unwrap();
            task_id = task.id;
            hub.append_command(CommandId::new(), draft(task_id, "n", EventKind::MessageUser))
                .unwrap();
            hub.append_command(CommandId::new(), draft(task_id, "n", EventKind::MessageUser))
                .unwrap();
        }
        let mut hub = Hub::open(&path).unwrap();
        let out = hub
            .append_command(CommandId::new(), draft(task_id, "n", EventKind::MessageUser))
            .unwrap();
        // Gapless across the reopen: next sequence is MAX+1, not a reset to 1.
        assert_eq!(out.sequence, 3);
    }

    #[test]
    fn dispatch_is_stored_with_event_and_entity() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task = Task {
            id: TaskId::new(),
            title: "queued".to_string(),
            status: TaskStatus::Open,
            created_at: Utc::now(),
        };
        let command_id = CommandId::new();
        let node = NodeId::from("offline");
        let message = HubToNode::DispatchCommand {
            command_id,
            expires_at: None,
            work: NodeWork::StartRun {
                run_id: RunId::new(),
                task_id: task.id,
                engine: "claude".to_string(),
                access_policy: AccessPolicy::Automatic,
                workspace_path: None,
            },
        };
        let outcome = hub
            .append_command_bundle(
                command_id,
                draft(task.id, "offline", EventKind::TaskCreated),
                EntityWrite::Task(task.clone()),
                Some((&node, &message)),
            )
            .unwrap();
        assert!(outcome.created);
        assert_eq!(hub.get_task(&task.id).unwrap(), Some(task));
        let pending = hub.pending_dispatches(&node).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message, message);
    }

    #[test]
    fn replay_does_not_duplicate_a_pending_dispatch() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task_id = TaskId::new();
        let node = NodeId::from("n");
        let command_id = CommandId::new();
        let message = HubToNode::DispatchCommand {
            command_id,
            expires_at: None,
            work: NodeWork::InterruptRun { run_id: RunId::new() },
        };
        for _ in 0..2 {
            hub.append_command_bundle(
                command_id,
                draft(task_id, "n", EventKind::RunInterrupted),
                EntityWrite::None,
                Some((&node, &message)),
            )
            .unwrap();
        }
        assert_eq!(hub.events_after(0).unwrap().len(), 1);
        assert_eq!(hub.pending_dispatches(&node).unwrap().len(), 1);
    }

    #[test]
    fn acknowledgement_removes_dispatch_from_pending_list() {
        let mut hub = Hub::open_in_memory().unwrap();
        let task_id = TaskId::new();
        let node = NodeId::from("n");
        let command_id = CommandId::new();
        let message = HubToNode::DispatchCommand {
            command_id,
            expires_at: None,
            work: NodeWork::InterruptRun { run_id: RunId::new() },
        };
        hub.append_command_bundle(
            command_id,
            draft(task_id, "n", EventKind::RunInterrupted),
            EntityWrite::None,
            Some((&node, &message)),
        )
        .unwrap();
        assert!(hub.acknowledge_dispatch(&command_id).unwrap());
        assert!(!hub.acknowledge_dispatch(&command_id).unwrap());
        assert!(hub.pending_dispatches(&node).unwrap().is_empty());
    }

    #[test]
    fn pending_dispatch_survives_database_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hub.db");
        let node = NodeId::from("n");
        let command_id = CommandId::new();
        let message = HubToNode::DispatchCommand {
            command_id,
            expires_at: None,
            work: NodeWork::InterruptRun { run_id: RunId::new() },
        };
        {
            let mut hub = Hub::open(&path).unwrap();
            hub.append_command_bundle(
                command_id,
                draft(TaskId::new(), "n", EventKind::RunInterrupted),
                EntityWrite::None,
                Some((&node, &message)),
            )
            .unwrap();
        }
        let hub = Hub::open(&path).unwrap();
        assert_eq!(hub.pending_dispatches(&node).unwrap()[0].message, message);
    }
}
