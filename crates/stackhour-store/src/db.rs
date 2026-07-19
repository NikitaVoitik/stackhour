//! Open/migrate stackhour.db, ingest heartbeats, agent status, wakatime days,
//! and row range/recent queries.
//!
//! Migration parity notes: the actor backfill UPDATE is ported as VERBATIM
//! SQL (GLOB is case-sensitive — do not translate to a Rust regex); the
//! per-column PRAGMA table_info checks run inside a single BEGIN IMMEDIATE
//! resumable transaction; DROP INDEX hb_dedupe / CREATE hb_dedupe2 (6-col
//! incl. actor) + hb_time run on EVERY open.

use crate::Heartbeat;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use stackhour_core::jsnum::{js_display, js_number, js_string_or, js_truthy, json_num, nonneg};
use stackhour_core::{Error, Result};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The ack shape returned by [`upsert_agent_status`] (JS:
/// `{ machine, serverTime, clockSkewSeconds }` — skew deliberately unclamped).
#[derive(Debug, Clone, PartialEq)]
pub struct StatusAck {
    pub machine: String,
    pub server_time: f64,
    pub clock_skew_seconds: f64,
}

/// `rusqlite::Error` -> the workspace error type. The SQLite message is what
/// the JS implementation surfaced too (node:sqlite rethrows the driver
/// message), so it is forwarded verbatim.
fn sql_err(e: rusqlite::Error) -> Error {
    Error::msg(e.to_string())
}

/// `Date.now() / 1000` — integer milliseconds divided by 1000, exactly like JS
/// (so the value keeps millisecond resolution and no sub-millisecond noise).
fn now_seconds() -> f64 {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    ms as f64 / 1000.0
}

/// JS `String.prototype.trim()`: ECMAScript *StrWhiteSpace* — WhiteSpace
/// (TAB VT FF SP NBSP ZWNBSP + Zs) plus LineTerminator (LF CR LS PS). Rust's
/// `char::is_whitespace` matches that set except it also includes U+0085 NEL
/// (which JS does NOT trim) and excludes U+FEFF ZWNBSP (which JS DOES trim).
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c == '\u{FEFF}' || (c.is_whitespace() && c != '\u{0085}'))
}

/// JS `String.prototype.slice(0, max)` — the limit counts UTF-16 code units,
/// not chars or bytes. A cut that would land inside a surrogate pair drops the
/// whole pair rather than emitting a lone surrogate (unrepresentable in Rust;
/// only reachable with >100/200 code units of astral text).
fn slice_utf16(s: &str, max: usize) -> String {
    let mut units = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let w = c.len_utf16();
        if units + w > max {
            break;
        }
        units += w;
        out.push(c);
    }
    out
}

/// Percent-encode a filesystem path for a SQLite `file:` URI.
fn uri_encode_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The heartbeats table DDL (src/db.js, comments stripped).
const HEARTBEATS_DDL: &str = "
    CREATE TABLE IF NOT EXISTS heartbeats (
      id INTEGER PRIMARY KEY,
      time REAL NOT NULL,
      machine TEXT NOT NULL,
      source TEXT NOT NULL,
      project TEXT NOT NULL,
      entity TEXT NOT NULL,
      entity_type TEXT NOT NULL DEFAULT 'file',
      category TEXT NOT NULL DEFAULT 'coding',
      language TEXT,
      branch TEXT,
      is_write INTEGER NOT NULL DEFAULT 0,
      actor TEXT NOT NULL DEFAULT 'human',
      tokens_in INTEGER NOT NULL DEFAULT 0,
      tokens_out INTEGER NOT NULL DEFAULT 0,
      cost REAL NOT NULL DEFAULT 0,
      created_at REAL NOT NULL
    );
";

/// Indexes + the auxiliary tables. Re-run on EVERY open, exactly like the JS
/// (the hb_dedupe -> hb_dedupe2 swap is idempotent and self-healing).
const TAIL_DDL: &str = "
    DROP INDEX IF EXISTS hb_dedupe;
    CREATE UNIQUE INDEX IF NOT EXISTS hb_dedupe2
      ON heartbeats (time, machine, source, project, entity, actor);
    CREATE INDEX IF NOT EXISTS hb_time ON heartbeats (time);
    CREATE TABLE IF NOT EXISTS wakatime_days (
      date TEXT NOT NULL,
      project TEXT NOT NULL,
      seconds REAL NOT NULL,
      UNIQUE (date, project)
    );
    CREATE TABLE IF NOT EXISTS agent_status (
      machine TEXT PRIMARY KEY,
      reported_at REAL NOT NULL,
      received_at REAL NOT NULL,
      version TEXT NOT NULL,
      node_version TEXT NOT NULL,
      interval_seconds REAL NOT NULL,
      queue_depth INTEGER NOT NULL,
      queue_bytes INTEGER NOT NULL,
      clock_skew_seconds REAL NOT NULL,
      watchers_json TEXT NOT NULL
    );
";

