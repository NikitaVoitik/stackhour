//! `stackhour backup restore` — split from backup.rs (line budget).
//!
//! Dry-run by default. Maintenance lock: wx + pid + fsync, path derived from
//! the db path. realpath same-file guard, restore-to-tmp, busy proof on the
//! existing DB (wal_checkpoint + BEGIN EXCLUSIVE with the /busy|locked/i
//! error rewrite), pre-restore rollback rename `.pre-restore-<ts>` (NEVER
//! deleted), swap, rollback-on-failure restoring the original DB, lock + tmp
//! cleanup in a finally-equivalent. Exact error strings; reliability tests
//! inject rename/lock failures.
//!
//! Ports `restoreBackup`, `acquireMaintenanceLock` and
//! `prepareExistingDatabase` from `src/backup.js`.

use rusqlite::Connection;
use stackhour_core::fsutil::{create_excl, fsync_dir_best_effort};
use stackhour_core::timeparse::iso_ts_for_filename;
use stackhour_core::{Error, Result};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Outcome of a restore (dry or confirmed) — feeds the exact stdout lines.
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub dry_run: bool,
    pub target: PathBuf,
    pub source: PathBuf,
    /// The `.pre-restore-<ts>` path, when a confirmed restore replaced a DB.
    pub rollback_path: Option<PathBuf>,
    pub heartbeats: i64,
}

/// The `database is busy` message is produced from two places (the checkpoint
/// result and the `/busy|locked/i` rewrite), so it lives in one constant.
const BUSY: &str = "database is busy; stop stackhour-server before restoring";

/// Append a suffix to a path without going through `to_string_lossy` (paths
/// are not required to be UTF-8; `${target}.pre-restore-…` in JS is a byte
/// concatenation).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// The maintenance-lock path derived from a db path (shared with the server's
/// startup check).
pub fn maintenance_lock_path(db: &Path) -> PathBuf {
    with_suffix(db, ".maintenance.lock")
}

/// `path.dirname()`; JS never yields `''` for the paths we handle.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// `fs.rmSync(file, { force: true })` — a missing file is not an error.
fn rm_force(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// `fsyncFile` — open for read, fsync, close. Errors propagate (unlike the
/// directory variant).
fn fsync_file(path: &Path) -> Result<()> {
    let handle = File::open(path)?;
    handle.sync_all()?;
    Ok(())
}

/// Removes `path` when dropped — the JS `finally { fs.rmSync(tmp, …) }`.
struct RmGuard(PathBuf);

impl Drop for RmGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The maintenance lock: an O_EXCL file holding our pid. Dropping it closes
/// the descriptor and deletes the file (the JS `release()` closure).
struct MaintenanceLock {
    path: PathBuf,
    _file: File,
}

impl Drop for MaintenanceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Injection seams for the reliability tests, which need a failing
/// `fs.renameSync` and a failing lock write, plus a verifier while
/// `backup::verify_backup` is being written by a sibling module.
struct Hooks {
    /// `fs.renameSync`.
    rename: fn(&Path, &Path) -> io::Result<()>,
    /// The pid write into the freshly created lock file.
    lock_write: fn(&mut File, &[u8]) -> io::Result<()>,
    /// `verifyBackup(file)` reduced to the only field this module reads.
    verify: fn(&Path) -> Result<i64>,
}

fn default_rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

fn default_lock_write(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)
}

fn default_verify(file: &Path) -> Result<i64> {
    crate::backup::verify_backup(file).map(|info| info.heartbeats)
}

impl Default for Hooks {
    fn default() -> Self {
        Hooks {
            rename: default_rename,
            lock_write: default_lock_write,
            verify: default_verify,
        }
    }
}

/// Restore `backup` over `db_path`. `confirm=false` -> dry-run (read-only).
/// `now_ms` feeds the `.pre-restore-<ts>` rollback filename.
pub fn restore_backup(db_path: &Path, backup: &Path, confirm: bool, now_ms: i64) -> Result<RestoreOutcome> {
    restore_with(db_path, backup, confirm, now_ms, &Hooks::default())
}

