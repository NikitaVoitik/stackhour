//! `stackhour backup create|verify`.
//!
//! create: quick_check source -> rusqlite backup API into a pid tmp ->
//! wal_checkpoint(TRUNCATE) (busy -> error) -> journal_mode DELETE -> sidecar
//! (-wal/-shm) removal -> verify -> chmod 0600 -> rename -> dir fsync ->
//! re-verify; a cleanup guard removes the tmp on every failure path.
//! verify: URI open `file:…?immutable=1` read-only, quick_check must return
//! exactly one 'ok' row, the heartbeats table must exist. All exact error
//! strings.

use crate::db::open_immutable;
use crate::restore::restore_backup;
use rusqlite::{Connection, OpenFlags};
use stackhour_core::config::Config;
use stackhour_core::fsutil::fsync_dir_best_effort;
use stackhour_core::timeparse::iso_ts_for_filename;
use stackhour_core::{Error, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What a create/verify produced (feeds the exact CLI stdout lines).
#[derive(Debug, Clone)]
pub struct BackupInfo {
    pub ok: bool,
    pub backup_path: PathBuf,
    pub bytes: u64,
    pub heartbeats: i64,
    pub tables: i64,
    /// create: the final output path (differs from backup_path pre-rename).
    pub output_path: Option<PathBuf>,
}

/// `rusqlite::Error` -> the workspace error type. The SQLite driver message is
/// forwarded verbatim, exactly as node:sqlite rethrew it.
fn sql_err(e: rusqlite::Error) -> Error {
    Error::msg(e.to_string())
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

/// `path.dirname(p)`, with the same '.'-for-bare-names fallback used elsewhere.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// `<path>-wal` / `<path>-shm` — the SQLite sidecars, as plain string suffixes
/// (NOT extensions: `live.db` -> `live.db-wal`).
fn sidecars(path: &Path) -> [PathBuf; 2] {
    let suffix = |s: &str| {
        let mut p = path.as_os_str().to_os_string();
        p.push(s);
        PathBuf::from(p)
    };
    [suffix("-wal"), suffix("-shm")]
}

/// `fs.rmSync(file, { force: true })` — a missing file is not an error.
fn rm_force(path: &Path) {
    let _ = fs::remove_file(path);
}

/// `requireFile`: the path must stat as a regular file. Any failure (missing,
/// a directory, a stat error) collapses into the single JS message.
fn require_file(file: &Path, label: &str) -> Result<()> {
    if fs::metadata(file).map(|m| m.is_file()).unwrap_or(false) {
        Ok(())
    } else {
        Err(Error::msg(format!(
            "{label} does not exist or is not a file: {}",
            file.display()
        )))
    }
}

/// `fsyncFile` — open for reading and fsync. Unlike `fsyncDir`, errors here are
/// NOT swallowed (the JS lets them propagate).
fn fsync_file(file: &Path) -> Result<()> {
    fs::File::open(file)?.sync_all()?;
    Ok(())
}

fn chmod_0600(file: &Path) -> Result<()> {
    fs::set_permissions(file, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Removes `tmp`, `tmp-wal` and `tmp-shm` on every exit path — the Rust
/// equivalent of the JS `finally { for (…) fs.rmSync(file, { force: true }); }`.
struct TmpGuard(PathBuf);

impl Drop for TmpGuard {
    fn drop(&mut self) {
        rm_force(&self.0);
        for sidecar in sidecars(&self.0) {
            rm_force(&sidecar);
        }
    }
}

/// `finalizeSnapshot`: collapse the freshly written snapshot into a single
/// standalone file — checkpoint the WAL (a busy checkpoint means the snapshot
/// is not self-contained), switch to the DELETE journal, then drop the
/// sidecars.
fn finalize_snapshot(file: &Path) -> Result<()> {
    {
        let db = Connection::open(file).map_err(sql_err)?;
        db.pragma_update(None, "busy_timeout", 5000i64).map_err(sql_err)?;
        // PRAGMA wal_checkpoint(TRUNCATE) -> (busy, log, checkpointed).
        let busy: i64 = db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .map_err(sql_err)?;
        if busy != 0 {
            return Err(Error::msg("backup snapshot is busy"));
        }
        // journal_mode returns a row, so it must go through query_row.
        db.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))
            .map_err(sql_err)?;
    }
    for sidecar in sidecars(file) {
        rm_force(&sidecar);
    }
    Ok(())
}

/// Verify a backup file without touching it (immutable URI open).
///
/// `immutable=1` is load-bearing: a plain read-only open would still create
/// `-wal`/`-shm` sidecars next to the file, and both `backup verify` and the
/// restore *preview* must be genuinely zero-write.
pub fn verify_backup(path: &Path) -> Result<BackupInfo> {
    let backup_path = resolve_path(path);
    require_file(&backup_path, "backup")?;

    // node:sqlite opens eagerly, so a non-database file fails at open under the
    // `cannot open backup:` prefix. rusqlite defers the header read until the
    // first statement, so the probe below stands in for that eager open.
    let db = open_immutable(&backup_path)
        .and_then(|db| {
            db.query_row("PRAGMA schema_version", [], |_| Ok(()))
                .map_err(sql_err)?;
            Ok(db)
        })
        .map_err(|err| Error::msg(format!("cannot open backup: {}", err.message())))?;

    let checks: Vec<String> = db
        .prepare("PRAGMA quick_check")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<String>>>()
        })
        .map_err(sql_err)?;
    if checks.len() != 1 || checks[0] != "ok" {
        return Err(Error::msg("SQLite quick_check failed"));
    }

    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<String>>>()
        })
        .map_err(sql_err)?;
    if !tables.iter().any(|name| name == "heartbeats") {
        return Err(Error::msg("not a Stackhour database (heartbeats table missing)"));
    }

    let heartbeats: i64 = db
        .query_row("SELECT count(*) count FROM heartbeats", [], |row| row.get(0))
        .map_err(sql_err)?;
    let bytes = fs::metadata(&backup_path)?.len();

    Ok(BackupInfo {
        ok: true,
        backup_path,
        bytes,
        heartbeats,
        // The JS returns the table NAME LIST here; the Rust surface carries the
        // count, which is all any caller (CLI output, restore preview) reads.
        tables: tables.len() as i64,
        output_path: None,
    })
}