/// The `actor` backfill, ported as VERBATIM SQL. GLOB is case-sensitive and
/// its `[^...]` negated classes have no LIKE/regex equivalent here — this
/// string must not be rewritten in Rust.
const ACTOR_MIGRATION: &str = r#"
    ALTER TABLE heartbeats ADD COLUMN actor TEXT NOT NULL DEFAULT 'human';
    UPDATE heartbeats SET actor = 'agent'
      WHERE source LIKE 'claude-%' OR source LIKE 'codex-%'
        OR source = 'zed-agent'
        OR lower(category) = 'ai'
        OR lower(category) GLOB 'ai[^a-z0-9_]*'
        OR lower(category) GLOB '*[^a-z0-9_]ai'
        OR lower(category) GLOB '*[^a-z0-9_]ai[^a-z0-9_]*';
"#;

/// mkdir -p the parent, open, busy_timeout 5000, WAL, full DDL + migrations.
pub fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let db = Connection::open(path).map_err(sql_err)?;

    // `PRAGMA journal_mode` returns a row, so it must not go through
    // execute_batch's execute() path (which can reject result-producing
    // statements depending on rusqlite's feature flags).
    db.pragma_update(None, "busy_timeout", 5000i64)
        .map_err(sql_err)?;
    db.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
        .map_err(sql_err)?;

    db.execute_batch(HEARTBEATS_DDL).map_err(sql_err)?;

    // Migrations for DBs created before newer columns existed. Every column is
    // checked independently so an upgrade interrupted between ALTER statements
    // is safely resumable on the next start.
    let cols = table_columns(&db, "heartbeats")?;
    let has = |name: &str| cols.iter().any(|c| c == name);
    db.execute_batch("BEGIN IMMEDIATE").map_err(sql_err)?;
    let migrated = (|| -> Result<()> {
        if !has("actor") {
            db.execute_batch(ACTOR_MIGRATION).map_err(sql_err)?;
        }
        if !has("tokens_in") {
            db.execute_batch(
                "ALTER TABLE heartbeats ADD COLUMN tokens_in INTEGER NOT NULL DEFAULT 0",
            )
            .map_err(sql_err)?;
        }
        if !has("tokens_out") {
            db.execute_batch(
                "ALTER TABLE heartbeats ADD COLUMN tokens_out INTEGER NOT NULL DEFAULT 0",
            )
            .map_err(sql_err)?;
        }
        if !has("cost") {
            db.execute_batch("ALTER TABLE heartbeats ADD COLUMN cost REAL NOT NULL DEFAULT 0")
                .map_err(sql_err)?;
        }
        db.execute_batch("COMMIT").map_err(sql_err)
    })();
    if let Err(err) = migrated {
        let _ = db.execute_batch("ROLLBACK"); // preserve the original error
        return Err(err);
    }

    db.execute_batch(TAIL_DDL).map_err(sql_err)?;
    Ok(db)
}

/// Column names of a table, via `PRAGMA table_info`.
fn table_columns(db: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = db
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(sql_err)?);
    }
    Ok(out)
}

/// Read-only immutable URI open (`file:…?immutable=1`) — plain read-only
/// would still create -wal/-shm sidecars next to a backup file.
pub fn open_immutable(path: &Path) -> Result<Connection> {
    let uri = format!("file:{}?immutable=1", uri_encode_path(path));
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql_err)
}

const INSERT_SQL: &str = "
    INSERT OR IGNORE INTO heartbeats
      (time, machine, source, project, entity, entity_type, category, language, branch, is_write, actor, tokens_in, tokens_out, cost, created_at)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