fn restore_with(
    db_path: &Path,
    backup: &Path,
    confirm: bool,
    now_ms: i64,
    hooks: &Hooks,
) -> Result<RestoreOutcome> {
    // `!backupPath` — an empty argument is the JS falsy case. A missing
    // positional arg is rejected by the CLI layer with the same message.
    if backup.as_os_str().is_empty() {
        return Err(Error::msg("backup file is required"));
    }
    let target = absolutize(db_path);
    let source = absolutize(backup);

    // The source is verified BEFORE anything else, so a missing or
    // non-Stackhour backup never creates maintenance artifacts.
    let heartbeats = (hooks.verify)(&source)?;

    // Symlink-aware same-file guard (createBackup compares resolved path
    // strings instead — the two directions differ deliberately).
    if target.exists() {
        let (a, b) = (fs::canonicalize(&target), fs::canonicalize(&source));
        if let (Ok(a), Ok(b)) = (a, b) {
            if a == b {
                return Err(Error::msg("backup file and target database must differ"));
            }
        }
    }

    if !confirm {
        return Ok(RestoreOutcome {
            dry_run: true,
            target,
            source,
            rollback_path: None,
            heartbeats,
        });
    }

    fs::create_dir_all(parent_of(&target))?;

    // Guard declaration order matters: Rust drops in reverse, so the tmp file
    // is removed first and the lock released second — the JS finally block's
    // order.
    let _lock = acquire_maintenance_lock(&target, hooks)?;
    let tmp = with_suffix(&target, &format!(".{}.restore.tmp", std::process::id()));
    let _tmp_guard = RmGuard(tmp.clone());

    let rollback_path = if target.exists() {
        Some(with_suffix(
            &target,
            &format!(".pre-restore-{}", iso_ts_for_filename(now_ms)),
        ))
    } else {
        None
    };

    if let Some(rollback) = &rollback_path {
        // exists() follows symlinks, matching fs.existsSync.
        if rollback.exists() || fs::symlink_metadata(rollback).is_ok() {
            return Err(Error::msg(format!(
                "rollback file already exists: {}",
                rollback.display()
            )));
        }
    }

    rm_force(&tmp)?;
    copy_excl(&source, &tmp)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fsync_file(&tmp)?;
    (hooks.verify)(&tmp)?;

    prepare_existing_database(&target)?;

    // Stale sidecars would otherwise be replayed onto the restored file.
    rm_force(&with_suffix(&target, "-wal"))?;
    rm_force(&with_suffix(&target, "-shm"))?;

    let mut old_moved = false;
    if let Some(rollback) = &rollback_path {
        (hooks.rename)(&target, rollback)?;
        old_moved = true;
    }

    match swap_in(&tmp, &target, hooks) {
        Ok(()) => Ok(RestoreOutcome {
            dry_run: false,
            target,
            source,
            rollback_path,
            heartbeats,
        }),
        Err(err) => {
            // Undo the swap: drop the half-written target, move the original
            // back. The rollback copy is only "never deleted" on success —
            // here it becomes the live database again.
            let _ = fs::remove_file(&target);
            if old_moved {
                if let Some(rollback) = &rollback_path {
                    (hooks.rename)(rollback, &target)?;
                }
            }
            Err(err)
        }
    }
}

/// rename tmp -> target, fsync the directory, re-verify the installed file.
fn swap_in(tmp: &Path, target: &Path, hooks: &Hooks) -> Result<()> {
    (hooks.rename)(tmp, target)?;
    fsync_dir_best_effort(parent_of(target));
    (hooks.verify)(target)?;
    Ok(())
}

/// `path.resolve(p)` — absolute paths pass through, relative ones are joined
/// to the cwd. Deliberately does NOT resolve symlinks (that is what the
/// canonicalize-based same-file guard is for).
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_dots(path)
    } else {
        match std::env::current_dir() {
            Ok(cwd) => normalize_dots(&cwd.join(path)),
            Err(_) => path.to_path_buf(),
        }
    }
}

