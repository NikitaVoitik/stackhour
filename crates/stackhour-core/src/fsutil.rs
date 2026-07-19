//! The atomic-write ritual used by every secret-bearing file.
//!
//! wx tmp `<file>.<pid>.tmp` (O_EXCL, mode 0600) -> write -> fsync file ->
//! rename over target -> chmod final -> best-effort parent-dir fsync; the tmp
//! file is removed in a Drop guard (JS `finally` equivalent). Plus
//! append+fsync+chmod for queue.jsonl, a 0644 variant for service units, and
//! O_EXCL creation for lock files.
//!
//! Ports: `writeConfig` in src/setup.js (mode 0600 + best-effort dir fsync),
//! `atomicWrite` in src/install.js (mode 0644) and src/agent/index.js
//! (mode 0600 + dir fsync), `appendQueue` in src/agent/index.js, and the
//! `fs.openSync(lockPath, 'wx', 0o600)` lock ritual.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Removes the tmp file when dropped, mirroring the JS
/// `finally { fs.rmSync(tmp, { force: true }); }`.
struct TmpGuard(PathBuf);

impl Drop for TmpGuard {
    fn drop(&mut self) {
        // force: true — a missing file (the happy path, after rename) is fine.
        let _ = fs::remove_file(&self.0);
    }
}

fn parent_of(path: &Path) -> &Path {
    // JS path.dirname() never yields '' for the paths we handle; fall back to
    // '.' for bare relative filenames.
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Atomic write with an explicit final mode.
///
/// mkdir -p parent; open `<path>.<pid>.tmp` with O_EXCL and the target mode;
/// write; fsync; close; rename over `path`; chmod `path` unconditionally
/// (even when it pre-existed with looser perms — and to defeat umask masking
/// of the open(2) mode); best-effort fsync of the parent dir. The tmp file is
/// removed on every exit path.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = parent_of(path);
    fs::create_dir_all(dir)?;

    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);

    // rmSync(tmp, { force: true }) before the O_EXCL open: a leftover tmp
    // from a previous crashed run with the same pid must not wedge us.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let _guard = TmpGuard(tmp.clone());

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true) // 'wx' — O_CREAT | O_EXCL
            .mode(mode)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    } // closed here, before rename (parity with the JS finally-close)

    fs::rename(&tmp, path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    fsync_dir_best_effort(dir);
    Ok(())
}

/// Atomic write, final mode 0600 (config.json, state files, exports).
pub fn atomic_write_0600(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_write(path, bytes, 0o600)
}

/// Atomic write, final mode 0644 (systemd units / launchd plists).
pub fn atomic_write_0644(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_write(path, bytes, 0o644)
}

/// Append + fsync + chmod 0600 (queue.jsonl). Creates the file when missing.
///
/// Ports `appendQueue`: mkdir -p parent; open 'a' mode 0600; write; fsync;
/// close; chmod 0600 (defeats umask on first creation, and re-tightens a
/// queue file that somehow gained looser perms).
pub fn append_fsync_0600(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(parent_of(path))?;
    {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// O_EXCL create with the given mode (agent lock, maintenance lock).
/// Fails with `AlreadyExists` when the file is present.
///
/// Ports `fs.openSync(lockPath, 'wx', mode)`. The parent directory is
/// created first, matching the JS callers which always mkdir before opening.
pub fn create_excl(path: &Path, mode: u32) -> io::Result<File> {
    fs::create_dir_all(parent_of(path))?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
}

/// Best-effort fsync of a directory; errors are swallowed (parity with the JS
/// try/catch around dir handles — "directory fsync is unavailable on some
/// platforms").
pub fn fsync_dir_best_effort(dir: &Path) {
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn atomic_write_creates_file_with_mode_and_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        atomic_write_0600(&target, b"{\"a\":1}\n").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"{\"a\":1}\n");
        assert_eq!(mode_of(&target), 0o600);
    }

    #[test]
    fn atomic_write_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a/b/c/unit.service");
        atomic_write_0644(&target, b"[Unit]\n").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"[Unit]\n");
        assert_eq!(mode_of(&target), 0o644);
    }

    #[test]
    fn atomic_write_overwrites_and_tightens_perms() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o666)).unwrap();
        atomic_write_0600(&target, b"new").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        // chmod happens unconditionally, even over a pre-existing looser file.
        assert_eq!(mode_of(&target), 0o600);
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        atomic_write_0600(&target, b"x").unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("config.json")]);
    }

    #[test]
    fn atomic_write_removes_stale_tmp_from_same_pid() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        let stale = dir.path().join(format!("config.json.{}.tmp", std::process::id()));
        fs::write(&stale, b"stale").unwrap();
        // Must not fail on O_EXCL against the leftover tmp.
        atomic_write_0600(&target, b"fresh").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"fresh");
        assert!(!stale.exists());
    }

    #[test]
    fn atomic_write_cleans_tmp_when_rename_fails() {
        let dir = tempfile::tempdir().unwrap();
        // Target is a non-empty directory -> rename(tmp, target) fails.
        let target = dir.path().join("occupied");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("child"), b"x").unwrap();
        atomic_write_0600(&target, b"x").unwrap_err();
        let tmp = dir.path().join(format!("occupied.{}.tmp", std::process::id()));
        assert!(!tmp.exists(), "tmp must be removed by the Drop guard");
    }

    #[test]
    fn append_creates_with_0600_then_appends() {
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("sub/queue.jsonl");
        append_fsync_0600(&queue, b"{\"a\":1}\n").unwrap();
        append_fsync_0600(&queue, b"{\"b\":2}\n").unwrap();
        assert_eq!(fs::read(&queue).unwrap(), b"{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(mode_of(&queue), 0o600);
    }

    #[test]
    fn append_retightens_loose_perms() {
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("queue.jsonl");
        fs::write(&queue, b"x\n").unwrap();
        fs::set_permissions(&queue, fs::Permissions::from_mode(0o644)).unwrap();
        append_fsync_0600(&queue, b"y\n").unwrap();
        assert_eq!(mode_of(&queue), 0o600);
    }

    #[test]
    fn create_excl_succeeds_once_then_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("agent.lock");
        let mut file = create_excl(&lock, 0o600).unwrap();
        file.write_all(b"123").unwrap();
        assert_eq!(mode_of(&lock), 0o600);
        let err = create_excl(&lock, 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn fsync_dir_best_effort_swallows_errors() {
        // Missing directory: must not panic or error.
        fsync_dir_best_effort(Path::new("/nonexistent/definitely/not/here"));
        let dir = tempfile::tempdir().unwrap();
        fsync_dir_best_effort(dir.path());
    }
}