/// `!h || !Number.isFinite(h.time)` — the JS uses the NON-coercing
/// `Number.isFinite`, so a string time like `"1700000000"` is not a number and
/// the row is dropped. Falsy / non-object rows have no `.time` at all.
fn finite_time(row: &Value) -> Option<f64> {
    match row.get("time") {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// `nonnegativeNumber(v, { integer: true })` for an optional (possibly
/// `undefined`) property. Absent === `undefined`, which is falsy -> 0.
fn nonneg_int(v: Option<&Value>) -> i64 {
    v.map(|val| nonneg(val, true) as i64).unwrap_or(0)
}

/// `nonnegativeNumber(v)` for an optional property.
fn nonneg_float(v: Option<&Value>) -> f64 {
    v.map(|val| nonneg(val, false)).unwrap_or(0.0)
}

/// Insert a batch inside one BEGIN IMMEDIATE with a prepared
/// INSERT OR IGNORE. All JS coercions via `stackhour_core::jsnum`; rows with
/// non-finite time are skipped silently; one shared created_at for the whole
/// batch; returns the sum of changes. Any SQL error rolls back the batch.
pub fn insert_heartbeats(db: &mut Connection, rows: &[Value]) -> Result<i64> {
    let now = now_seconds();
    db.execute_batch("BEGIN IMMEDIATE").map_err(sql_err)?;
    let result = (|| -> Result<i64> {
        let mut inserted: i64 = 0;
        {
            let mut stmt = db.prepare(INSERT_SQL).map_err(sql_err)?;
            for h in rows {
                // Falsy rows and missing / non-numeric / non-finite times fall
                // out here — silently, but still counted in `received`.
                let time = match finite_time(h) {
                    Some(t) => t,
                    None => continue,
                };
                let language = h.get("language").filter(|v| js_truthy(v)).map(js_display);
                let branch = h.get("branch").filter(|v| js_truthy(v)).map(js_display);
                let changes = stmt
                    .execute(params![
                        time,
                        js_string_or(h.get("machine"), "unknown"),
                        js_string_or(h.get("source"), "unknown"),
                        js_string_or(h.get("project"), "unknown"),
                        js_string_or(h.get("entity"), "unknown"),
                        // strict `=== 'app'`; anything else is 'file'
                        if h.get("entity_type").and_then(Value::as_str) == Some("app") {
                            "app"
                        } else {
                            "file"
                        },
                        js_string_or(h.get("category"), "coding"),
                        language,
                        branch,
                        i64::from(h.get("is_write").map(js_truthy).unwrap_or(false)),
                        // strict `=== 'agent'`; anything else is 'human'
                        if h.get("actor").and_then(Value::as_str) == Some("agent") {
                            "agent"
                        } else {
                            "human"
                        },
                        nonneg_int(h.get("tokens_in")),
                        nonneg_int(h.get("tokens_out")),
                        nonneg_float(h.get("cost")),
                        now,
                    ])
                    .map_err(sql_err)?;
                inserted += changes as i64;
            }
        }
        db.execute_batch("COMMIT").map_err(sql_err)?;
        Ok(inserted)
    })();
    match result {
        Ok(n) => Ok(n),
        Err(err) => {
            let _ = db.execute_batch("ROLLBACK"); // preserve the original error
            Err(err)
        }
    }
}

const AGENT_STATUS_UPSERT: &str = "
    INSERT INTO agent_status
      (machine, reported_at, received_at, version, node_version, interval_seconds,
       queue_depth, queue_bytes, clock_skew_seconds, watchers_json)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    ON CONFLICT(machine) DO UPDATE SET
      reported_at=excluded.reported_at, received_at=excluded.received_at,
      version=excluded.version, node_version=excluded.node_version,
      interval_seconds=excluded.interval_seconds, queue_depth=excluded.queue_depth,
      queue_bytes=excluded.queue_bytes, clock_skew_seconds=excluded.clock_skew_seconds,
      watchers_json=excluded.watchers_json";

/// Upsert one agent status report. `invalid agent status` on a missing/empty
/// machine or non-finite reportedAt; machine truncated at 200 chars, version
/// fields at 100; watchers JSON kept only for plain non-array objects.
pub fn upsert_agent_status(db: &Connection, status: &Value, received_at: f64) -> Result<StatusAck> {
    // `Number(status.time)` — a COERCING conversion here, unlike insert's
    // non-coercing `Number.isFinite(h.time)`, so the string "7" is accepted.
    // An ABSENT key is `undefined` -> NaN (rejected); an explicit `null` is 0
    // (accepted) — the two must not be collapsed.
    let reported_at = match status.get("time") {
        Some(v) => js_number(v),
        None => f64::NAN,
    };
    let machine = slice_utf16(js_trim(&js_string_or(status.get("machine"), "")), 200);
    if machine.is_empty() || !reported_at.is_finite() {
        return Err(Error::msg("invalid agent status"));
    }
    // Only a plain, non-array object survives; everything else becomes `{}`.
    let watchers = match status.get("watchers") {
        Some(v @ Value::Object(_)) => v.clone(),
        _ => json!({}),
    };
    let skew = received_at - reported_at;
    db.prepare_cached(AGENT_STATUS_UPSERT)
        .map_err(sql_err)?
        .execute(params![
            machine,
            reported_at,
            received_at,
            slice_utf16(&js_string_or(status.get("version"), "unknown"), 100),
            slice_utf16(&js_string_or(status.get("nodeVersion"), "unknown"), 100),
            nonneg_float(status.get("intervalSeconds")),
            nonneg_int(status.get("queueDepth")),
            nonneg_int(status.get("queueBytes")),
            skew,
            watchers.to_string(),
        ])
        .map_err(sql_err)?;
    Ok(StatusAck {
        machine,
        server_time: received_at,
        clock_skew_seconds: skew,
    })
}

/// All agent status rows ordered by machine, mapped to camelCase objects;
/// ageSeconds clamped at 0; watchers JSON parse failure -> `{}`.
pub fn list_agent_status(db: &Connection, now: f64) -> Result<Vec<Value>> {
    let mut stmt = db
        .prepare("SELECT * FROM agent_status ORDER BY machine")
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |row| {
            let watchers_json: String = row.get("watchers_json")?;
            // A successful parse is used verbatim, so a stored "null" yields
            // `null` (not `{}`); only a parse FAILURE falls back to `{}`.
            let watchers: Value = serde_json::from_str(&watchers_json).unwrap_or_else(|_| json!({}));
            let received_at: f64 = row.get("received_at")?;
            Ok(json!({
                "machine": row.get::<_, String>("machine")?,
                "reportedAt": json_num(row.get::<_, f64>("reported_at")?),
                "receivedAt": json_num(received_at),
                "ageSeconds": json_num((now - received_at).max(0.0)),
                "version": row.get::<_, String>("version")?,
                "nodeVersion": row.get::<_, String>("node_version")?,
                "intervalSeconds": json_num(row.get::<_, f64>("interval_seconds")?),
                "queueDepth": row.get::<_, i64>("queue_depth")?,
                "queueBytes": row.get::<_, i64>("queue_bytes")?,
                "clockSkewSeconds": json_num(row.get::<_, f64>("clock_skew_seconds")?),
                "watchers": watchers,
            }))
        })
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(sql_err)?);
    }
    Ok(out)
}

