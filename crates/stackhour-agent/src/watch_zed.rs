//! `zed` watcher: Zed threads.db differ (this module uses rusqlite directly —
//! the only sqlite use outside stackhour-store).
//!
//! First-existing ZED_DB_PATHS candidate; bigint-stat change signature over
//! db and db+'-wal' (NEVER -shm); early return on an unchanged signature;
//! rusqlite backup-API snapshot to zed-threads-copy.db via a pid tmp
//! (fallback: open the live db read-only); schema discovery (prefer table
//! 'threads', require id+updated_at, optional summary, QUOTED identifiers);
//! thread diff vs state zedThreads with zedInitDone first-run suppression
//! (init without emission; reappeared ids DO emit); summary-first-120-chars
//! entities; legacy zedDbMtime key cleanup; chmod 0600 on copies in a
//! finally-equivalent.
//!
//! Port of `src/agent/watch-zed.js`.

use crate::{Gate, Watcher};
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use stackhour_core::config::Config;
use stackhour_core::{Error, Result};
use std::path::{Path, PathBuf};

/// Where Zed persists agent-panel threads, macOS first then Linux.
pub fn zed_db_paths() -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    vec![
        home.join("Library")
            .join("Application Support")
            .join("Zed")
            .join("threads")
            .join("threads.db"),
        home.join(".local")
            .join("share")
            .join("zed")
            .join("threads")
            .join("threads.db"),
    ]
}

/// The Zed agent-threads watcher.
#[derive(Debug, Default)]
pub struct ZedWatcher {
    /// Overrides the candidate db paths (tests).
    pub candidate_paths: Option<Vec<PathBuf>>,
    /// Overrides the snapshot destination directory (tests).
    pub data_dir: Option<PathBuf>,
}

/// A cheap "did anything commit?" fingerprint.
///
/// The WAL carries committed changes before checkpointing, so it must be part
/// of the signature. `-shm` deliberately is NOT: a read-only connection can
/// mutate its lock metadata without any data changing, which would make every
/// tick look dirty and re-snapshot the whole database.
fn db_signature(src: &Path) -> String {
    ["", "-wal"]
        .iter()
        .map(|suffix| {
            let path = PathBuf::from(format!("{}{suffix}", src.display()));
            match std::fs::metadata(&path) {
                Ok(md) => {
                    use std::os::unix::fs::MetadataExt;
                    format!(
                        "{suffix}:{}:{}{:09}:{}{:09}",
                        md.len(),
                        md.mtime(),
                        md.mtime_nsec(),
                        md.ctime(),
                        md.ctime_nsec()
                    )
                }
                Err(_) => format!("{suffix}:missing"),
            }
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn rm_sidecars(base: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", base.display()));
    }
}

fn chmod_600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// Take a transactionally consistent copy of `src` so we never read a
/// half-committed page set out from under a live Zed.
fn snapshot_db(src: &Path, data_dir: &Path) -> Result<PathBuf> {
    let dst = data_dir.join("zed-threads-copy.db");
    let tmp = data_dir.join(format!("zed-threads-copy.db.{}.tmp", std::process::id()));
    std::fs::create_dir_all(data_dir)
        .map_err(|e| Error::msg(format!("cannot create {}: {e}", data_dir.display())))?;
    rm_sidecars(&tmp);

    let result = (|| -> rusqlite::Result<()> {
        let source = Connection::open_with_flags(
            src,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        let mut dest = Connection::open(&tmp)?;
        let backup = rusqlite::backup::Backup::new(&source, &mut dest)?;
        backup.run_to_completion(1024, std::time::Duration::from_millis(0), None)?;
        Ok(())
    })();
    if let Err(err) = result {
        rm_sidecars(&tmp);
        return Err(Error::msg(format!("cannot open threads.db: {err}")));
    }

    chmod_600(&tmp);
    rm_sidecars(&dst);
    std::fs::rename(&tmp, &dst).map_err(|e| {
        rm_sidecars(&tmp);
        Error::msg(format!("cannot open threads.db: {e}"))
    })?;
    Ok(dst)
}

/// `"ident"` with embedded quotes doubled — the schema is undocumented and
/// discovered at runtime, so identifiers are never interpolated bare.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Watcher for ZedWatcher {
    fn name(&self) -> &'static str {
        "zed"
    }

    fn gate(&self, cfg: &Config) -> Gate {
        if cfg.agent.watch.zed {
            Gate::Run
        } else {
            Gate::Skipped {
                enabled: false,
                available: false,
                reason: "disabled in config".to_string(),
            }
        }
    }

    fn input_marker(&self, state: &Value) -> Option<String> {
        // JS: state.zedDbSignature || state.zedDbMtime || ''.
        Some(
            state
                .get("zedDbSignature")
                .and_then(Value::as_str)
                .or_else(|| state.get("zedDbMtime").and_then(Value::as_str))
                .unwrap_or("")
                .to_string(),
        )
    }

    fn run(&mut self, cfg: &Config, state: &mut Value, now: f64) -> Result<Vec<Value>> {
        let candidates = self.candidate_paths.clone().unwrap_or_else(zed_db_paths);
        let Some(db_path) = candidates.into_iter().find(|p| p.exists()) else {
            return Ok(Vec::new());
        };

        let signature = db_signature(&db_path);
        if state.get("zedDbSignature").and_then(Value::as_str) == Some(signature.as_str()) {
            return Ok(Vec::new());
        }

        let data_dir = self
            .data_dir
            .clone()
            .unwrap_or_else(|| cfg.paths.data_dir.clone());
        // A consistent copy when we can take one; a read-only view of the
        // live file when we cannot, so the watcher degrades rather than dies.
        let (db, copied) = match snapshot_db(&db_path, &data_dir) {
            Ok(copy) => (
                Connection::open_with_flags(&copy, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|e| Error::msg(format!("cannot open threads.db: {e}")))?,
                Some(copy),
            ),
            Err(_) => (
                Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|e| Error::msg(format!("cannot open threads.db: {e}")))?,
                None,
            ),
        };

        let outcome = read_threads(&db, state, now, &signature);

        drop(db);
        if let Some(copy) = copied {
            for suffix in ["", "-wal", "-shm"] {
                let p = PathBuf::from(format!("{}{suffix}", copy.display()));
                if p.exists() {
                    chmod_600(&p);
                }
            }
        }
        outcome
    }
}

