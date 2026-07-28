//! The SQLite persistence layer and the load-bearing [`Hub`] API.
//!
//! The hub owns one append-only `events` table with a single, hub-assigned,
//! gapless `sequence` starting at 1; idempotent `command_receipts`
//! (`command_id` -> the assigned sequence/event id); and the durable `tasks`,
//! `runs`, `approvals`, and `nodes` rows. Every write that assigns a sequence
//! does so inside one `BEGIN IMMEDIATE` transaction, so the sequence is never a
//! separate read-then-write and concurrent callers cannot interleave.
//!
//! Two append paths, matching the reconnect/idempotency rules:
//!
//! - [`Hub::append_command`] — a client mutation carrying a `CommandId`. If the
//!   command already has a receipt, the *same* stored result is returned and no
//!   new event is written. Otherwise a sequence and `event_id` are assigned and
//!   the event + receipt are inserted atomically.
//! - [`Hub::append_node_event`] — a node-originated event carrying its own
//!   stable `EventId` and no command. De-duplicated by `event_id` *before* any
//!   sequence is assigned.

use crate::entities::{
    AccessPolicy, Approval, ConnectionStatus, Decision, Node, Run, RunStatus, Task, TaskStatus,
};
use crate::event::{Event, EventDraft, EventKind};
use crate::ids::{ApprovalId, CommandId, EventId, NodeId, RunId, TaskId};
use crate::protocol::HubToNode;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::types::Type as SqlType;
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde_json::Value;
use stackhour_core::{Error, Result};
use std::path::Path;
use std::str::FromStr;

/// The outcome of an append: the assigned (or already-stored) `sequence` and
/// `event_id`, plus whether this call actually created a new event.
///
/// A replayed command or a duplicate node event returns `created == false` with
/// the original `sequence`/`event_id`, so retries are safely idempotent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendOutcome {
    /// The global sequence of the event.
    pub sequence: i64,
    /// The stable id of the event.
    pub event_id: EventId,
    /// `true` if this call inserted a new event; `false` if it returned an
    /// already-stored result (idempotent replay / de-duplication).
    pub created: bool,
}

/// A durable entity a client command mints alongside its event, persisted in
/// the *same* `BEGIN IMMEDIATE` transaction as the event + command receipt.
///
/// This is the atomicity contract of [`Hub::append_command_with`]: the entity
/// row and the event that announces it commit together, all-or-nothing. A crash
/// or I/O error between "event written" and "row written" can never leave a
/// durable `task.created`/`run.started` event whose backing row is missing —
/// and because the receipt commits in the same unit, a rolled-back append is
/// cleanly re-created by a client retry rather than being blocked by a receipt
/// with no entity behind it.
pub enum EntityWrite {
    /// The command mints no durable entity (the common case).
    None,
    /// A new [`Task`] row (from `CreateTask`).
    Task(Task),
    /// A new [`Run`] row (from `StartRun`).
    Run(Run),
}

/// One hub-to-node command that remains pending until the node acknowledges it.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingDispatch {
    pub command_id: CommandId,
    pub node_id: NodeId,
    pub message: HubToNode,
    pub created_at: DateTime<Utc>,
}

/// The durable control-plane store.
pub struct Hub {
    conn: Connection,
}

/// `rusqlite::Error` -> the workspace error type (message forwarded verbatim,
/// matching `stackhour-store`'s convention).
fn sql_err(e: rusqlite::Error) -> Error {
    Error::msg(e.to_string())
}

/// Wrap a domain parse failure as a rusqlite column-conversion error so it can
/// surface out of a `query_map` closure.
fn conv_err(msg: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, SqlType::Text, Box::new(Error::msg(msg)))
}

/// RFC3339 (UTC, nanosecond precision) text form used for every timestamp
/// column. Nanosecond precision makes the write/read round trip exact, so a
/// returned entity equals the one later fetched from the db.
fn fmt_time(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Parse a stored RFC3339 timestamp back to UTC.
fn parse_time(s: &str) -> std::result::Result<DateTime<Utc>, rusqlite::Error> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| conv_err(format!("bad timestamp {s:?}: {e}")))
}