/// The default output path: `<dirname(source)>/backups/stackhour-<ts>.db`.
fn default_destination(source: &Path, now_ms: i64) -> PathBuf {
    parent_of(source)
        .join("backups")
        .join(format!("stackhour-{}.db", iso_ts_for_filename(now_ms)))
}

/// Create a verified backup of `db_path`. `now_ms` feeds the
/// `stackhour-backup-<iso-with-dashes>.db` default filename.
pub fn create_backup(db_path: &Path, output: Option<&Path>, force: bool, now_ms: i64) -> Result<BackupInfo> {
    let source_path = resolve_path(db_path);
    require_file(&source_path, "database")?;

    let destination = match output {
        Some(out) => resolve_path(out),
        None => resolve_path(&default_destination(&source_path, now_ms)),
    };
    if destination == source_path {
        return Err(Error::msg("backup output must differ from the database"));
    }
    // existsSync follows symlinks: an output symlink pointing at a live file
    // needs --force, and the rename below then replaces the LINK, leaving its
    // referent untouched.
    if destination.exists() && !force {
        return Err(Error::msg(format!(
            "backup exists: {}; pass --force to replace it",
            destination.display()
        )));
    }
    fs::create_dir_all(parent_of(&destination))?;

    let tmp = {
        let mut p = destination.as_os_str().to_os_string();
        p.push(format!(".{}.tmp", std::process::id()));
        PathBuf::from(p)
    };
    // Clear leftovers from a crashed run with the same pid before writing, so a
    // stale snapshot cannot be mistaken for this one.
    rm_force(&tmp);
    for sidecar in sidecars(&tmp) {
        rm_force(&sidecar);
    }
    let _guard = TmpGuard(tmp.clone());

    {
        // READ_ONLY, deliberately NOT immutable: the source may be a live WAL
        // database and committed-but-uncheckpointed rows must be included.
        let source = Connection::open_with_flags(
            &source_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sql_err)?;
        let check: String = source
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(sql_err)?;
        if check != "ok" {
            return Err(Error::msg("source database failed SQLite quick_check"));
        }

        // The online-backup API, NOT a file copy: it reads through the WAL, so
        // committed WAL rows land in the snapshot.
        let mut snapshot = Connection::open(&tmp).map_err(sql_err)?;
        {
            let backup = rusqlite::backup::Backup::new(&source, &mut snapshot).map_err(sql_err)?;
            backup
                .run_to_completion(1000, Duration::from_millis(0), None)
                .map_err(sql_err)?;
        }
        snapshot.close().map_err(|(_, e)| sql_err(e))?;
        source.close().map_err(|(_, e)| sql_err(e))?;
    }

    finalize_snapshot(&tmp)?;
    verify_backup(&tmp)?;
    chmod_0600(&tmp)?;
    fsync_file(&tmp)?;

    // A stale sidecar next to the destination would shadow the new snapshot.
    for sidecar in sidecars(&destination) {
        rm_force(&sidecar);
    }
    fs::rename(&tmp, &destination)?;
    chmod_0600(&destination)?;
    fsync_dir_best_effort(parent_of(&destination));

    let mut verified = verify_backup(&destination)?;
    verified.output_path = Some(destination);
    Ok(verified)
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// `optionValues(args, name).at(-1)` — only the `--name=value` form is
/// recognized, and the LAST occurrence wins.
fn last_option(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("--{name}=");
    args.iter()
        .filter_map(|a| a.strip_prefix(&prefix))
        .next_back()
        .map(str::to_string)
}

/// Exact-membership boolean flag.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const USAGE: &str = "usage: stackhour backup <create|verify FILE|restore FILE> [options]";

/// The `stackhour backup create|verify|restore` CLI: dispatch + exact stdout
/// strings (restore is delegated to `crate::restore`).
pub fn run_backup_cli(cfg: &Config, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create") => {
            let output = last_option(args, "output").map(PathBuf::from);
            let result = create_backup(
                &cfg.server.db,
                output.as_deref(),
                has_flag(args, "--force"),
                now_ms(),
            )?;
            // output_path is always Some on the create path.
            let path = result.output_path.unwrap_or(result.backup_path);
            println!(
                "Created backup {} ({} heartbeats)",
                path.display(),
                result.heartbeats
            );
            Ok(())
        }
        Some("verify") => {
            // JS hands args[1] straight to path.resolve, which throws a
            // TypeError when it is undefined; the port says so plainly instead.
            let file = args
                .get(1)
                .ok_or_else(|| Error::msg("backup file path is required"))?;
            let result = verify_backup(Path::new(file))?;
            println!(
                "Backup OK: {} ({} heartbeats)",
                result.backup_path.display(),
                result.heartbeats
            );
            Ok(())
        }
        Some("restore") => {
            let file = args.get(1).ok_or_else(|| Error::msg("backup file is required"))?;
            let result = restore_backup(
                &cfg.server.db,
                Path::new(file),
                has_flag(args, "--confirm"),
                now_ms(),
            )?;
            if result.dry_run {
                println!(
                    "Would restore {} to {}; rerun with --confirm after stopping stackhour-server",
                    result.source.display(),
                    result.target.display()
                );
            } else {
                let previous = match &result.rollback_path {
                    Some(p) => format!("; previous database: {}", p.display()),
                    None => String::new(),
                };
                println!(
                    "Restored {} heartbeats to {}{}",
                    result.heartbeats,
                    result.target.display(),
                    previous
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

    /// A minimal WAL database with the columns the reference test fixture uses.
    fn make_db(file: &Path, entities: &[&str]) -> Connection {
        fs::create_dir_all(parent_of(file)).expect("mkdir");
        let db = Connection::open(file).expect("open");
        db.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .expect("wal");
        db.execute_batch(
            "CREATE TABLE heartbeats (
               id INTEGER PRIMARY KEY,
               time REAL NOT NULL,
               machine TEXT NOT NULL,
               source TEXT NOT NULL,
               project TEXT NOT NULL,
               entity TEXT NOT NULL,
               actor TEXT NOT NULL DEFAULT 'human'
             );",
        )
        .expect("ddl");
        for (index, entity) in entities.iter().enumerate() {
            db.execute(
                "INSERT INTO heartbeats (time, machine, source, project, entity, actor)
                 VALUES (?, 'machine', 'test', 'project', ?, 'human')",
                rusqlite::params![(index + 1) as f64, entity],
            )
            .expect("insert");
        }
        db
    }

    fn entities(file: &Path) -> Vec<String> {
        let db = open_immutable(file).expect("open");
        let mut stmt = db
            .prepare("SELECT entity FROM heartbeats ORDER BY id")
            .expect("prepare");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("rows");
        rows
    }

    fn mode_of(file: &Path) -> u32 {
        fs::metadata(file).expect("stat").permissions().mode() & 0o777
    }

    fn tmp_for(destination: &Path) -> PathBuf {
        let mut p = destination.as_os_str().to_os_string();
        p.push(format!(".{}.tmp", std::process::id()));
        PathBuf::from(p)
    }

    /// The load-bearing assertion: rows still sitting in the WAL must reach the
    /// snapshot, which a plain file copy would miss.
    #[test]
    fn create_snapshots_committed_wal_rows_into_a_standalone_0600_file() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let source = dir.path().join("live.db");
        let db = make_db(&source, &["wal-one"]);
        db.execute(
            "INSERT INTO heartbeats (time, machine, source, project, entity, actor)
             VALUES (2, 'machine', 'test', 'project', 'wal-two', 'human')",
            [],
        )
        .expect("insert");
        assert!(sidecars(&source)[0].exists(), "the -wal must still be live");

        let output = dir.path().join("snapshots").join("live.db");
        let result = create_backup(&source, Some(&output), false, 0).expect("create");

        assert_eq!(result.output_path.as_deref(), Some(output.as_path()));
        assert_eq!(result.heartbeats, 2);
        assert!(result.ok && result.bytes > 0);
        assert_eq!(entities(&output), vec!["wal-one", "wal-two"]);
        assert_eq!(mode_of(&output), 0o600);
        for sidecar in sidecars(&output) {
            assert!(!sidecar.exists(), "{} must not exist", sidecar.display());
        }
        drop(db);
    }

    #[test]
    fn create_uses_the_timestamped_default_path() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let source = dir.path().join("stackhour.db");
        drop(make_db(&source, &["source"]));
        // Date.parse('2026-07-18T12:34:56.789Z')
        let now = 1_784_378_096_789i64;

        let result = create_backup(&source, None, false, now).expect("create");
        assert_eq!(
            result.output_path.as_deref(),
            Some(
                dir.path()
                    .join("backups")
                    .join("stackhour-2026-07-18T12-34-56-789Z.db")
                    .as_path()
            )
        );
        assert_eq!(entities(&result.backup_path), vec!["source"]);
    }

    #[test]
    fn create_replaces_an_existing_output_only_with_force() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let source = dir.path().join("live.db");
        let custom = dir.path().join("custom.db");
        drop(make_db(&source, &["source"]));
        drop(make_db(&custom, &["old-output"]));

        let err = create_backup(&source, Some(&custom), false, 0).expect_err("must refuse");
        assert_eq!(
            err.message(),
            format!("backup exists: {}; pass --force to replace it", custom.display())
        );
        assert_eq!(entities(&custom), vec!["old-output"]);

        create_backup(&source, Some(&custom), true, 0).expect("forced");
        assert_eq!(entities(&custom), vec!["source"]);
        assert_eq!(mode_of(&custom), 0o600);
    }

    #[test]
    fn create_refuses_the_source_path() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let source = dir.path().join("live.db");
        drop(make_db(&source, &["source"]));
        let err = create_backup(&source, Some(&source), true, 0).expect_err("must refuse");
        assert_eq!(err.message(), "backup output must differ from the database");
    }

    /// A destination symlink is REPLACED by the rename; its referent is left
    /// exactly as it was.
    #[test]
    fn create_replaces_a_destination_symlink_without_touching_its_referent() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let source = dir.path().join("source.db");
        let referent = dir.path().join("referent.db");
        let link = dir.path().join("backup.db");
        drop(make_db(&source, &["source"]));
        drop(make_db(&referent, &["referent"]));
        std::os::unix::fs::symlink(&referent, &link).expect("symlink");

        assert!(create_backup(&source, Some(&link), false, 0).is_err());
        create_backup(&source, Some(&link), true, 0).expect("forced");

        assert!(!fs::symlink_metadata(&link)
            .expect("lstat")
            .file_type()
            .is_symlink());
        assert_eq!(entities(&link), vec!["source"]);
        assert_eq!(entities(&referent), vec!["referent"]);
    }

    #[test]
    fn failed_create_removes_tmp_files_and_preserves_the_destination() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let corrupt = dir.path().join("corrupt.db");
        let destination = dir.path().join("destination.db");
        fs::write(&corrupt, b"not sqlite").expect("write");
        drop(make_db(&destination, &["keep"]));

        create_backup(&corrupt, Some(&destination), true, 0).expect_err("must fail");
        assert_eq!(entities(&destination), vec!["keep"]);

        let tmp = tmp_for(&destination);
        assert!(!tmp.exists());
        for sidecar in sidecars(&tmp) {
            assert!(!sidecar.exists());
        }
    }

    #[test]
    fn create_rejects_a_missing_source() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let missing = dir.path().join("missing.db");
        let err = create_backup(&missing, None, false, 0).expect_err("must fail");
        assert_eq!(
            err.message(),
            format!("database does not exist or is not a file: {}", missing.display())
        );
    }

    #[test]
    fn verify_reports_metadata_for_a_valid_backup() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let valid = dir.path().join("valid.db");
        drop(make_db(&valid, &["one", "two"]));

        let verified = verify_backup(&valid).expect("verify");
        assert!(verified.ok);
        assert_eq!(verified.backup_path, valid);
        assert_eq!(verified.heartbeats, 2);
        assert!(verified.bytes > 0);
        assert!(verified.tables >= 1);
        assert_eq!(verified.output_path, None);
    }

    /// The immutable open is what keeps a verify genuinely zero-write.
    #[test]
    fn verify_creates_no_sidecars() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let valid = dir.path().join("valid.db");
        drop(make_db(&valid, &["one"]));
        // Collapse the WAL first, exactly as create does before verifying.
        finalize_snapshot(&valid).expect("finalize");

        verify_backup(&valid).expect("verify");
        for sidecar in sidecars(&valid) {
            assert!(!sidecar.exists(), "{} must not exist", sidecar.display());
        }
    }

    #[test]
    fn verify_refuses_missing_directory_corrupt_and_unrelated_files() {
        let dir = tempfile::tempdir().expect("tmpdir");

        let missing = dir.path().join("missing.db");
        assert_eq!(
            verify_backup(&missing).expect_err("missing").message(),
            format!("backup does not exist or is not a file: {}", missing.display())
        );
        assert!(verify_backup(dir.path())
            .expect_err("directory")
            .message()
            .contains("does not exist or is not a file"));

        let corrupt = dir.path().join("corrupt.db");
        fs::write(&corrupt, b"definitely not sqlite").expect("write");
        let err = verify_backup(&corrupt).expect_err("corrupt");
        assert!(
            err.message().starts_with("cannot open backup: "),
            "{}",
            err.message()
        );

        let unrelated = dir.path().join("unrelated.db");
        let db = Connection::open(&unrelated).expect("open");
        db.execute_batch("CREATE TABLE unrelated (id INTEGER)")
            .expect("ddl");
        drop(db);
        assert_eq!(
            verify_backup(&unrelated).expect_err("unrelated").message(),
            "not a Stackhour database (heartbeats table missing)"
        );
    }

    #[test]
    fn resolve_path_normalizes_lexically() {
        assert_eq!(resolve_path(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(resolve_path(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(resolve_path(Path::new("/a/")), PathBuf::from("/a"));
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(resolve_path(Path::new("x.db")), cwd.join("x.db"));
    }

    #[test]
    fn sidecar_names_are_suffixes_not_extensions() {
        let [wal, shm] = sidecars(Path::new("/tmp/live.db"));
        assert_eq!(wal, PathBuf::from("/tmp/live.db-wal"));
        assert_eq!(shm, PathBuf::from("/tmp/live.db-shm"));
    }

    #[test]
    fn default_destination_is_a_timestamped_backups_sibling() {
        assert_eq!(
            default_destination(Path::new("/data/stackhour.db"), 1_784_378_096_789),
            PathBuf::from("/data/backups/stackhour-2026-07-18T12-34-56-789Z.db")
        );
    }

    /// Only `--name=value`, last occurrence wins; `--force` is exact membership.
    #[test]
    fn option_parsing_quirks() {
        let args: Vec<String> = ["create", "--output=a.db", "--output=b.db", "--force"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(last_option(&args, "output"), Some("b.db".to_string()));
        assert!(has_flag(&args, "--force"));
        assert!(!has_flag(&args, "--confirm"));

        // The space-separated form is deliberately NOT recognized.
        let spaced: Vec<String> = ["create", "--output", "a.db"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(last_option(&spaced, "output"), None);
    }

    #[test]
    fn usage_string_is_pinned() {
        assert_eq!(
            USAGE,
            "usage: stackhour backup <create|verify FILE|restore FILE> [options]"
        );
    }
}