/// The schema-discovery + diff half, split out so the connection cleanup in
/// `run` is a single `finally`-equivalent path.
fn read_threads(db: &Connection, state: &mut Value, now: f64, signature: &str) -> Result<Vec<Value>> {
    let fail = |e: rusqlite::Error| Error::msg(format!("cannot read threads.db: {e}"));

    let tables: Vec<String> = {
        let mut stmt = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .map_err(fail)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(fail)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(fail)?;
        rows
    };
    // Prefer the table Zed actually uses; fall back to scanning the rest so a
    // rename does not blind the watcher.
    let mut ordered: Vec<String> = tables.iter().filter(|t| *t == "threads").cloned().collect();
    ordered.extend(tables.iter().filter(|t| *t != "threads").cloned());

    let mut table = None;
    let mut cols: Vec<String> = Vec::new();
    for candidate in &ordered {
        let mut stmt = match db.prepare(&format!("PRAGMA table_info({})", quote_ident(candidate))) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let names: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(1)) {
            Ok(rows) => rows.filter_map(std::result::Result::ok).collect(),
            Err(_) => continue,
        };
        if names.iter().any(|c| c == "id") && names.iter().any(|c| c == "updated_at") {
            table = Some(candidate.clone());
            cols = names;
            break;
        }
    }
    let Some(table) = table else {
        // Nothing recognisable: remember the signature so we do not re-open
        // the database every tick, and report healthy-with-no-rows.
        record_signature(state, signature);
        return Ok(Vec::new());
    };

    let summary_col = cols.iter().find(|c| *c == "summary").cloned();
    let fields = std::iter::once("id".to_string())
        .chain(std::iter::once("updated_at".to_string()))
        .chain(summary_col.clone())
        .map(|c| quote_ident(&c))
        .collect::<Vec<_>>()
        .join(", ");

    let previous: Map<String, Value> = state
        .get("zedThreads")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let init_done = state
        .get("zedInitDone")
        .is_some_and(stackhour_core::jsnum::js_truthy);
    let mut next = Map::new();
    let mut rows = Vec::new();

    let mut stmt = db
        .prepare(&format!("SELECT {fields} FROM {}", quote_ident(&table)))
        .map_err(fail)?;
    let mut query = stmt.query([]).map_err(fail)?;
    while let Some(row) = query.next().map_err(fail)? {
        let key = row.get_ref(0).ok().map(sql_to_string).unwrap_or_default();
        let updated = row.get_ref(1).ok().map(sql_to_string).unwrap_or_default();
        if previous.get(&key).and_then(Value::as_str) == Some(updated.as_str()) {
            next.insert(key, json!(updated));
            continue;
        }
        let first_sight = !previous.contains_key(&key);
        next.insert(key.clone(), json!(updated));
        // On the very first run every thread is "new"; emitting them would
        // backfill months of history as if it happened this second.
        if first_sight && !init_done {
            continue;
        }
        let summary = summary_col
            .as_ref()
            .and_then(|_| row.get_ref(2).ok().map(sql_to_string))
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(120).collect::<String>())
            .unwrap_or_else(|| format!("thread {key}"));
        rows.push(json!({
            "time": now,
            "source": "zed-agent",
            // Thread rows carry no project path of their own.
            "project": "zed-agent",
            "entity": summary,
            "entity_type": "app",
            "category": "ai coding",
            "actor": "agent",
            "is_write": 0,
        }));
    }

    if let Some(obj) = state.as_object_mut() {
        obj.insert("zedThreads".into(), Value::Object(next));
        obj.insert("zedInitDone".into(), json!(true));
    }
    record_signature(state, signature);
    Ok(rows)
}