/// The first control-plane schema. `sequence` is an explicit `INTEGER PRIMARY
/// KEY` assigned as `MAX+1` inside a write transaction — not `AUTOINCREMENT` —
/// so the tail is provably gapless.
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS events (
      sequence            INTEGER PRIMARY KEY,
      event_id            TEXT NOT NULL UNIQUE,
      command_id          TEXT,
      kind                TEXT NOT NULL,
      task_id             TEXT NOT NULL,
      run_id              TEXT,
      provider_session_id TEXT,
      node_id             TEXT NOT NULL,
      protocol_version    INTEGER NOT NULL,
      occurred_at         TEXT NOT NULL,
      hub_received_at     TEXT NOT NULL,
      payload             TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS events_task ON events (task_id, sequence);

    CREATE TABLE IF NOT EXISTS command_receipts (
      command_id  TEXT PRIMARY KEY,
      sequence    INTEGER NOT NULL,
      event_id    TEXT NOT NULL,
      received_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS tasks (
      task_id    TEXT PRIMARY KEY,
      title      TEXT NOT NULL,
      status     TEXT NOT NULL,
      created_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS runs (
      run_id         TEXT PRIMARY KEY,
      task_id        TEXT NOT NULL,
      node_id        TEXT NOT NULL,
      engine         TEXT NOT NULL,
      access_policy  TEXT NOT NULL,
      workspace_path TEXT,
      status         TEXT NOT NULL,
      started_at     TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS approvals (
      approval_id  TEXT PRIMARY KEY,
      run_id       TEXT NOT NULL,
      task_id      TEXT NOT NULL,
      tool_call_id TEXT NOT NULL,
      scope        TEXT NOT NULL,
      options      TEXT NOT NULL,
      expires_at   TEXT,
      decision     TEXT NOT NULL,
      resolved_by  TEXT,
      created_at   TEXT NOT NULL,
      resolved_at  TEXT
    );

    CREATE TABLE IF NOT EXISTS nodes (
      node_id            TEXT PRIMARY KEY,
      label              TEXT NOT NULL,
      status             TEXT NOT NULL,
      software_version   TEXT NOT NULL,
      capabilities       TEXT NOT NULL,
      last_seen_sequence INTEGER
    );

    CREATE TABLE IF NOT EXISTS pending_dispatches (
      command_id TEXT PRIMARY KEY,
      node_id     TEXT NOT NULL,
      message     TEXT NOT NULL,
      created_at  TEXT NOT NULL,
      acked_at    TEXT
    );
    CREATE INDEX IF NOT EXISTS pending_dispatches_node
      ON pending_dispatches (node_id, created_at);
";

/// Latest control-plane database schema understood by this binary.
pub const LATEST_HUB_SCHEMA_VERSION: i64 = 1;

const MIGRATIONS_DDL: &str = "
    CREATE TABLE IF NOT EXISTS stackhour_hub_schema_migrations (
      version    INTEGER PRIMARY KEY,
      name       TEXT NOT NULL,
      applied_at TEXT NOT NULL
    );
";

const MIGRATIONS: [(i64, &str, &str); LATEST_HUB_SCHEMA_VERSION as usize] =
    [(1, "create control-plane schema", SCHEMA)];

fn applied_migrations(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn
        .prepare("SELECT version, name FROM stackhour_hub_schema_migrations ORDER BY version")
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(sql_err)?;
    let mut applied = Vec::new();
    for row in rows {
        applied.push(row.map_err(sql_err)?);
    }
    Ok(applied)
}

fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATIONS_DDL).map_err(sql_err)?;

    let applied = applied_migrations(conn)?;
    for (index, (version, name)) in applied.iter().enumerate() {
        let expected = MIGRATIONS.get(index).ok_or_else(|| {
            Error::msg(format!(
                "control-plane database schema version {version} is newer than supported version {LATEST_HUB_SCHEMA_VERSION}"
            ))
        })?;
        if *version != expected.0 || name != expected.1 {
            return Err(Error::msg(format!(
                "invalid control-plane migration history at version {version}: expected {} ({:?}), found {version} ({name:?})",
                expected.0, expected.1
            )));
        }
    }

    for (version, name, sql) in MIGRATIONS.iter().skip(applied.len()) {
        conn.execute_batch("BEGIN IMMEDIATE").map_err(sql_err)?;
        let result = (|| -> Result<()> {
            conn.execute_batch(sql).map_err(sql_err)?;
            conn.execute(
                "INSERT INTO stackhour_hub_schema_migrations
                   (version, name, applied_at)
                 VALUES (?1, ?2, ?3)",
                params![version, name, fmt_time(Utc::now())],
            )
            .map_err(sql_err)?;
            conn.execute_batch("COMMIT").map_err(sql_err)
        })();
        if let Err(error) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(Error::msg(format!(
                "control-plane database migration {version} ({name}) failed: {error}"
            )));
        }
    }
    Ok(())
}

impl Hub {
    /// Open (creating parent dirs as needed) and migrate an on-disk hub db.
    /// Sets `busy_timeout` and WAL, then runs the schema, exactly like
    /// `stackhour-store::db::open_db`.
    pub fn open(path: impl AsRef<Path>) -> Result<Hub> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path).map_err(sql_err)?;
        conn.pragma_update(None, "busy_timeout", 5000i64)
            .map_err(sql_err)?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .map_err(sql_err)?;
        run_migrations(&conn)?;
        Ok(Hub { conn })
    }

    /// An in-memory hub for tests. Same schema, no journal file.
    pub fn open_in_memory() -> Result<Hub> {
        let conn = Connection::open_in_memory().map_err(sql_err)?;
        conn.pragma_update(None, "busy_timeout", 5000i64)
            .map_err(sql_err)?;
        run_migrations(&conn)?;
        Ok(Hub { conn })
    }

    /// Return the latest migration version recorded in this database.
    pub fn schema_version(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0)
                 FROM stackhour_hub_schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(sql_err)
    }

    // --- the event log ----------------------------------------------------

    /// Append a client-command-originated event idempotently, minting no durable
    /// entity. Shorthand for [`Hub::append_command_with`] with
    /// [`EntityWrite::None`].
    pub fn append_command(&mut self, command_id: CommandId, draft: EventDraft) -> Result<AppendOutcome> {
        self.append_command_with(command_id, draft, EntityWrite::None)
    }

    /// Append a client-command-originated event idempotently, persisting the
    /// entity the command mints in the *same* transaction as the event.
    ///
    /// In one `BEGIN IMMEDIATE` transaction: if `command_id` already has a
    /// receipt, return the same stored `(sequence, event_id)` and write nothing
    /// (not even `entity`); otherwise assign the next sequence and a fresh
    /// `event_id`, insert the event row, the command receipt, *and* `entity`
    /// together, and commit them as one atomic unit before returning.
    ///
    /// Persisting the entity inside this transaction is the invariant that makes
    /// the durable `Task`/`Run` row exist the instant the event does: a failure
    /// writing the entity rolls back the event and the receipt too, so a client
    /// retry cleanly re-creates all three rather than finding an orphan event
    /// behind a committed receipt (see [`EntityWrite`]).
    pub fn append_command_with(
        &mut self,
        command_id: CommandId,
        draft: EventDraft,
        entity: EntityWrite,
    ) -> Result<AppendOutcome> {
        self.append_command_bundle(command_id, draft, entity, None)
    }

    /// Append a command event, its entity, and its node dispatch atomically.
    ///
    /// A connected hub can send the dispatch after this transaction commits.
    /// An offline node gets the same dispatch when it reconnects. A replayed
    /// client command does not create a second dispatch.
    pub fn append_command_bundle(
        &mut self,
        command_id: CommandId,
        draft: EventDraft,
        entity: EntityWrite,
        dispatch: Option<(&NodeId, &HubToNode)>,
    ) -> Result<AppendOutcome> {
        let received_at = Utc::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        // Existing receipt -> return the same result, no new effect (the entity
        // was already written by the original call, so it is not re-inserted).
        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT sequence, event_id FROM command_receipts WHERE command_id = ?1",
                params![command_id.to_string()],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((sequence, event_id)) = existing {
            let event_id = EventId::from_str(&event_id)?;
            // Read-only path: nothing to commit.
            return Ok(AppendOutcome {
                sequence,
                event_id,
                created: false,
            });
        }

        let event_id = EventId::new();
        let sequence = next_sequence(&tx)?;
        insert_event(&tx, sequence, event_id, Some(command_id), &draft, received_at)?;
        tx.execute(
            "INSERT INTO command_receipts (command_id, sequence, event_id, received_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                command_id.to_string(),
                sequence,
                event_id.to_string(),
                fmt_time(received_at)
            ],
        )
        .map_err(sql_err)?;
        // The entity commits with the event + receipt, atomically.
        match &entity {
            EntityWrite::None => {}
            EntityWrite::Task(task) => insert_task_row(&tx, task)?,
            EntityWrite::Run(run) => insert_run_row(&tx, run)?,
        }
        if let Some((node_id, message)) = dispatch {
            tx.execute(
                "INSERT INTO pending_dispatches
                   (command_id, node_id, message, created_at, acked_at)
                 VALUES (?1, ?2, ?3, ?4, NULL)",
                params![
                    command_id.to_string(),
                    node_id.as_str(),
                    serde_json::to_string(message)?,
                    fmt_time(received_at),
                ],
            )
            .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;
        Ok(AppendOutcome {
            sequence,
            event_id,
            created: true,
        })
    }

    /// Get all unacknowledged commands for one node, in creation order.
    pub fn pending_dispatches(&self, node_id: &NodeId) -> Result<Vec<PendingDispatch>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT command_id, node_id, message, created_at
                 FROM pending_dispatches
                 WHERE node_id = ?1 AND acked_at IS NULL
                 ORDER BY rowid ASC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![node_id.as_str()], |row| {
                let command = row.get::<_, String>(0)?;
                let message = row.get::<_, String>(2)?;
                Ok(PendingDispatch {
                    command_id: req_id::<CommandId>(command)?,
                    node_id: NodeId(row.get::<_, String>(1)?),
                    message: serde_json::from_str(&message)
                        .map_err(|e| conv_err(format!("bad pending dispatch: {e}")))?,
                    created_at: parse_time(&row.get::<_, String>(3)?)?,
                })
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(sql_err)?);
        }
        Ok(out)
    }

    /// Mark a dispatched command as accepted by its node.
    pub fn acknowledge_dispatch(&mut self, command_id: &CommandId) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "UPDATE pending_dispatches SET acked_at = ?1
                 WHERE command_id = ?2 AND acked_at IS NULL",
                params![fmt_time(Utc::now()), command_id.to_string()],
            )
            .map_err(sql_err)?;
        Ok(changed == 1)
    }

    /// The current head sequence: `MAX(sequence)` over the event log, or `0` when
    /// empty. A cheap single-row query so a subscribing client's catch-up scan
    /// reads only the tail it needs (`events_after(cursor)`), never the whole
    /// log just to learn the head.
    pub fn head_sequence(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(sql_err)
    }

    /// Append a node-originated event, de-duplicated by its stable `event_id`
    /// *before* a sequence is assigned. A retried delivery of the same
    /// `event_id` returns the original sequence and assigns nothing new.
    pub fn append_node_event(&mut self, event_id: EventId, draft: EventDraft) -> Result<AppendOutcome> {
        let received_at = Utc::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        let existing: Option<i64> = tx
            .query_row(
                "SELECT sequence FROM events WHERE event_id = ?1",
                params![event_id.to_string()],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some(sequence) = existing {
            return Ok(AppendOutcome {
                sequence,
                event_id,
                created: false,
            });
        }

        let sequence = next_sequence(&tx)?;
        insert_event(&tx, sequence, event_id, None, &draft, received_at)?;
        tx.commit().map_err(sql_err)?;
        Ok(AppendOutcome {
            sequence,
            event_id,
            created: true,
        })
    }

    /// The gapless event tail with `sequence > after`, ascending — the
    /// `after_sequence` catch-up query a reconnecting subscriber replays before
    /// switching to live delivery. Pass `0` for the whole log.
    pub fn events_after(&self, after: i64) -> Result<Vec<Event>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT sequence, event_id, command_id, kind, task_id, run_id,
                        provider_session_id, node_id, protocol_version,
                        occurred_at, hub_received_at, payload
                 FROM events WHERE sequence > ?1 ORDER BY sequence ASC",
            )
            .map_err(sql_err)?;
        let rows = stmt.query_map(params![after], row_to_event).map_err(sql_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(sql_err)?);
        }
        Ok(out)
    }

    // --- task / run helpers ----------------------------------------------

    /// Create a durable task (status `Open`) with a fresh id and return it.
    pub fn create_task(&mut self, title: &str) -> Result<Task> {
        let task = Task {
            id: TaskId::new(),
            title: title.to_string(),
            status: TaskStatus::Open,
            created_at: Utc::now(),
        };
        self.insert_task(&task)?;
        Ok(task)
    }

    /// Persist a caller-built [`Task`] row verbatim.
    ///
    /// The hub uses this to store the durable task whose id it already stamped
    /// onto a `task.created` event, so the event and the row share one id. Fails
    /// if a row with the same id already exists (a genuinely-new command mints a
    /// fresh id, so this never collides in normal use).
    pub fn insert_task(&mut self, task: &Task) -> Result<()> {
        insert_task_row(&self.conn, task)
    }

    /// Fetch a task by id.
    pub fn get_task(&self, id: &TaskId) -> Result<Option<Task>> {
        self.conn
            .query_row(
                "SELECT task_id, title, status, created_at FROM tasks WHERE task_id = ?1",
                params![id.to_string()],
                row_to_task,
            )
            .optional()
            .map_err(sql_err)
    }

    /// Start a run (status `Started`) with a fresh id for a task on a node and
    /// return it.
    pub fn start_run(
        &mut self,
        task_id: &TaskId,
        node_id: &NodeId,
        engine: &str,
        access_policy: AccessPolicy,
    ) -> Result<Run> {
        let run = Run {
            id: RunId::new(),
            task_id: *task_id,
            node_id: node_id.clone(),
            engine: engine.to_string(),
            access_policy,
            workspace_path: None,
            status: RunStatus::Started,
            started_at: Utc::now(),
        };
        self.insert_run(&run)?;
        Ok(run)
    }

    /// Persist a caller-built [`Run`] row verbatim.
    ///
    /// The counterpart to [`Hub::insert_task`]: the hub stamps a `run.started`
    /// event with a run id it minted, then stores the matching row through this
    /// so the durable run exists alongside the event.
    pub fn insert_run(&mut self, run: &Run) -> Result<()> {
        insert_run_row(&self.conn, run)
    }

    /// Fetch a run by id.
    pub fn get_run(&self, id: &RunId) -> Result<Option<Run>> {
        self.conn
            .query_row(
                "SELECT run_id, task_id, node_id, engine, access_policy, workspace_path, status, started_at
                 FROM runs WHERE run_id = ?1",
                params![id.to_string()],
                row_to_run,
            )
            .optional()
            .map_err(sql_err)
    }

    // --- approvals --------------------------------------------------------

    /// Persist a pending approval request and return it. Durable *before* it is
    /// shown in any client.
    pub fn request_approval(
        &mut self,
        run_id: &RunId,
        task_id: &TaskId,
        tool_call_id: &str,
        scope: &str,
        options: &[String],
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<Approval> {
        let approval = Approval {
            id: ApprovalId::new(),
            run_id: *run_id,
            task_id: *task_id,
            tool_call_id: tool_call_id.to_string(),
            scope: scope.to_string(),
            options: options.to_vec(),
            expires_at,
            decision: Decision::Pending,
            resolved_by: None,
            created_at: Utc::now(),
            resolved_at: None,
        };
        let options_json = serde_json::to_string(&approval.options)?;
        self.conn
            .execute(
                "INSERT INTO approvals
                   (approval_id, run_id, task_id, tool_call_id, scope, options,
                    expires_at, decision, resolved_by, created_at, resolved_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, NULL)",
                params![
                    approval.id.to_string(),
                    approval.run_id.to_string(),
                    approval.task_id.to_string(),
                    approval.tool_call_id,
                    approval.scope,
                    options_json,
                    approval.expires_at.map(fmt_time),
                    approval.decision.as_str(),
                    fmt_time(approval.created_at),
                ],
            )
            .map_err(sql_err)?;
        Ok(approval)
    }

    /// Resolve a pending approval to `Allowed` or `Denied` by `actor` at `now`.
    ///
    /// Idempotent: a duplicate or late resolve of an already-resolved approval
    /// is a no-op that returns the *existing* decision unchanged. Runs in one
    /// `BEGIN IMMEDIATE` transaction so concurrent resolves cannot both win.
    pub fn resolve_approval(
        &mut self,
        approval_id: &ApprovalId,
        decision: Decision,
        actor: &str,
        now: DateTime<Utc>,
    ) -> Result<Approval> {
        if decision == Decision::Pending {
            return Err(Error::msg("cannot resolve an approval to pending"));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let mut current = tx
            .query_row(
                "SELECT approval_id, run_id, task_id, tool_call_id, scope, options,
                        expires_at, decision, resolved_by, created_at, resolved_at
                 FROM approvals WHERE approval_id = ?1",
                params![approval_id.to_string()],
                row_to_approval,
            )
            .optional()
            .map_err(sql_err)?
            .ok_or_else(|| Error::msg(format!("unknown approval: {approval_id}")))?;

        // Already resolved -> no-op, return the first decision.
        if !current.is_pending() {
            return Ok(current);
        }

        tx.execute(
            "UPDATE approvals SET decision = ?1, resolved_by = ?2, resolved_at = ?3
             WHERE approval_id = ?4",
            params![decision.as_str(), actor, fmt_time(now), approval_id.to_string()],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;

        current.decision = decision;
        current.resolved_by = Some(actor.to_string());
        current.resolved_at = Some(now);
        Ok(current)
    }

    /// Fetch an approval by id.
    pub fn get_approval(&self, id: &ApprovalId) -> Result<Option<Approval>> {
        self.conn
            .query_row(
                "SELECT approval_id, run_id, task_id, tool_call_id, scope, options,
                        expires_at, decision, resolved_by, created_at, resolved_at
                 FROM approvals WHERE approval_id = ?1",
                params![id.to_string()],
                row_to_approval,
            )
            .optional()
            .map_err(sql_err)
    }

    // --- nodes ------------------------------------------------------------

    /// Insert or replace a node's identity, capability snapshot, and last-seen
    /// cursor.
    pub fn upsert_node(&mut self, node: &Node) -> Result<()> {
        let capabilities = serde_json::to_string(&node.capabilities)?;
        self.conn
            .execute(
                "INSERT INTO nodes
                   (node_id, label, status, software_version, capabilities, last_seen_sequence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(node_id) DO UPDATE SET
                   label=excluded.label, status=excluded.status,
                   software_version=excluded.software_version,
                   capabilities=excluded.capabilities,
                   last_seen_sequence=excluded.last_seen_sequence",
                params![
                    node.id.as_str(),
                    node.label,
                    node.status.as_str(),
                    node.software_version,
                    capabilities,
                    node.last_seen_sequence,
                ],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Fetch a node by id.
    pub fn get_node(&self, id: &NodeId) -> Result<Option<Node>> {
        self.conn
            .query_row(
                "SELECT node_id, label, status, software_version, capabilities, last_seen_sequence
                 FROM nodes WHERE node_id = ?1",
                params![id.as_str()],
                row_to_node,
            )
            .optional()
            .map_err(sql_err)
    }

    /// List all known nodes in stable node-id order.
    pub fn list_nodes(&self) -> Result<Vec<Node>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT node_id, label, status, software_version, capabilities, last_seen_sequence
                 FROM nodes ORDER BY node_id",
            )
            .map_err(sql_err)?;
        let rows = statement.query_map([], row_to_node).map_err(sql_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)
    }
}

/// `MAX(sequence)+1` inside the current write transaction — the single point of
/// sequence assignment. Gapless because a `BEGIN IMMEDIATE` write lock is held.
fn next_sequence(tx: &rusqlite::Transaction<'_>) -> Result<i64> {
    tx.query_row("SELECT COALESCE(MAX(sequence), 0) + 1 FROM events", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(sql_err)
}

/// Insert one fully-stamped event row.
fn insert_event(
    tx: &rusqlite::Transaction<'_>,
    sequence: i64,
    event_id: EventId,
    command_id: Option<CommandId>,
    draft: &EventDraft,
    received_at: DateTime<Utc>,
) -> Result<()> {
    let payload = serde_json::to_string(&draft.payload)?;
    tx.execute(
        "INSERT INTO events
           (sequence, event_id, command_id, kind, task_id, run_id,
            provider_session_id, node_id, protocol_version,
            occurred_at, hub_received_at, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            sequence,
            event_id.to_string(),
            command_id.map(|c| c.to_string()),
            draft.kind.as_str(),
            draft.task_id.to_string(),
            draft.run_id.map(|r| r.to_string()),
            draft.provider_session_id,
            draft.node_id.as_str(),
            draft.protocol_version,
            fmt_time(draft.occurred_at),
            fmt_time(received_at),
            payload,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

// --- row mappers -----------------------------------------------------------

fn opt_id<T: FromStr<Err = Error>>(v: Option<String>) -> std::result::Result<Option<T>, rusqlite::Error> {
    match v {
        Some(s) => T::from_str(&s)
            .map(Some)
            .map_err(|e| conv_err(e.message().to_string())),
        None => Ok(None),
    }
}

fn req_id<T: FromStr<Err = Error>>(s: String) -> std::result::Result<T, rusqlite::Error> {
    T::from_str(&s).map_err(|e| conv_err(e.message().to_string()))
}

fn row_to_event(row: &Row<'_>) -> rusqlite::Result<Event> {
    let kind_s: String = row.get("kind")?;
    let kind =
        EventKind::from_dotted(&kind_s).ok_or_else(|| conv_err(format!("unknown event kind: {kind_s}")))?;
    let payload_s: String = row.get("payload")?;
    let payload: Value =
        serde_json::from_str(&payload_s).map_err(|e| conv_err(format!("bad payload: {e}")))?;
    Ok(Event {
        sequence: row.get("sequence")?,
        event_id: req_id::<EventId>(row.get("event_id")?)?,
        command_id: opt_id::<CommandId>(row.get("command_id")?)?,
        kind,
        task_id: req_id::<TaskId>(row.get("task_id")?)?,
        run_id: opt_id::<RunId>(row.get("run_id")?)?,
        provider_session_id: row.get("provider_session_id")?,
        node_id: NodeId(row.get::<_, String>("node_id")?),
        protocol_version: row.get("protocol_version")?,
        occurred_at: parse_time(&row.get::<_, String>("occurred_at")?)?,
        hub_received_at: parse_time(&row.get::<_, String>("hub_received_at")?)?,
        payload,
    })
}

fn row_to_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: req_id::<TaskId>(row.get("task_id")?)?,
        title: row.get("title")?,
        status: TaskStatus::from_db(&row.get::<_, String>("status")?)
            .map_err(|e| conv_err(e.message().to_string()))?,
        created_at: parse_time(&row.get::<_, String>("created_at")?)?,
    })
}

fn row_to_run(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: req_id::<RunId>(row.get("run_id")?)?,
        task_id: req_id::<TaskId>(row.get("task_id")?)?,
        node_id: NodeId(row.get::<_, String>("node_id")?),
        engine: row.get("engine")?,
        access_policy: AccessPolicy::from_db(&row.get::<_, String>("access_policy")?)
            .map_err(|e| conv_err(e.message().to_string()))?,
        workspace_path: row.get("workspace_path")?,
        status: RunStatus::from_db(&row.get::<_, String>("status")?)
            .map_err(|e| conv_err(e.message().to_string()))?,
        started_at: parse_time(&row.get::<_, String>("started_at")?)?,
    })
}

/// Insert a durable `tasks` row. Takes `&Connection` so it runs against either
/// the live connection or an in-flight `Transaction` (deref-coerced), letting
/// the row commit atomically with the event that minted its id.
fn insert_task_row(conn: &Connection, task: &Task) -> Result<()> {
    conn.execute(
        "INSERT INTO tasks (task_id, title, status, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            task.id.to_string(),
            task.title,
            task.status.as_str(),
            fmt_time(task.created_at)
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Insert a durable `runs` row (including the optional `workspace_path`). Same
/// connection-or-transaction contract as [`insert_task_row`].
fn insert_run_row(conn: &Connection, run: &Run) -> Result<()> {
    conn.execute(
        "INSERT INTO runs (run_id, task_id, node_id, engine, access_policy, workspace_path, status, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            run.id.to_string(),
            run.task_id.to_string(),
            run.node_id.as_str(),
            run.engine,
            run.access_policy.as_str(),
            run.workspace_path,
            run.status.as_str(),
            fmt_time(run.started_at)
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn row_to_approval(row: &Row<'_>) -> rusqlite::Result<Approval> {
    let options_s: String = row.get("options")?;
    let options: Vec<String> =
        serde_json::from_str(&options_s).map_err(|e| conv_err(format!("bad options: {e}")))?;
    let expires_at: Option<String> = row.get("expires_at")?;
    let resolved_at: Option<String> = row.get("resolved_at")?;
    Ok(Approval {
        id: req_id::<ApprovalId>(row.get("approval_id")?)?,
        run_id: req_id::<RunId>(row.get("run_id")?)?,
        task_id: req_id::<TaskId>(row.get("task_id")?)?,
        tool_call_id: row.get("tool_call_id")?,
        scope: row.get("scope")?,
        options,
        expires_at: expires_at.map(|s| parse_time(&s)).transpose()?,
        decision: Decision::from_db(&row.get::<_, String>("decision")?)
            .map_err(|e| conv_err(e.message().to_string()))?,
        resolved_by: row.get("resolved_by")?,
        created_at: parse_time(&row.get::<_, String>("created_at")?)?,
        resolved_at: resolved_at.map(|s| parse_time(&s)).transpose()?,
    })
}

fn row_to_node(row: &Row<'_>) -> rusqlite::Result<Node> {
    let capabilities_s: String = row.get("capabilities")?;
    let capabilities: Value =
        serde_json::from_str(&capabilities_s).map_err(|e| conv_err(format!("bad capabilities: {e}")))?;
    Ok(Node {
        id: NodeId(row.get::<_, String>("node_id")?),
        label: row.get("label")?,
        status: ConnectionStatus::from_db(&row.get::<_, String>("status")?)
            .map_err(|e| conv_err(e.message().to_string()))?,
        software_version: row.get("software_version")?,
        capabilities,
        last_seen_sequence: row.get("last_seen_sequence")?,
    })
}