/// Upsert one wakatime_days row.
pub fn upsert_wakatime_day(db: &Connection, date: &str, project: &str, seconds: f64) -> Result<()> {
    db.prepare_cached(
        "INSERT INTO wakatime_days (date, project, seconds) VALUES (?, ?, ?)
         ON CONFLICT (date, project) DO UPDATE SET seconds = excluded.seconds",
    )
    .map_err(sql_err)?
    .execute(params![date, project, seconds])
    .map_err(sql_err)?;
    Ok(())
}

/// Map one `SELECT *` heartbeats row.
fn heartbeat_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Heartbeat> {
    Ok(Heartbeat {
        id: row.get("id")?,
        time: row.get("time")?,
        machine: row.get("machine")?,
        source: row.get("source")?,
        project: row.get("project")?,
        entity: row.get("entity")?,
        entity_type: row.get("entity_type")?,
        category: row.get("category")?,
        language: row.get("language")?,
        branch: row.get("branch")?,
        is_write: row.get("is_write")?,
        actor: row.get("actor")?,
        tokens_in: row.get("tokens_in")?,
        tokens_out: row.get("tokens_out")?,
        cost: row.get("cost")?,
        created_at: row.get("created_at")?,
    })
}

fn collect_heartbeats(
    db: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Heartbeat>> {
    let mut stmt = db.prepare(sql).map_err(sql_err)?;
    let rows = stmt.query_map(params, heartbeat_from_row).map_err(sql_err)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(sql_err)?);
    }
    Ok(out)
}

/// `SELECT * FROM heartbeats WHERE time >= ? AND time <= ?`.
///
/// The JS leaves the order to SQLite, which satisfies this predicate from the
/// `hb_time` index and therefore yields (time, rowid) ascending; the ORDER BY
/// is spelled out so the Rust port stays deterministic regardless of the
/// planner. Callers (reattribution, summarize) sort per stream anyway.
pub fn rows_in_range(db: &Connection, from: f64, to: f64) -> Result<Vec<Heartbeat>> {
    collect_heartbeats(
        db,
        "SELECT * FROM heartbeats WHERE time >= ? AND time <= ? ORDER BY time, id",
        &[&from, &to],
    )
}