/// Store the fresh signature and drop the superseded `zedDbMtime` key that
/// older agent-state.json files carry.
fn record_signature(state: &mut Value, signature: &str) {
    if let Some(obj) = state.as_object_mut() {
        obj.insert("zedDbSignature".into(), json!(signature));
        obj.shift_remove("zedDbMtime");
    }
}

/// `String(value)` for whatever the undocumented schema stores in a column.
fn sql_to_string(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => String::new(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => stackhour_core::jsnum::js_display(&json!(f)),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_db(path: &Path, rows: &[(&str, &str, &str)]) {
        let db = Connection::open(path).unwrap();
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS threads (id TEXT PRIMARY KEY, updated_at TEXT, summary TEXT)",
        )
        .unwrap();
        for (id, updated, summary) in rows {
            db.execute(
                "INSERT INTO threads (id, updated_at, summary) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET updated_at=?2, summary=?3",
                [id, updated, summary],
            )
            .unwrap();
        }
    }

    struct Env {
        _tmp: TempDir,
        db: PathBuf,
        watcher: ZedWatcher,
        cfg: Config,
    }

    fn env() -> Env {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("threads.db");
        let data = tmp.path().join("data");
        Env {
            watcher: ZedWatcher {
                candidate_paths: Some(vec![db.clone()]),
                data_dir: Some(data),
            },
            cfg: crate::test_config(json!({})),
            db,
            _tmp: tmp,
        }
    }

    /// The first run must LEARN the existing threads without emitting them —
    /// otherwise enrolling a machine backfills every historical thread as
    /// activity happening right now.
    #[test]
    fn the_first_run_initialises_without_emitting_history() {
        let mut e = env();
        make_db(&e.db, &[("t1", "100", "old thread"), ("t2", "100", "")]);
        let mut state = json!({});
        let rows = e.watcher.run(&e.cfg, &mut state, 500.0).unwrap();
        assert!(rows.is_empty(), "history was flooded: {rows:#?}");
        assert_eq!(state["zedInitDone"], true);
        assert_eq!(state["zedThreads"]["t1"], "100");
        assert!(state["zedDbSignature"].is_string());
    }

    /// After init, an updated thread emits one row and a brand-new thread
    /// emits one too; untouched threads stay silent.
    #[test]
    fn updates_and_new_threads_emit_but_untouched_ones_do_not() {
        let mut e = env();
        make_db(&e.db, &[("t1", "100", "first"), ("t2", "100", "second")]);
        let mut state = json!({});
        e.watcher.run(&e.cfg, &mut state, 500.0).unwrap();

        make_db(
            &e.db,
            &[("t1", "200", "first thread summary"), ("t3", "1", "brand new")],
        );
        let rows = e.watcher.run(&e.cfg, &mut state, 600.0).unwrap();
        let mut entities: Vec<&str> = rows.iter().map(|r| r["entity"].as_str().unwrap()).collect();
        entities.sort_unstable();
        assert_eq!(entities, ["brand new", "first thread summary"]);
        assert_eq!(rows[0]["source"], "zed-agent");
        assert_eq!(rows[0]["actor"], "agent");
        assert_eq!(rows[0]["project"], "zed-agent");
        assert_eq!(rows[0]["time"], 600.0);
    }

    /// An unchanged signature short-circuits before the database is even
    /// opened, and must never re-emit the same thread twice.
    #[test]
    fn an_unchanged_signature_short_circuits_and_avoids_duplicates() {
        let mut e = env();
        make_db(&e.db, &[("t1", "100", "a")]);
        let mut state = json!({});
        e.watcher.run(&e.cfg, &mut state, 500.0).unwrap();
        make_db(&e.db, &[("t1", "200", "changed")]);
        assert_eq!(e.watcher.run(&e.cfg, &mut state, 600.0).unwrap().len(), 1);

        // Nothing has touched the file since; the signature is identical.
        let before = state["zedDbSignature"].clone();
        assert!(e.watcher.run(&e.cfg, &mut state, 700.0).unwrap().is_empty());
        assert_eq!(state["zedDbSignature"], before);
    }

    /// A thread with no summary falls back to its id, so the row is still
    /// identifiable in the timeline.
    #[test]
    fn a_summaryless_thread_falls_back_to_its_id() {
        let mut e = env();
        make_db(&e.db, &[("t1", "100", "x")]);
        let mut state = json!({});
        e.watcher.run(&e.cfg, &mut state, 500.0).unwrap();
        make_db(&e.db, &[("t9", "1", "")]);
        let rows = e.watcher.run(&e.cfg, &mut state, 600.0).unwrap();
        assert_eq!(rows[0]["entity"], "thread t9");
    }

    /// The schema is undocumented: a database with no id+updated_at table is
    /// recorded and skipped, not an error.
    #[test]
    fn an_unrecognised_schema_is_recorded_and_skipped() {
        let mut e = env();
        Connection::open(&e.db)
            .unwrap()
            .execute_batch("CREATE TABLE notes (body TEXT)")
            .unwrap();
        let mut state = json!({});
        assert!(e.watcher.run(&e.cfg, &mut state, 500.0).unwrap().is_empty());
        assert!(state["zedDbSignature"].is_string());
    }

    /// Upgrading from a Node data dir: the superseded `zedDbMtime` key is
    /// dropped rather than left to confuse the input marker forever.
    #[test]
    fn the_legacy_mtime_key_is_cleaned_up() {
        let mut e = env();
        make_db(&e.db, &[("t1", "100", "a")]);
        let mut state = json!({"zedDbMtime": "12345"});
        e.watcher.run(&e.cfg, &mut state, 500.0).unwrap();
        assert!(state.get("zedDbMtime").is_none());
    }

    /// A machine with no Zed installed is the common case, not an error.
    #[test]
    fn a_missing_database_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let mut w = ZedWatcher {
            candidate_paths: Some(vec![tmp.path().join("nope.db")]),
            data_dir: Some(tmp.path().to_path_buf()),
        };
        assert!(w
            .run(&crate::test_config(json!({})), &mut json!({}), 1.0)
            .unwrap()
            .is_empty());
    }

    /// Identifier quoting must survive a table or column name containing a
    /// double quote — the schema is discovered, never trusted.
    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(quote_ident("threads"), "\"threads\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn gate_reflects_the_config_toggle() {
        assert_eq!(
            ZedWatcher::default().gate(&crate::test_config(json!({}))),
            Gate::Run
        );
        assert_eq!(
            ZedWatcher::default().gate(&crate::test_config(json!({"agent": {"watch": {"zed": false}}}))),
            Gate::Skipped {
                enabled: false,
                available: false,
                reason: "disabled in config".to_string()
            }
        );
    }
}