/// Lexical `.`/`..` collapsing, as `path.resolve` does.
fn normalize_dots(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `fs.copyFileSync(src, dst, COPYFILE_EXCL)` — fails if `dst` exists, and
/// always produces a regular file (a symlinked source is read through).
fn copy_excl(src: &Path, dst: &Path) -> Result<()> {
    let mut input = File::open(src)?;
    let mut output = create_excl(dst, 0o600)?;
    io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}

/// `acquireMaintenanceLock` — O_EXCL create, pid, fsync. EEXIST maps to the
/// exact "maintenance already in progress" message and, crucially, leaves the
/// foreign lock file untouched; any other failure removes the lock we just
/// created so a crashed setup does not wedge the next restore.
fn acquire_maintenance_lock(target: &Path, hooks: &Hooks) -> Result<MaintenanceLock> {
    let lock_path = maintenance_lock_path(target);
    let mut file = match create_excl(&lock_path, 0o600) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Err(Error::msg(format!(
                "maintenance already in progress ({})",
                lock_path.display()
            )));
        }
        Err(err) => {
            // The JS rmSync in this branch is a no-op when the open failed,
            // but it is kept for parity with a partially created file.
            let _ = fs::remove_file(&lock_path);
            return Err(err.into());
        }
    };
    let written = (hooks.lock_write)(&mut file, std::process::id().to_string().as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(err) = written {
        drop(file);
        let _ = fs::remove_file(&lock_path);
        return Err(err.into());
    }
    Ok(MaintenanceLock {
        path: lock_path,
        _file: file,
    })
}

/// `prepareExistingDatabase` — prove no writer holds the target before we
/// move it aside: checkpoint the WAL (busy is fatal) and take an exclusive
/// transaction. busy_timeout is 1000 here, not the usual 5000.
fn prepare_existing_database(db_path: &Path) -> Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let db = match Connection::open(db_path) {
        Ok(db) => db,
        Err(err) => return Err(rewrite_busy(&err.to_string())),
    };
    match prove_idle(&db) {
        Ok(()) => Ok(()),
        Err(msg) => {
            let _ = db.execute_batch("ROLLBACK"); // there may be no transaction
            Err(rewrite_busy(&msg))
        }
    }
}

fn prove_idle(db: &Connection) -> std::result::Result<(), String> {
    db.pragma_update(None, "busy_timeout", 1000i64)
        .map_err(|e| e.to_string())?;
    // Columns: (busy, log, checkpointed). A truthy `busy` means another
    // connection blocked the checkpoint.
    let busy: i64 = db
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if busy != 0 {
        return Err(BUSY.to_string());
    }
    db.execute_batch("BEGIN EXCLUSIVE").map_err(|e| e.to_string())?;
    db.execute_batch("COMMIT").map_err(|e| e.to_string())?;
    Ok(())
}