/// Newest-N page (`ORDER BY time DESC, id DESC LIMIT ?`).
pub fn recent_page(db: &Connection, limit: i64) -> Result<Vec<Heartbeat>> {
    collect_heartbeats(
        db,
        "SELECT * FROM heartbeats ORDER BY time DESC, id DESC LIMIT ?",
        &[&limit],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_db() -> (TempDir, Connection) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("nested").join("stackhour.db");
        let db = open_db(&path).expect("open_db");
        (dir, db)
    }

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn js_trim_matches_ecmascript_whitespace() {
        assert_eq!(js_trim("  box  "), "box");
        assert_eq!(js_trim("\t\n\r\u{000B}\u{000C}x\u{00A0}\u{2028}"), "x");
        // U+FEFF is trimmed by JS, U+0085 is not.
        assert_eq!(js_trim("\u{FEFF}x\u{FEFF}"), "x");
        assert_eq!(js_trim("\u{0085}x\u{0085}"), "\u{0085}x\u{0085}");
        assert_eq!(js_trim("   "), "");
    }

    #[test]
    fn slice_utf16_counts_code_units() {
        assert_eq!(slice_utf16("abcdef", 3), "abc");
        assert_eq!(slice_utf16("abc", 100), "abc");
        assert_eq!(slice_utf16("", 5), "");
        // 'é' is one UTF-16 unit but two UTF-8 bytes.
        assert_eq!(slice_utf16("ééé", 2), "éé");
        // An astral char is two units: it fits at 2, not at 1.
        assert_eq!(slice_utf16("\u{1F600}b", 2), "\u{1F600}");
        assert_eq!(slice_utf16("\u{1F600}b", 1), "");
        assert_eq!(slice_utf16("a\u{1F600}", 2), "a");
    }

    #[test]
    fn uri_encode_path_escapes_specials() {
        assert_eq!(uri_encode_path(Path::new("/tmp/a.db")), "/tmp/a.db");
        assert_eq!(
            uri_encode_path(Path::new("/tmp/my db?x#y.db")),
            "/tmp/my%20db%3Fx%23y.db"
        );
    }

    #[test]
    fn finite_time_rejects_non_numeric() {
        assert_eq!(finite_time(&json!({"time": 12.5})), Some(12.5));
        assert_eq!(finite_time(&json!({"time": 12})), Some(12.0));
        // Number.isFinite does NOT coerce.
        assert_eq!(finite_time(&json!({"time": "12"})), None);
        assert_eq!(finite_time(&json!({"time": null})), None);
        assert_eq!(finite_time(&json!({})), None);
        assert_eq!(finite_time(&json!(null)), None);
        assert_eq!(finite_time(&json!("nope")), None);
        assert_eq!(finite_time(&json!([1, 2])), None);
    }

    // ---- open_db / migrations --------------------------------------------

    #[test]
    fn open_db_creates_parent_dirs_and_schema() {
        let (_dir, db) = temp_db();
        let cols = table_columns(&db, "heartbeats").expect("cols");
        for want in [
            "id",
            "time",
            "machine",
            "source",
            "project",
            "entity",
            "entity_type",
            "category",
            "language",
            "branch",
            "is_write",
            "actor",
            "tokens_in",
            "tokens_out",
            "cost",
            "created_at",
        ] {
            assert!(cols.iter().any(|c| c == want), "missing column {want}");
        }
        let mode: String = db
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("journal_mode");
        assert_eq!(mode, "wal");
        let timeout: i64 = db
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .expect("busy_timeout");
        assert_eq!(timeout, 5000);

        let idx = index_names(&db);
        assert!(idx.iter().any(|n| n == "hb_dedupe2"));
        assert!(idx.iter().any(|n| n == "hb_time"));
        assert!(!idx.iter().any(|n| n == "hb_dedupe"));
    }

    fn index_names(db: &Connection) -> Vec<String> {
        let mut stmt = db
            .prepare("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name")
            .expect("prep");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect");
        rows
    }

    #[test]
    fn open_db_is_idempotent() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("s.db");
        {
            let mut db = open_db(&path).expect("first open");
            insert_heartbeats(&mut db, &[json!({"time": 1.0, "machine": "m"})]).expect("insert");
        }
        let db = open_db(&path).expect("second open");
        let n: i64 = db
            .query_row("SELECT count(*) FROM heartbeats", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 1);
    }

    #[test]
    fn migration_adds_columns_and_backfills_actor() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("old.db");
        {
            // A pre-actor / pre-tokens schema, plus the legacy hb_dedupe index.
            let old = Connection::open(&path).expect("open old");
            old.execute_batch(
                "CREATE TABLE heartbeats (
                   id INTEGER PRIMARY KEY, time REAL NOT NULL, machine TEXT NOT NULL,
                   source TEXT NOT NULL, project TEXT NOT NULL, entity TEXT NOT NULL,
                   entity_type TEXT NOT NULL DEFAULT 'file',
                   category TEXT NOT NULL DEFAULT 'coding',
                   language TEXT, branch TEXT,
                   is_write INTEGER NOT NULL DEFAULT 0, created_at REAL NOT NULL);
                 CREATE UNIQUE INDEX hb_dedupe ON heartbeats (time, machine, source, project, entity);
                 INSERT INTO heartbeats (time, machine, source, project, entity, category, created_at)
                 VALUES
                   (1, 'm', 'claude-code', 'p', 'e1', 'coding', 0),
                   (2, 'm', 'codex-cli', 'p', 'e2', 'coding', 0),
                   (3, 'm', 'zed-agent', 'p', 'e3', 'coding', 0),
                   (4, 'm', 'webstorm', 'p', 'e4', 'AI', 0),
                   (5, 'm', 'webstorm', 'p', 'e5', 'ai coding', 0),
                   (6, 'm', 'webstorm', 'p', 'e6', 'coding ai', 0),
                   (7, 'm', 'webstorm', 'p', 'e7', 'x ai y', 0),
                   (8, 'm', 'webstorm', 'p', 'e8', 'coding', 0),
                   (9, 'm', 'webstorm', 'p', 'e9', 'aim', 0),
                   (10, 'm', 'webstorm', 'p', 'e10', 'said', 0);",
            )
            .expect("old schema");
        }
        let db = open_db(&path).expect("migrating open");
        let cols = table_columns(&db, "heartbeats").expect("cols");
        for want in ["actor", "tokens_in", "tokens_out", "cost"] {
            assert!(cols.iter().any(|c| c == want), "missing {want}");
        }
        let agents: Vec<i64> = {
            let mut stmt = db
                .prepare("SELECT time FROM heartbeats WHERE actor = 'agent' ORDER BY time")
                .expect("prep");
            stmt.query_map([], |r| r.get::<_, f64>(0).map(|f| f as i64))
                .expect("q")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("collect")
        };
        // 1-3 match by source, 4-7 by category; 8 ('coding'), 9 ('aim') and
        // 10 ('said') must NOT — the GLOB classes require a non-word boundary.
        assert_eq!(agents, vec![1, 2, 3, 4, 5, 6, 7]);
        // Legacy index swapped for the 6-column one.
        assert!(!index_names(&db).iter().any(|n| n == "hb_dedupe"));
        assert!(index_names(&db).iter().any(|n| n == "hb_dedupe2"));
    }

    #[test]
    fn migration_resumes_when_only_some_columns_exist() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("partial.db");
        {
            let old = Connection::open(&path).expect("open old");
            old.execute_batch(
                "CREATE TABLE heartbeats (
                   id INTEGER PRIMARY KEY, time REAL NOT NULL, machine TEXT NOT NULL,
                   source TEXT NOT NULL, project TEXT NOT NULL, entity TEXT NOT NULL,
                   entity_type TEXT NOT NULL DEFAULT 'file',
                   category TEXT NOT NULL DEFAULT 'coding',
                   language TEXT, branch TEXT,
                   is_write INTEGER NOT NULL DEFAULT 0,
                   actor TEXT NOT NULL DEFAULT 'human',
                   tokens_in INTEGER NOT NULL DEFAULT 0,
                   created_at REAL NOT NULL);",
            )
            .expect("partial schema");
        }
        let db = open_db(&path).expect("resuming open");
        let cols = table_columns(&db, "heartbeats").expect("cols");
        assert!(cols.iter().any(|c| c == "tokens_out"));
        assert!(cols.iter().any(|c| c == "cost"));
        // tokens_in existed already and must not have been added twice.
        assert_eq!(cols.iter().filter(|c| *c == "tokens_in").count(), 1);
    }

    #[test]
    fn open_immutable_reads_without_writing() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("s.db");
        {
            let mut db = open_db(&path).expect("open");
            insert_heartbeats(&mut db, &[json!({"time": 5.0})]).expect("insert");
            let _ = db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            let _ = db.pragma_update(None, "journal_mode", "DELETE");
        }
        let ro = open_immutable(&path).expect("open_immutable");
        let n: i64 = ro
            .query_row("SELECT count(*) FROM heartbeats", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 1);
        assert!(ro.execute_batch("CREATE TABLE x (a)").is_err());
    }

    // ---- insert_heartbeats ------------------------------------------------

    #[test]
    fn insert_applies_js_coercions() {
        let (_dir, mut db) = temp_db();
        let n = insert_heartbeats(
            &mut db,
            &[json!({
                "time": 100.5,
                "machine": "", "source": 0, "project": null, "entity": false,
                "entity_type": "App", "category": "",
                "language": "", "branch": "main",
                "is_write": "yes", "actor": "Agent",
                "tokens_in": "12.6", "tokens_out": -4, "cost": "0.5",
            })],
        )
        .expect("insert");
        assert_eq!(n, 1);
        let row = rows_in_range(&db, 0.0, 1000.0).expect("range").remove(0);
        assert_eq!(row.time, 100.5);
        assert_eq!(row.machine, "unknown");
        assert_eq!(row.source, "unknown");
        assert_eq!(row.project, "unknown");
        assert_eq!(row.entity, "unknown");
        assert_eq!(row.entity_type, "file"); // 'App' !== 'app'
        assert_eq!(row.category, "coding");
        assert_eq!(row.language, None);
        assert_eq!(row.branch.as_deref(), Some("main"));
        assert_eq!(row.is_write, 1);
        assert_eq!(row.actor, "human"); // 'Agent' !== 'agent'
        assert_eq!(row.tokens_in, 13); // Math.round(Number("12.6"))
        assert_eq!(row.tokens_out, 0); // negative clamps to 0
        assert_eq!(row.cost, 0.5);
        assert!(row.created_at > 0.0);
    }

    #[test]
    fn insert_keeps_explicit_app_and_agent() {
        let (_dir, mut db) = temp_db();
        insert_heartbeats(
            &mut db,
            &[json!({"time": 1.0, "entity_type": "app", "actor": "agent"})],
        )
        .expect("insert");
        let row = rows_in_range(&db, 0.0, 10.0).expect("range").remove(0);
        assert_eq!(row.entity_type, "app");
        assert_eq!(row.actor, "agent");
    }

    #[test]
    fn insert_skips_bad_time_rows_silently() {
        let (_dir, mut db) = temp_db();
        let n = insert_heartbeats(
            &mut db,
            &[
                json!(null),
                json!(false),
                json!("nope"),
                json!({}),
                json!({"time": null}),
                json!({"time": "1700000000"}),
                json!({"time": 42.0}),
            ],
        )
        .expect("insert");
        assert_eq!(n, 1);
    }

    #[test]
    fn insert_dedupes_on_the_six_column_index() {
        let (_dir, mut db) = temp_db();
        let row = json!({
            "time": 1.0, "machine": "m", "source": "s", "project": "p",
            "entity": "e", "actor": "human"
        });
        // The same tuple twice in one batch, then again in a second batch.
        assert_eq!(
            insert_heartbeats(&mut db, &[row.clone(), row.clone()]).expect("i1"),
            1
        );
        assert_eq!(
            insert_heartbeats(&mut db, std::slice::from_ref(&row)).expect("i2"),
            0
        );
        // actor differs -> a distinct row (hb_dedupe2 includes actor).
        let mut agent = row.clone();
        agent["actor"] = json!("agent");
        assert_eq!(insert_heartbeats(&mut db, &[agent]).expect("i3"), 1);
    }

    #[test]
    fn insert_shares_one_created_at_for_the_batch() {
        let (_dir, mut db) = temp_db();
        insert_heartbeats(
            &mut db,
            &[
                json!({"time": 1.0, "entity": "a"}),
                json!({"time": 2.0, "entity": "b"}),
            ],
        )
        .expect("insert");
        let rows = rows_in_range(&db, 0.0, 10.0).expect("range");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].created_at, rows[1].created_at);
    }

    #[test]
    fn insert_rolls_back_the_whole_batch_on_sql_error() {
        let (_dir, mut db) = temp_db();
        db.execute_batch(
            "CREATE TRIGGER boom BEFORE INSERT ON heartbeats
             WHEN NEW.entity = 'bad' BEGIN SELECT RAISE(ABORT, 'nope'); END;",
        )
        .expect("trigger");
        let err = insert_heartbeats(
            &mut db,
            &[
                json!({"time": 1.0, "entity": "ok"}),
                json!({"time": 2.0, "entity": "bad"}),
            ],
        )
        .expect_err("should fail");
        assert!(err.message().contains("nope"), "got {}", err.message());
        let n: i64 = db
            .query_row("SELECT count(*) FROM heartbeats", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 0, "the good row must be rolled back too");
        // The connection is usable again (no dangling transaction).
        assert_eq!(
            insert_heartbeats(&mut db, &[json!({"time": 3.0, "entity": "ok"})]).expect("after"),
            1
        );
    }

    #[test]
    fn insert_empty_batch_is_a_noop() {
        let (_dir, mut db) = temp_db();
        assert_eq!(insert_heartbeats(&mut db, &[]).expect("insert"), 0);
    }

    // ---- agent status -----------------------------------------------------

    #[test]
    fn agent_status_roundtrip() {
        let (_dir, db) = temp_db();
        let ack = upsert_agent_status(
            &db,
            &json!({
                "time": 1000.0, "machine": "  box  ", "version": "0.1.0",
                "nodeVersion": "v22.0.0", "intervalSeconds": 20,
                "queueDepth": "3.4", "queueBytes": -10,
                "watchers": {"editor": {"enabled": true}}
            }),
            1005.0,
        )
        .expect("upsert");
        assert_eq!(ack.machine, "box");
        assert_eq!(ack.server_time, 1005.0);
        assert_eq!(ack.clock_skew_seconds, 5.0);

        let list = list_agent_status(&db, 1010.0).expect("list");
        assert_eq!(list.len(), 1);
        let r = &list[0];
        assert_eq!(r["machine"], json!("box"));
        assert_eq!(r["reportedAt"], json!(1000));
        assert_eq!(r["receivedAt"], json!(1005));
        assert_eq!(r["ageSeconds"], json!(5));
        assert_eq!(r["version"], json!("0.1.0"));
        assert_eq!(r["nodeVersion"], json!("v22.0.0"));
        assert_eq!(r["intervalSeconds"], json!(20));
        assert_eq!(r["queueDepth"], json!(3)); // Math.round(Number("3.4"))
        assert_eq!(r["queueBytes"], json!(0)); // negative clamps
        assert_eq!(r["clockSkewSeconds"], json!(5));
        assert_eq!(r["watchers"], json!({"editor": {"enabled": true}}));
        // camelCase key order is part of the response shape.
        let keys: Vec<&str> = r
            .as_object()
            .expect("obj")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "machine",
                "reportedAt",
                "receivedAt",
                "ageSeconds",
                "version",
                "nodeVersion",
                "intervalSeconds",
                "queueDepth",
                "queueBytes",
                "clockSkewSeconds",
                "watchers"
            ]
        );
    }

    #[test]
    fn agent_status_upsert_replaces_by_machine() {
        let (_dir, db) = temp_db();
        upsert_agent_status(&db, &json!({"time": 1.0, "machine": "box"}), 2.0).expect("first");
        upsert_agent_status(
            &db,
            &json!({"time": 50.0, "machine": "box", "version": "9.9.9"}),
            60.0,
        )
        .expect("second");
        let list = list_agent_status(&db, 60.0).expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["version"], json!("9.9.9"));
        assert_eq!(list[0]["reportedAt"], json!(50));
        assert_eq!(list[0]["clockSkewSeconds"], json!(10));
    }

    #[test]
    fn agent_status_defaults_and_validation() {
        let (_dir, db) = temp_db();
        for bad in [
            json!({"time": 1.0}),
            json!({"time": 1.0, "machine": "   "}),
            json!({"machine": "box"}),
            json!({"time": "abc", "machine": "box"}),
            json!({"time": {}, "machine": "box"}),
        ] {
            let err = upsert_agent_status(&db, &bad, 1.0).expect_err("must reject");
            assert_eq!(err.message(), "invalid agent status");
        }
        // An explicit null is Number(null) === 0 and IS accepted, unlike an
        // absent key (undefined -> NaN), which is rejected above.
        upsert_agent_status(&db, &json!({"time": null, "machine": "nul"}), 1.0)
            .expect("null time is 0");
        // Number("7") coerces here (unlike heartbeat time).
        upsert_agent_status(&db, &json!({"time": "7", "machine": "box"}), 9.0).expect("string time");
        let r = list_agent_status(&db, 9.0).expect("list").remove(0);
        assert_eq!(r["reportedAt"], json!(7));
        assert_eq!(r["version"], json!("unknown"));
        assert_eq!(r["nodeVersion"], json!("unknown"));
        assert_eq!(r["intervalSeconds"], json!(0));
        assert_eq!(r["queueDepth"], json!(0));
        assert_eq!(r["queueBytes"], json!(0));
        assert_eq!(r["watchers"], json!({}));
    }

    #[test]
    fn agent_status_watchers_only_for_plain_objects() {
        let (_dir, db) = temp_db();
        for watchers in [json!([1, 2]), json!("x"), json!(null), json!(7)] {
            upsert_agent_status(
                &db,
                &json!({"time": 1.0, "machine": "box", "watchers": watchers}),
                1.0,
            )
            .expect("upsert");
            let r = list_agent_status(&db, 1.0).expect("list").remove(0);
            assert_eq!(r["watchers"], json!({}));
        }
    }

    #[test]
    fn agent_status_truncates_long_strings() {
        let (_dir, db) = temp_db();
        let long_machine = "m".repeat(500);
        let long_version = "v".repeat(400);
        let ack = upsert_agent_status(
            &db,
            &json!({
                "time": 1.0, "machine": long_machine,
                "version": long_version.clone(), "nodeVersion": long_version
            }),
            1.0,
        )
        .expect("upsert");
        assert_eq!(ack.machine.len(), 200);
        let r = list_agent_status(&db, 1.0).expect("list").remove(0);
        assert_eq!(r["version"].as_str().expect("str").len(), 100);
        assert_eq!(r["nodeVersion"].as_str().expect("str").len(), 100);
    }

    #[test]
    fn agent_status_age_clamps_and_skew_does_not() {
        let (_dir, db) = temp_db();
        // Agent clock ahead of the server: skew is negative and stays negative.
        let ack =
            upsert_agent_status(&db, &json!({"time": 500.0, "machine": "box"}), 100.0).expect("up");
        assert_eq!(ack.clock_skew_seconds, -400.0);
        // `now` before received_at: ageSeconds clamps at 0.
        let r = list_agent_status(&db, 50.0).expect("list").remove(0);
        assert_eq!(r["ageSeconds"], json!(0));
        assert_eq!(r["clockSkewSeconds"], json!(-400));
    }

    #[test]
    fn agent_status_listing_is_ordered_by_machine() {
        let (_dir, db) = temp_db();
        for m in ["zeta", "alpha", "mid"] {
            upsert_agent_status(&db, &json!({"time": 1.0, "machine": m}), 1.0).expect("up");
        }
        let names: Vec<String> = list_agent_status(&db, 1.0)
            .expect("list")
            .iter()
            .map(|r| r["machine"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn agent_status_corrupt_watchers_json_becomes_empty_object() {
        let (_dir, db) = temp_db();
        upsert_agent_status(&db, &json!({"time": 1.0, "machine": "box"}), 1.0).expect("up");
        db.execute("UPDATE agent_status SET watchers_json = '{oops'", [])
            .expect("corrupt");
        let r = list_agent_status(&db, 1.0).expect("list").remove(0);
        assert_eq!(r["watchers"], json!({}));
    }

    #[test]
    fn agent_status_empty_table_lists_nothing() {
        let (_dir, db) = temp_db();
        assert!(list_agent_status(&db, 1.0).expect("list").is_empty());
    }

    // ---- wakatime days ----------------------------------------------------

    #[test]
    fn wakatime_day_upsert_replaces_seconds() {
        let (_dir, db) = temp_db();
        upsert_wakatime_day(&db, "2026-07-19", "stackhour", 100.0).expect("first");
        upsert_wakatime_day(&db, "2026-07-19", "stackhour", 250.5).expect("second");
        upsert_wakatime_day(&db, "2026-07-19", "other", 10.0).expect("other project");
        let total: f64 = db
            .query_row("SELECT sum(seconds) FROM wakatime_days", [], |r| r.get(0))
            .expect("sum");
        assert_eq!(total, 260.5);
        let n: i64 = db
            .query_row("SELECT count(*) FROM wakatime_days", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 2);
    }

    // ---- queries ----------------------------------------------------------

    #[test]
    fn rows_in_range_is_inclusive_and_time_ordered() {
        let (_dir, mut db) = temp_db();
        let rows: Vec<Value> = [30.0, 10.0, 20.0, 40.0]
            .iter()
            .map(|t| json!({"time": t, "entity": format!("e{t}")}))
            .collect();
        insert_heartbeats(&mut db, &rows).expect("insert");
        let times: Vec<f64> = rows_in_range(&db, 10.0, 30.0)
            .expect("range")
            .iter()
            .map(|r| r.time)
            .collect();
        assert_eq!(times, vec![10.0, 20.0, 30.0]);
        assert!(rows_in_range(&db, 100.0, 200.0).expect("empty").is_empty());
    }

    #[test]
    fn recent_page_is_newest_first_and_limited() {
        let (_dir, mut db) = temp_db();
        let rows: Vec<Value> = (1..=5)
            .map(|t| json!({"time": t as f64, "entity": format!("e{t}")}))
            .collect();
        insert_heartbeats(&mut db, &rows).expect("insert");
        let times: Vec<f64> = recent_page(&db, 3)
            .expect("recent")
            .iter()
            .map(|r| r.time)
            .collect();
        assert_eq!(times, vec![5.0, 4.0, 3.0]);
        assert_eq!(recent_page(&db, 0).expect("zero").len(), 0);
        assert_eq!(recent_page(&db, 100).expect("all").len(), 5);
    }

    #[test]
    fn heartbeat_rows_map_all_columns() {
        let (_dir, mut db) = temp_db();
        insert_heartbeats(
            &mut db,
            &[json!({
                "time": 7.5, "machine": "box", "source": "claude-code", "project": "stackhour",
                "entity": "/tmp/a.rs", "entity_type": "app", "category": "ai",
                "language": "rust", "branch": "main", "is_write": 1, "actor": "agent",
                "tokens_in": 10, "tokens_out": 20, "cost": 1.25
            })],
        )
        .expect("insert");
        let r = recent_page(&db, 1).expect("recent").remove(0);
        assert!(r.id > 0);
        assert_eq!(r.time, 7.5);
        assert_eq!(r.machine, "box");
        assert_eq!(r.source, "claude-code");
        assert_eq!(r.project, "stackhour");
        assert_eq!(r.entity, "/tmp/a.rs");
        assert_eq!(r.entity_type, "app");
        assert_eq!(r.category, "ai");
        assert_eq!(r.language.as_deref(), Some("rust"));
        assert_eq!(r.branch.as_deref(), Some("main"));
        assert_eq!(r.is_write, 1);
        assert_eq!(r.actor, "agent");
        assert_eq!(r.tokens_in, 10);
        assert_eq!(r.tokens_out, 20);
        assert_eq!(r.cost, 1.25);
    }
}
