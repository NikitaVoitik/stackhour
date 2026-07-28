//! `stackhour data stats|export|prune`.
//!
//! stats: read-only, exact two-line human output + `--json` shape with
//! per-query 0 fallbacks. export: NDJSON with the header line first,
//! heartbeats ordered (time, id), wakatime-days ordered (date, project),
//! fromDate / '9999-12-31' unbounded-toDate logic, atomic wx 0600 tmp write.
//! prune: dry-run read-only; `--confirm` runs one BEGIN IMMEDIATE txn with
//! two deletes; STRICT `<` boundaries; UTC cutoffDate in messages.

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};
use stackhour_core::config::Config;
use stackhour_core::fsutil::{create_excl, fsync_dir_best_effort};
use stackhour_core::timeparse::{iso_date_utc, parse_time};
use stackhour_core::{js_display, json_num, Error, Result};
use std::ffi::OsString;
use std::fs;
use std::io::{BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `Number.MAX_SAFE_INTEGER` — the JS default for an unbounded `--to`, and the
/// sentinel that selects the `'9999-12-31'` upper date bound.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

const USAGE: &str = "usage: stackhour data <stats|export|prune> [options]";

/// What a prune did (or would do) — feeds the exact stdout lines.
#[derive(Debug, Clone)]
pub struct PruneOutcome {
    pub confirmed: bool,
    pub cutoff: f64,
    pub heartbeats_deleted: i64,
    pub wakatime_days_deleted: i64,
}

/// `rusqlite::Error` -> the workspace error type. The driver message is
/// forwarded verbatim, exactly as node:sqlite rethrew it (and never carries
/// file CONTENTS, so a malformed DB cannot leak private bytes).
fn sql_err(e: rusqlite::Error) -> Error {
    Error::msg(e.to_string())
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `path.resolve(p)` — absolutize against the cwd and normalize `.` / `..`
/// purely lexically (no symlink resolution, no stat), like Node's path.resolve.
fn resolve_path(p: &Path) -> PathBuf {
    let base = if p.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    };
    let joined = base.join(p);
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            // Never pop past the root: path.resolve('/..') === '/'.
            Component::ParentDir => {
                if out.parent().is_some() {
                    out.pop();
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// `path.dirname(p)`, with the '.'-for-bare-names fallback.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// `existingDb(dbPath, readOnly)`: refuse to touch a database that is not
/// already there (a plain open would CREATE it), then open without running any
/// DDL or migration — `stackhour data` is a reader first.
fn existing_db(db_path: &Path, read_only: bool) -> Result<Connection> {
    if !db_path.exists() {
        return Err(Error::msg(format!(
            "database does not exist: {}",
            db_path.display()
        )));
    }
    let flags = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(db_path, flags).map_err(sql_err)
}

/// `scalar(db, sql, fallback)` — a single-column count whose ENTIRE failure
/// mode (missing table on a legacy database, malformed SQL) degrades to the
/// fallback, so stats never dies on an optional table.
fn scalar(db: &Connection, sql: &str, fallback: i64) -> i64 {
    db.query_row(sql, [], |row| row.get::<_, Option<i64>>(0))
        .ok()
        .flatten()
        .unwrap_or(fallback)
}

/// Read-only stats over the DB (the `--json` shape).
///
/// The heartbeats query is deliberately UNGUARDED: a database without that
/// table is not a stackhour database and must surface the SQLite error.
pub fn stats(db_path: &Path) -> Result<Value> {
    let db = existing_db(db_path, true)?;
    let (count, first, last) = db
        .query_row(
            "SELECT count(*) count, min(time) first, max(time) last FROM heartbeats",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<f64>>(1)?,
                    row.get::<_, Option<f64>>(2)?,
                ))
            },
        )
        .map_err(sql_err)?;
    let database_bytes = fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

    let mut out = Map::new();
    out.insert(
        "dbPath".into(),
        Value::String(db_path.to_string_lossy().into_owned()),
    );
    out.insert("databaseBytes".into(), Value::from(database_bytes));
    out.insert("heartbeats".into(), Value::from(count));
    out.insert("firstHeartbeat".into(), opt_num(first));
    out.insert("lastHeartbeat".into(), opt_num(last));
    out.insert(
        "machines".into(),
        Value::from(scalar(
            &db,
            "SELECT count(DISTINCT machine) value FROM heartbeats",
            0,
        )),
    );
    out.insert(
        "projects".into(),
        Value::from(scalar(
            &db,
            "SELECT count(DISTINCT project) value FROM heartbeats",
            0,
        )),
    );
    out.insert(
        "agentStatuses".into(),
        Value::from(scalar(&db, "SELECT count(*) value FROM agent_status", 0)),
    );
    out.insert(
        "wakatimeDays".into(),
        Value::from(scalar(&db, "SELECT count(*) value FROM wakatime_days", 0)),
    );
    Ok(Value::Object(out))
}

/// `min()`/`max()` over an empty table yield SQL NULL -> JSON `null`.
fn opt_num(v: Option<f64>) -> Value {
    match v {
        Some(n) => Value::Number(json_num(n)),
        None => Value::Null,
    }
}

/// Removes the export tmp file on EVERY exit path (the JS `finally`), so a
/// failed or forced-but-broken export never leaves `*.tmp` litter behind.
struct TmpGuard(PathBuf);

impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// `<output>.<pid>.tmp`, built on the raw OS string so non-UTF-8 paths survive.
fn tmp_path_for(output: &Path) -> PathBuf {
    let mut name: OsString = output.as_os_str().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    PathBuf::from(name)
}

/// A SQLite row as JSON, generically by column name (`SELECT *`), so exports
/// of legacy databases carry whatever columns those actually have.
fn row_to_value(row: &rusqlite::Row<'_>, names: &[String]) -> rusqlite::Result<Value> {
    let mut obj = Map::new();
    for (i, name) in names.iter().enumerate() {
        let value = match row.get_ref(i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => Value::from(n),
            ValueRef::Real(f) => Value::Number(json_num(f)),
            ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
            // JSON.stringify of a Uint8Array is an index-keyed object.
            ValueRef::Blob(b) => {
                let mut m = Map::new();
                for (idx, byte) in b.iter().enumerate() {
                    m.insert(idx.to_string(), Value::from(*byte));
                }
                Value::Object(m)
            }
        };
        obj.insert(name.clone(), value);
    }
    Ok(Value::Object(obj))
}

fn column_names(stmt: &rusqlite::Statement<'_>) -> Vec<String> {
    stmt.column_names().into_iter().map(String::from).collect()
}

fn write_record(w: &mut impl Write, kind: &str, data: Value) -> Result<()> {
    let mut obj = Map::new();
    obj.insert("type".into(), Value::from(kind));
    obj.insert("data".into(), data);
    let line = serde_json::to_string(&Value::Object(obj))?;
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    Ok(())
}

/// NDJSON export. Returns (heartbeat_count, wakatime_day_count, output_path).
///
/// `from`/`to` are already-parsed epoch seconds; `None` means unbounded and
/// maps to `0` / `Number.MAX_SAFE_INTEGER`, the latter doubling as the
/// sentinel for the `'9999-12-31'` wakatime-day upper bound.
pub fn export(
    db_path: &Path,
    output: &Path,
    from: Option<f64>,
    to: Option<f64>,
    force: bool,
) -> Result<(u64, u64, PathBuf)> {
    let from_time = from.unwrap_or(0.0);
    let to_time = to.unwrap_or(MAX_SAFE_INTEGER);
    if from_time > to_time {
        return Err(Error::msg("from must not be after to"));
    }
    let db = existing_db(db_path, true)?;
    let resolved = resolve_path(output);

    let from_date = iso_date_utc(from_time);
    // An unbounded --to must not be turned into a real calendar date: the
    // wakatime_days key is TEXT, so the open end is spelled as a date that
    // sorts above every realistic value.
    let to_date = if to_time == MAX_SAFE_INTEGER {
        "9999-12-31".to_string()
    } else {
        iso_date_utc(to_time)
    };

    let mut hb_stmt = db
        .prepare("SELECT * FROM heartbeats WHERE time >= ? AND time <= ? ORDER BY time, id")
        .map_err(sql_err)?;
    let hb_columns = column_names(&hb_stmt);
    let mut day_stmt = db
        .prepare("SELECT * FROM wakatime_days WHERE date >= ? AND date <= ? ORDER BY date, project")
        .map_err(sql_err)?;
    let day_columns = column_names(&day_stmt);

    // --- atomic write ----------------------------------------------------
    // `exists()` follows symlinks, like fs.existsSync: an export onto a link
    // to a sensitive file reports "output exists", and --force REPLACES the
    // link (rename onto the link path) instead of writing through it.
    if resolved.exists() && !force {
        return Err(Error::msg(format!(
            "output exists: {}; pass --force to replace it",
            resolved.display()
        )));
    }
    let dir = parent_of(&resolved).to_path_buf();
    fs::create_dir_all(&dir)?;
    let tmp = tmp_path_for(&resolved);
    let _ = fs::remove_file(&tmp);
    let guard = TmpGuard(tmp.clone());

    let mut heartbeats: u64 = 0;
    let mut wakatime_days: u64 = 0;
    {
        let file = create_excl(&tmp, 0o600)?;
        let mut w = BufWriter::new(file);

        let mut header = Map::new();
        header.insert("type".into(), Value::from("stackhour-export"));
        header.insert("version".into(), Value::from(1));
        header.insert("createdAt".into(), Value::Number(json_num(now_seconds())));
        header.insert("from".into(), Value::Number(json_num(from_time)));
        header.insert("to".into(), Value::Number(json_num(to_time)));
        let line = serde_json::to_string(&Value::Object(header))?;
        w.write_all(line.as_bytes())?;
        w.write_all(b"\n")?;

        let mut rows = hb_stmt.query((from_time, to_time)).map_err(sql_err)?;
        while let Some(row) = rows.next().map_err(sql_err)? {
            let value = row_to_value(row, &hb_columns).map_err(sql_err)?;
            write_record(&mut w, "heartbeat", value)?;
            heartbeats += 1;
        }
        let mut rows = day_stmt
            .query((from_date.as_str(), to_date.as_str()))
            .map_err(sql_err)?;
        while let Some(row) = rows.next().map_err(sql_err)? {
            let value = row_to_value(row, &day_columns).map_err(sql_err)?;
            write_record(&mut w, "wakatime-day", value)?;
            wakatime_days += 1;
        }

        let file = w.into_inner().map_err(|e| Error::msg(e.to_string()))?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &resolved)?;
    drop(guard);
    fs::set_permissions(&resolved, fs::Permissions::from_mode(0o600))?;
    fsync_dir_best_effort(&dir);

    Ok((heartbeats, wakatime_days, resolved))
}

/// Delete rows strictly older than `before` (epoch seconds).
///
/// Without `confirm` the database is opened READ-ONLY, so a preview cannot
/// write even by accident. Both boundaries are STRICT `<`: a row exactly at
/// the cutoff (and the cutoff's own UTC day of imported data) is kept.
pub fn prune(db_path: &Path, before: f64, confirm: bool) -> Result<PruneOutcome> {
    let cutoff_date = iso_date_utc(before);
    let db = existing_db(db_path, !confirm)?;

    let heartbeats: i64 = db
        .query_row(
            "SELECT count(*) count FROM heartbeats WHERE time < ?",
            (before,),
            |row| row.get(0),
        )
        .map_err(sql_err)?;
    let wakatime_days: i64 = db
        .query_row(
            "SELECT count(*) count FROM wakatime_days WHERE date < ?",
            (cutoff_date.as_str(),),
            |row| row.get(0),
        )
        .map_err(sql_err)?;

    if !confirm {
        return Ok(PruneOutcome {
            confirmed: false,
            cutoff: before,
            heartbeats_deleted: heartbeats,
            wakatime_days_deleted: wakatime_days,
        });
    }

    db.execute_batch("BEGIN IMMEDIATE").map_err(sql_err)?;
    let deleted = (|| -> Result<()> {
        db.execute("DELETE FROM heartbeats WHERE time < ?", (before,))
            .map_err(sql_err)?;
        db.execute(
            "DELETE FROM wakatime_days WHERE date < ?",
            (cutoff_date.as_str(),),
        )
        .map_err(sql_err)?;
        db.execute_batch("COMMIT").map_err(sql_err)
    })();
    if let Err(err) = deleted {
        let _ = db.execute_batch("ROLLBACK"); // preserve the original error
        return Err(err);
    }

    Ok(PruneOutcome {
        confirmed: true,
        cutoff: before,
        heartbeats_deleted: heartbeats,
        wakatime_days_deleted: wakatime_days,
    })
}

/// `optionValues(args, name).at(-1)` — ONLY the `--name=value` form, last
/// occurrence wins.
fn last_option(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("--{name}=");
    args.iter()
        .filter_map(|a| a.strip_prefix(&prefix))
        .next_back()
        .map(str::to_string)
}

/// Exact-membership boolean flag (`args.includes('--json')`).
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// `String(x)` for a stats field, so the human line prints `1`, not `1.0`.
fn field_str(value: &Value, key: &str) -> String {
    value.get(key).map(js_display).unwrap_or_default()
}

/// The `stackhour data <subcommand>` CLI dispatch + exact output strings.
pub fn run_data(cfg: &Config, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("stats") => {
            let result = stats(&cfg.server.db)?;
            if has_flag(args, "--json") {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "{} heartbeats · {} machines · {} projects",
                    field_str(&result, "heartbeats"),
                    field_str(&result, "machines"),
                    field_str(&result, "projects"),
                );
                println!(
                    "{} imported days · {} bytes",
                    field_str(&result, "wakatimeDays"),
                    field_str(&result, "databaseBytes"),
                );
            }
            Ok(())
        }
        Some("export") => {
            let output = last_option(args, "output").ok_or_else(|| {
                // JS: `if (!outputPath)` — an empty --output= is falsy too.
                Error::msg("--output is required")
            })?;
            if output.is_empty() {
                return Err(Error::msg("--output is required"));
            }
            let from = parse_time(last_option(args, "from").as_deref(), "from")?;
            let to = parse_time(last_option(args, "to").as_deref(), "to")?;
            let (heartbeats, days, path) = export(
                &cfg.server.db,
                Path::new(&output),
                from,
                to,
                has_flag(args, "--force"),
            )?;
            println!(
                "Exported {heartbeats} heartbeats and {days} imported days to {}",
                path.display()
            );
            Ok(())
        }
        Some("prune") => {
            let before = parse_time(last_option(args, "before").as_deref(), "before")?
                .ok_or_else(|| Error::msg("--before is required"))?;
            let outcome = prune(&cfg.server.db, before, has_flag(args, "--confirm"))?;
            if outcome.confirmed {
                println!(
                    "Deleted {} heartbeats and {} imported days",
                    outcome.heartbeats_deleted, outcome.wakatime_days_deleted
                );
            } else {
                println!(
                    "Would delete {} heartbeats and {} imported days; rerun with --confirm",
                    outcome.heartbeats_deleted, outcome.wakatime_days_deleted
                );
            }
            Ok(())
        }
        _ => Err(Error::msg(USAGE)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{insert_heartbeats, open_db, upsert_wakatime_day};
    use serde_json::json;
    use tempfile::TempDir;

    /// A default config with only `server.db` pointed at the test database —
    /// the single key `stackhour data` reads.
    fn test_config(db: &Path) -> Config {
        let mut cfg =
            stackhour_core::config::load_config(Path::new("/nonexistent/stackhour-data-test/config.json"))
                .expect("defaults");
        cfg.server.db = db.to_path_buf();
        cfg
    }

    fn heartbeat(time: f64, extra: &[(&str, Value)]) -> Value {
        let mut v = json!({
            "time": time,
            "machine": "workstation",
            "source": "editor-files",
            "project": "stackhour",
            "entity": format!("/work/file-{time}.js"),
            "actor": "human",
        });
        for (k, val) in extra {
            v[*k] = val.clone();
        }
        v
    }

    fn fixture() -> (TempDir, PathBuf, Connection) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("stackhour.db");
        let db = open_db(&path).expect("open_db");
        (dir, path, db)
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        let text = fs::read_to_string(path).expect("read export");
        text.trim_end()
            .split('\n')
            .map(|l| serde_json::from_str(l).expect("json line"))
            .collect()
    }

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn tmp_path_appends_pid_suffix() {
        let tmp = tmp_path_for(Path::new("/tmp/export.jsonl"));
        let name = tmp.to_string_lossy().into_owned();
        assert!(name.starts_with("/tmp/export.jsonl."), "{name}");
        assert_eq!(
            Path::new(&name).extension(),
            Some(std::ffi::OsStr::new("tmp")),
            "{name}"
        );
        assert!(name.contains(&std::process::id().to_string()));
    }

    #[test]
    fn resolve_path_is_lexical() {
        assert_eq!(resolve_path(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
        assert_eq!(resolve_path(Path::new("/..")), PathBuf::from("/"));
        assert!(resolve_path(Path::new("rel.jsonl")).is_absolute());
    }

    #[test]
    fn last_option_and_flags_follow_js_parsing() {
        let args: Vec<String> = ["export", "--output=a", "--output=b", "--force"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(last_option(&args, "output").as_deref(), Some("b"));
        assert_eq!(last_option(&args, "from"), None);
        assert!(has_flag(&args, "--force"));
        assert!(!has_flag(&args, "--confirm"));
        // The space-separated form is NOT supported.
        let spaced: Vec<String> = vec!["--output".into(), "x".into()];
        assert_eq!(last_option(&spaced, "output"), None);
    }

    // ---- stats ------------------------------------------------------------

    #[test]
    fn stats_reports_an_empty_database_without_inventing_coverage() {
        let (_dir, path, db) = fixture();
        drop(db);
        let s = stats(&path).expect("stats");
        assert_eq!(s["heartbeats"], json!(0));
        assert_eq!(s["firstHeartbeat"], Value::Null);
        assert_eq!(s["lastHeartbeat"], Value::Null);
        assert_eq!(s["machines"], json!(0));
        assert_eq!(s["projects"], json!(0));
        assert_eq!(s["agentStatuses"], json!(0));
        assert_eq!(s["wakatimeDays"], json!(0));
        assert!(s["databaseBytes"].as_u64().unwrap_or(0) > 0);
        assert_eq!(s["dbPath"], json!(path.to_string_lossy()));
    }

    #[test]
    fn stats_counts_distinct_dimensions_and_exact_coverage() {
        let (_dir, path, mut db) = fixture();
        insert_heartbeats(
            &mut db,
            &[
                heartbeat(30.0, &[("machine", json!("laptop")), ("project", json!("api"))]),
                heartbeat(10.0, &[("machine", json!("desktop")), ("project", json!("web"))]),
                heartbeat(20.0, &[("machine", json!("desktop")), ("project", json!("api"))]),
            ],
        )
        .expect("insert");
        upsert_wakatime_day(&db, "2026-07-17", "legacy", 123.0).expect("day");
        upsert_wakatime_day(&db, "2026-07-18", "legacy", 456.0).expect("day");
        drop(db);

        let s = stats(&path).expect("stats");
        assert_eq!(s["heartbeats"], json!(3));
        assert_eq!(s["firstHeartbeat"], json!(10));
        assert_eq!(s["lastHeartbeat"], json!(30));
        assert_eq!(s["machines"], json!(2));
        assert_eq!(s["projects"], json!(2));
        assert_eq!(s["wakatimeDays"], json!(2));
    }

    #[test]
    fn stats_key_order_matches_the_js_json_shape() {
        let (_dir, path, db) = fixture();
        drop(db);
        let s = stats(&path).expect("stats");
        let keys: Vec<&str> = s
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "dbPath",
                "databaseBytes",
                "heartbeats",
                "firstHeartbeat",
                "lastHeartbeat",
                "machines",
                "projects",
                "agentStatuses",
                "wakatimeDays",
            ]
        );
    }

    #[test]
    fn stats_tolerates_legacy_databases_without_optional_tables() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("legacy.db");
        {
            let db = Connection::open(&path).expect("open");
            db.execute_batch("CREATE TABLE heartbeats (time REAL, machine TEXT, project TEXT)")
                .expect("ddl");
            db.execute("INSERT INTO heartbeats VALUES (?, ?, ?)", (12.0, "old", "legacy"))
                .expect("insert");
        }
        let s = stats(&path).expect("stats");
        assert_eq!(s["heartbeats"], json!(1));
        assert_eq!(s["machines"], json!(1));
        assert_eq!(s["projects"], json!(1));
        assert_eq!(s["agentStatuses"], json!(0));
        assert_eq!(s["wakatimeDays"], json!(0));
    }

    #[test]
    fn stats_refuses_a_missing_database_and_never_leaks_file_contents() {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("missing.db");
        let err = stats(&missing).expect_err("missing");
        assert!(err.message().starts_with("database does not exist:"));
        assert!(!missing.exists());

        let malformed = dir.path().join("malformed.db");
        fs::write(&malformed, "PRIVATE-CONTENTS-THAT-MUST-NOT-LEAK").expect("write");
        let err = stats(&malformed).expect_err("malformed");
        assert!(!err.message().contains("PRIVATE-CONTENTS"));
    }

    // ---- export -----------------------------------------------------------

    #[test]
    fn export_writes_a_header_then_ordered_inclusive_rows() {
        let (dir, path, mut db) = fixture();
        let start = 1_784_332_800.0; // 2026-07-18T00:00:00Z
        insert_heartbeats(
            &mut db,
            &[
                heartbeat(
                    start + 20.0,
                    &[("entity", json!("/z.js")), ("project", json!("zeta"))],
                ),
                heartbeat(start - 1.0, &[("entity", json!("/before.js"))]),
                heartbeat(start + 10.0, &[("entity", json!("/b.js"))]),
                heartbeat(
                    start + 10.0,
                    &[
                        ("entity", json!("/a.js")),
                        ("source", json!("codex-cli")),
                        ("actor", json!("agent")),
                    ],
                ),
                heartbeat(start + 30.0, &[("entity", json!("/after.js"))]),
            ],
        )
        .expect("insert");
        upsert_wakatime_day(&db, "2026-07-17", "old", 1.0).expect("day");
        upsert_wakatime_day(&db, "2026-07-18", "zeta", 2.0).expect("day");
        upsert_wakatime_day(&db, "2026-07-18", "alpha", 3.0).expect("day");
        upsert_wakatime_day(&db, "2026-07-19", "new", 4.0).expect("day");
        drop(db);

        let output = dir.path().join("nested").join("export.jsonl");
        let (hb, days, resolved) =
            export(&path, &output, Some(start + 10.0), Some(start + 20.0), false).expect("export");
        assert_eq!((hb, days), (3, 2));
        assert_eq!(resolved, output);

        let lines = read_lines(&output);
        assert_eq!(lines[0]["type"], json!("stackhour-export"));
        assert_eq!(lines[0]["version"], json!(1));
        assert_eq!(lines[0]["from"].as_f64(), Some(start + 10.0));
        assert_eq!(lines[0]["to"].as_f64(), Some(start + 20.0));
        // Integral REALs serialize as JS integers, not `1784332810.0`.
        assert_eq!(lines[0]["from"].to_string(), "1784332810");
        let hbs: Vec<(&str, f64, &str)> = lines[1..4]
            .iter()
            .map(|l| {
                (
                    l["type"].as_str().unwrap_or(""),
                    l["data"]["time"].as_f64().unwrap_or(0.0),
                    l["data"]["entity"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            hbs,
            vec![
                ("heartbeat", start + 10.0, "/b.js"),
                ("heartbeat", start + 10.0, "/a.js"),
                ("heartbeat", start + 20.0, "/z.js"),
            ]
        );
        let ds: Vec<(&str, &str, &str)> = lines[4..]
            .iter()
            .map(|l| {
                (
                    l["type"].as_str().unwrap_or(""),
                    l["data"]["date"].as_str().unwrap_or(""),
                    l["data"]["project"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            ds,
            vec![
                ("wakatime-day", "2026-07-18", "alpha"),
                ("wakatime-day", "2026-07-18", "zeta"),
            ]
        );
        // Full raw rows, including the DB-assigned columns.
        assert!(lines[1]["data"]["id"].as_i64().unwrap_or(0) > 0);
        assert!(lines[1]["data"]["created_at"].as_f64().unwrap_or(0.0) > 0.0);
    }

    #[test]
    fn export_defaults_span_everything_and_use_the_open_ended_date_bound() {
        let (dir, path, mut db) = fixture();
        insert_heartbeats(&mut db, &[heartbeat(10.0, &[])]).expect("insert");
        upsert_wakatime_day(&db, "9999-12-30", "far-future", 1.0).expect("day");
        drop(db);

        let output = dir.path().join("all.jsonl");
        let (hb, days, _) = export(&path, &output, None, None, false).expect("export");
        assert_eq!((hb, days), (1, 1));
        let lines = read_lines(&output);
        assert_eq!(lines[0]["from"].to_string(), "0");
        assert_eq!(lines[0]["to"].to_string(), "9007199254740991");
    }

    #[test]
    fn export_rejects_an_inverted_range_before_creating_anything() {
        let (dir, path, db) = fixture();
        drop(db);
        let output = dir.path().join("bad.jsonl");
        let err = export(&path, &output, Some(20.0), Some(10.0), false).expect_err("inverted");
        assert_eq!(err.message(), "from must not be after to");
        assert!(!output.exists());
    }

    #[test]
    fn export_creates_private_files_refuses_overwrite_and_force_replaces() {
        let (dir, path, mut db) = fixture();
        insert_heartbeats(&mut db, &[heartbeat(10.0, &[])]).expect("insert");
        drop(db);
        let output = dir.path().join("export.jsonl");

        export(&path, &output, None, None, false).expect("first");
        let mode = fs::metadata(&output).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let original = fs::read_to_string(&output).expect("read");

        let err = export(&path, &output, None, None, false).expect_err("exists");
        assert!(err.message().starts_with("output exists: "));
        assert!(err.message().ends_with("; pass --force to replace it"));
        assert_eq!(fs::read_to_string(&output).expect("read"), original);
        assert!(no_tmp_files(dir.path()));

        export(&path, &output, Some(0.0), None, true).expect("forced");
        assert_eq!(
            fs::metadata(&output).expect("stat").permissions().mode() & 0o777,
            0o600
        );
        assert!(no_tmp_files(dir.path()));
    }

    #[test]
    fn a_failed_export_cleans_its_tmp_and_preserves_the_destination() {
        let (dir, path, db) = fixture();
        drop(db);
        let destination = dir.path().join("destination");
        fs::create_dir(&destination).expect("mkdir");

        let err = export(&path, &destination, None, None, true).expect_err("dir target");
        assert!(!err.message().is_empty());
        assert!(destination.is_dir());
        assert!(no_tmp_files(dir.path()));
    }

    #[test]
    fn export_does_not_follow_a_destination_symlink() {
        let (dir, path, db) = fixture();
        drop(db);
        let target = dir.path().join("sensitive.txt");
        let output = dir.path().join("export.jsonl");
        fs::write(&target, "DO NOT REPLACE").expect("write");
        std::os::unix::fs::symlink(&target, &output).expect("symlink");

        let err = export(&path, &output, None, None, false).expect_err("exists");
        assert!(err.message().starts_with("output exists: "));
        assert_eq!(fs::read_to_string(&target).expect("read"), "DO NOT REPLACE");

        export(&path, &output, None, None, true).expect("forced");
        assert!(!fs::symlink_metadata(&output)
            .expect("lstat")
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&target).expect("read"), "DO NOT REPLACE");
    }

    fn no_tmp_files(dir: &Path) -> bool {
        fs::read_dir(dir)
            .map(|entries| {
                !entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            })
            .unwrap_or(false)
    }

    // ---- prune ------------------------------------------------------------

    #[test]
    fn prune_dry_run_reports_strict_boundaries_and_writes_nothing() {
        let (_dir, path, mut db) = fixture();
        let cutoff = 1_784_376_000.0; // 2026-07-18T12:00:00Z
        insert_heartbeats(
            &mut db,
            &[
                heartbeat(cutoff - 1.0, &[]),
                heartbeat(cutoff, &[]),
                heartbeat(cutoff + 1.0, &[]),
            ],
        )
        .expect("insert");
        upsert_wakatime_day(&db, "2026-07-17", "old", 1.0).expect("day");
        upsert_wakatime_day(&db, "2026-07-18", "boundary", 2.0).expect("day");
        drop(db);

        let outcome = prune(&path, cutoff, false).expect("dry run");
        assert!(!outcome.confirmed);
        assert_eq!(outcome.heartbeats_deleted, 1);
        assert_eq!(outcome.wakatime_days_deleted, 1);

        let after = stats(&path).expect("stats");
        assert_eq!(after["heartbeats"], json!(3));
        assert_eq!(after["wakatimeDays"], json!(2));
    }

    #[test]
    fn confirmed_prune_deletes_only_rows_strictly_before_each_boundary() {
        let (_dir, path, mut db) = fixture();
        let cutoff = 1_784_332_800.0; // 2026-07-18T00:00:00Z
        insert_heartbeats(
            &mut db,
            &[
                heartbeat(cutoff - 0.001, &[]),
                heartbeat(cutoff, &[]),
                heartbeat(cutoff + 0.001, &[]),
            ],
        )
        .expect("insert");
        upsert_wakatime_day(&db, "2026-07-17", "old", 1.0).expect("day");
        upsert_wakatime_day(&db, "2026-07-18", "boundary", 2.0).expect("day");
        upsert_wakatime_day(&db, "2026-07-19", "new", 3.0).expect("day");
        drop(db);

        let outcome = prune(&path, cutoff, true).expect("prune");
        assert!(outcome.confirmed);
        assert_eq!(outcome.heartbeats_deleted, 1);
        assert_eq!(outcome.wakatime_days_deleted, 1);

        let db = Connection::open(&path).expect("reopen");
        let remaining: i64 = db
            .query_row("SELECT count(*) FROM heartbeats", [], |r| r.get(0))
            .expect("count");
        assert_eq!(remaining, 2);
        let dates: Vec<String> = {
            let mut stmt = db
                .prepare("SELECT date FROM wakatime_days ORDER BY date")
                .expect("prepare");
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).expect("query");
            rows.flatten().collect()
        };
        assert_eq!(dates, vec!["2026-07-18", "2026-07-19"]);
    }

    #[test]
    fn confirmed_prune_rolls_back_when_the_second_delete_fails() {
        let (_dir, path, mut db) = fixture();
        insert_heartbeats(&mut db, &[heartbeat(1.0, &[]), heartbeat(20.0, &[])]).expect("insert");
        upsert_wakatime_day(&db, "1970-01-01", "legacy", 1.0).expect("day");
        db.execute_batch(
            "CREATE TRIGGER reject_wakatime_prune BEFORE DELETE ON wakatime_days
             BEGIN SELECT RAISE(ABORT, 'simulated prune failure'); END;",
        )
        .expect("trigger");
        drop(db);

        let err = prune(&path, 86_400.0, true).expect_err("abort");
        assert!(err.message().contains("simulated prune failure"));

        let after = stats(&path).expect("stats");
        assert_eq!(after["heartbeats"], json!(2));
        assert_eq!(after["wakatimeDays"], json!(1));
    }

    #[test]
    fn prune_never_creates_a_missing_database() {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("missing.db");
        let err = prune(&missing, 10.0, true).expect_err("missing");
        assert!(err.message().starts_with("database does not exist:"));
        assert!(!missing.exists());
    }

    // ---- CLI dispatch -----------------------------------------------------

    #[test]
    fn unknown_and_missing_subcommands_report_the_usage_string() {
        let cfg = test_config(Path::new("/nonexistent/stackhour.db"));
        let err = run_data(&cfg, &[]).expect_err("no subcommand");
        assert_eq!(err.message(), USAGE);
        let err = run_data(&cfg, &["unknown".to_string()]).expect_err("unknown");
        assert_eq!(err.message(), USAGE);
    }

    #[test]
    fn export_requires_an_output_before_touching_the_database() {
        let cfg = test_config(Path::new("/nonexistent/stackhour.db"));
        let err = run_data(&cfg, &["export".to_string()]).expect_err("no output");
        assert_eq!(err.message(), "--output is required");
    }

    #[test]
    fn prune_requires_a_cutoff_before_touching_the_database() {
        let cfg = test_config(Path::new("/nonexistent/stackhour.db"));
        let err = run_data(&cfg, &["prune".to_string()]).expect_err("no cutoff");
        assert_eq!(err.message(), "--before is required");
        let err =
            run_data(&cfg, &["prune".to_string(), "--before=nope".to_string()]).expect_err("bad cutoff");
        assert_eq!(err.message(), "before must be a Unix timestamp or ISO date");
    }
}