/// `/busy|locked/i.test(err.message)` -> the canonical message; anything else
/// surfaces verbatim.
fn rewrite_busy(message: &str) -> Error {
    let lower = message.to_ascii_lowercase();
    if lower.contains("busy") || lower.contains("locked") {
        Error::msg(BUSY)
    } else {
        Error::msg(message.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::TempDir;

    // ---- test doubles -----------------------------------------------------
    //
    // `backup::verify_backup` is owned by a sibling module; these tests drive
    // this module's logic through the same seam the reliability tests use for
    // rename/lock failure injection. The stub reproduces verifyBackup's two
    // observable behaviours: the "does not exist or is not a file" and
    // "not a Stackhour database" errors, and the heartbeat count.

    fn stub_verify(file: &Path) -> Result<i64> {
        match fs::metadata(file) {
            Ok(meta) if meta.is_file() => {}
            _ => {
                return Err(Error::msg(format!(
                    "backup does not exist or is not a file: {}",
                    file.display()
                )))
            }
        }
        let db = Connection::open(file).map_err(|e| Error::msg(e.to_string()))?;
        db.query_row("SELECT count(*) FROM heartbeats", [], |r| r.get::<_, i64>(0))
            .map_err(|_| Error::msg("not a Stackhour database (heartbeats table missing)"))
    }

    fn hooks() -> Hooks {
        Hooks {
            verify: stub_verify,
            ..Hooks::default()
        }
    }

    static RENAME_FIRED: AtomicBool = AtomicBool::new(false);

    /// Fails the tmp -> target swap once, exactly like the JS test's
    /// `from === tmp && to === target` guard. The target -> rollback rename
    /// and the rollback -> target undo both go through untouched.
    fn failing_swap_rename(from: &Path, to: &Path) -> io::Result<()> {
        let is_swap = from.as_os_str().to_string_lossy().ends_with(".restore.tmp");
        if is_swap && !RENAME_FIRED.swap(true, Ordering::SeqCst) {
            return Err(io::Error::other("injected replacement failure"));
        }
        fs::rename(from, to)
    }

    fn failing_lock_write(_file: &mut File, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::other("injected lock write failure"))
    }

    // ---- fixtures ---------------------------------------------------------

    fn make_db(file: &Path, entities: &[&str]) {
        fs::create_dir_all(parent_of(file)).expect("mkdir");
        let db = Connection::open(file).expect("open");
        db.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .expect("wal");
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS heartbeats (id INTEGER PRIMARY KEY, entity TEXT NOT NULL)",
        )
        .expect("ddl");
        for entity in entities {
            db.execute("INSERT INTO heartbeats (entity) VALUES (?)", [entity])
                .expect("insert");
        }
        db.close().expect("close");
    }

    fn entities(file: &Path) -> Vec<String> {
        let db = Connection::open(file).expect("open");
        let mut stmt = db
            .prepare("SELECT entity FROM heartbeats ORDER BY id")
            .expect("prepare");
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect();
        rows
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    fn tmp_name(target: &Path) -> PathBuf {
        with_suffix(target, &format!(".{}.restore.tmp", std::process::id()))
    }

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn lock_path_is_the_db_path_plus_suffix() {
        assert_eq!(
            maintenance_lock_path(Path::new("/data/stackhour.db")),
            PathBuf::from("/data/stackhour.db.maintenance.lock")
        );
        // No extension stripping — a suffix append, not a replace.
        assert_eq!(
            maintenance_lock_path(Path::new("/x/y")),
            PathBuf::from("/x/y.maintenance.lock")
        );
    }

    #[test]
    fn rollback_name_uses_the_js_timestamp_encoding() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        let target = dir.path().join("stackhour.db");
        make_db(&source, &["new"]);
        make_db(&target, &["old"]);
        // 2026-07-18T13:14:15.016Z
        let now = 1_784_380_455_016;
        let out = restore_with(&target, &source, true, now, &hooks()).expect("restore");
        assert_eq!(
            out.rollback_path.expect("rollback"),
            with_suffix(&target, ".pre-restore-2026-07-18T13-14-15-016Z")
        );
    }

    #[test]
    fn busy_rewrite_matches_the_js_regex() {
        assert_eq!(rewrite_busy("database is locked").message(), BUSY);
        assert_eq!(rewrite_busy("SQLITE_BUSY: db BUSY").message(), BUSY);
        assert_eq!(rewrite_busy("disk I/O error").message(), "disk I/O error");
    }

    #[test]
    fn absolutize_collapses_dots_without_following_symlinks() {
        assert_eq!(
            absolutize(Path::new("/a/b/../c/./d.db")),
            PathBuf::from("/a/c/d.db")
        );
        assert!(absolutize(Path::new("rel.db")).is_absolute());
    }

    // ---- dry run ----------------------------------------------------------

    #[test]
    fn dry_run_verifies_the_source_and_writes_nothing() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        let target = dir.path().join("missing").join("stackhour.db");
        make_db(&source, &["backup"]);
        let before: Vec<_> = fs::read_dir(dir.path())
            .expect("readdir")
            .map(|e| e.expect("entry").file_name())
            .collect();

        let out = restore_with(&target, &source, false, 0, &hooks()).expect("dry run");

        assert!(out.dry_run);
        assert_eq!(out.heartbeats, 1);
        assert_eq!(out.rollback_path, None);
        assert_eq!(out.target, target);
        let after: Vec<_> = fs::read_dir(dir.path())
            .expect("readdir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(after.len(), before.len());
        assert!(!parent_of(&target).exists(), "no target dir is created");
        assert!(!maintenance_lock_path(&target).exists());
    }

    // ---- confirmed restore ------------------------------------------------

    #[test]
    fn confirmed_restore_creates_a_missing_target_at_0600_without_rollback() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        let target = dir.path().join("data").join("stackhour.db");
        make_db(&source, &["restored"]);

        let out = restore_with(&target, &source, true, 0, &hooks()).expect("restore");

        assert!(!out.dry_run);
        assert_eq!(out.rollback_path, None);
        assert_eq!(entities(&target), vec!["restored".to_string()]);
        assert_eq!(mode_of(&target), 0o600);
        assert!(!maintenance_lock_path(&target).exists());
        assert!(!tmp_name(&target).exists());
    }

    #[test]
    fn confirmed_restore_preserves_the_old_db_and_removes_stale_sidecars() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        let target = dir.path().join("stackhour.db");
        make_db(&source, &["new"]);
        make_db(&target, &["old"]);
        fs::write(with_suffix(&target, "-wal"), b"").expect("wal");
        fs::write(with_suffix(&target, "-shm"), b"").expect("shm");

        let out = restore_with(&target, &source, true, 1_784_384_055_000, &hooks()).expect("restore");
        let rollback = out.rollback_path.clone().expect("rollback");

        // Checked before reopening the WAL databases: a read may recreate them.
        assert!(!with_suffix(&target, "-wal").exists());
        assert!(!with_suffix(&target, "-shm").exists());
        assert_eq!(entities(&target), vec!["new".to_string()]);
        assert_eq!(entities(&rollback), vec!["old".to_string()]);
        assert!(!maintenance_lock_path(&target).exists());
    }

    #[test]
    fn restore_replaces_a_target_symlink_and_keeps_the_referent() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        let referent = dir.path().join("original.db");
        let target = dir.path().join("stackhour.db");
        make_db(&source, &["new"]);
        make_db(&referent, &["referent"]);
        std::os::unix::fs::symlink(&referent, &target).expect("symlink");

        let out = restore_with(&target, &source, true, 0, &hooks()).expect("restore");
        let rollback = out.rollback_path.clone().expect("rollback");

        assert!(!fs::symlink_metadata(&target)
            .expect("lstat")
            .file_type()
            .is_symlink());
        assert_eq!(entities(&target), vec!["new".to_string()]);
        assert!(fs::symlink_metadata(&rollback)
            .expect("lstat")
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::canonicalize(&rollback).expect("rollback realpath"),
            fs::canonicalize(&referent).expect("referent realpath")
        );
        assert_eq!(entities(&referent), vec!["referent".to_string()]);
    }

    // ---- guards -----------------------------------------------------------

    #[test]
    fn empty_backup_argument_is_rejected() {
        let err =
            restore_with(Path::new("/tmp/x.db"), Path::new(""), true, 0, &hooks()).expect_err("must fail");
        assert_eq!(err.message(), "backup file is required");
    }

    #[test]
    fn invalid_sources_fail_before_any_maintenance_artifact() {
        let dir = TempDir::new().expect("tmp");
        let target = dir.path().join("data").join("target.db");

        let err =
            restore_with(&target, &dir.path().join("missing.db"), true, 0, &hooks()).expect_err("missing");
        assert!(err.message().contains("does not exist"), "{}", err);

        let invalid = dir.path().join("invalid.db");
        let db = Connection::open(&invalid).expect("open");
        db.execute_batch("CREATE TABLE other (id INTEGER)").expect("ddl");
        db.close().expect("close");
        let err = restore_with(&target, &invalid, true, 0, &hooks()).expect_err("invalid");
        assert_eq!(
            err.message(),
            "not a Stackhour database (heartbeats table missing)"
        );

        assert!(!parent_of(&target).exists());
    }

    #[test]
    fn same_file_aliases_are_refused_through_symlinks() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        make_db(&source, &["backup"]);
        let alias = dir.path().join("backup-link.db");
        std::os::unix::fs::symlink(&source, &alias).expect("symlink");

        let err = restore_with(&source, &alias, true, 0, &hooks()).expect_err("must differ");
        assert_eq!(err.message(), "backup file and target database must differ");
    }

    #[test]
    fn a_foreign_maintenance_lock_blocks_and_is_left_byte_identical() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        let target = dir.path().join("target.db");
        make_db(&source, &["backup"]);
        make_db(&target, &["target"]);
        let lock = maintenance_lock_path(&target);
        fs::write(&lock, b"someone-else").expect("lock");

        let err = restore_with(&target, &source, true, 0, &hooks()).expect_err("locked");
        assert_eq!(
            err.message(),
            format!("maintenance already in progress ({})", lock.display())
        );
        assert_eq!(entities(&target), vec!["target".to_string()]);
        assert_eq!(fs::read(&lock).expect("read"), b"someone-else");
    }

    #[test]
    fn a_pre_existing_rollback_filename_aborts_and_releases_the_lock() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        let target = dir.path().join("target.db");
        make_db(&source, &["backup"]);
        make_db(&target, &["target"]);
        // 2026-07-18T14:00:00.000Z
        let now = 1_784_383_200_000;
        let rollback = with_suffix(&target, ".pre-restore-2026-07-18T14-00-00-000Z");
        fs::write(&rollback, b"reserved").expect("reserve");

        let err = restore_with(&target, &source, true, now, &hooks()).expect_err("collision");
        assert_eq!(
            err.message(),
            format!("rollback file already exists: {}", rollback.display())
        );
        assert_eq!(entities(&target), vec!["target".to_string()]);
        assert_eq!(fs::read(&rollback).expect("read"), b"reserved");
        assert!(!maintenance_lock_path(&target).exists());
        assert!(!tmp_name(&target).exists());
    }

    #[test]
    fn an_active_write_transaction_is_refused_and_artifacts_are_cleaned() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        let target = dir.path().join("target.db");
        make_db(&source, &["backup"]);
        make_db(&target, &["target"]);

        let busy = Connection::open(&target).expect("open");
        busy.execute_batch("BEGIN IMMEDIATE").expect("begin");

        let err = restore_with(&target, &source, true, 0, &hooks()).expect_err("busy");
        assert_eq!(err.message(), BUSY);

        busy.execute_batch("ROLLBACK").expect("rollback");
        drop(busy);

        assert_eq!(entities(&target), vec!["target".to_string()]);
        assert!(!maintenance_lock_path(&target).exists());
        assert!(!tmp_name(&target).exists());
    }

    // ---- failure injection ------------------------------------------------

    #[test]
    fn a_failed_swap_restores_the_original_and_cleans_every_artifact() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        let target = dir.path().join("target.db");
        make_db(&source, &["new"]);
        make_db(&target, &["original"]);
        // 2026-07-18T15:00:00.000Z
        let now = 1_784_386_800_000;

        RENAME_FIRED.store(false, Ordering::SeqCst);
        let injected = Hooks {
            rename: failing_swap_rename,
            ..hooks()
        };
        let err = restore_with(&target, &source, true, now, &injected).expect_err("injected");
        assert!(RENAME_FIRED.load(Ordering::SeqCst), "injection must fire");
        assert_eq!(err.message(), "injected replacement failure");

        assert_eq!(entities(&target), vec!["original".to_string()]);
        assert!(!with_suffix(&target, ".pre-restore-2026-07-18T15-00-00-000Z").exists());
        assert!(!tmp_name(&target).exists());
        assert!(!maintenance_lock_path(&target).exists());
    }

    #[test]
    fn a_failed_lock_write_leaves_no_stale_lock() {
        let dir = TempDir::new().expect("tmp");
        let source = dir.path().join("backup.db");
        let target = dir.path().join("target.db");
        make_db(&source, &["new"]);
        make_db(&target, &["original"]);

        let injected = Hooks {
            lock_write: failing_lock_write,
            ..hooks()
        };
        let err = restore_with(&target, &source, true, 0, &injected).expect_err("injected");
        assert_eq!(err.message(), "injected lock write failure");
        assert!(!maintenance_lock_path(&target).exists());
        assert_eq!(entities(&target), vec!["original".to_string()]);
    }
}
