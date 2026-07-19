//! agent.lock pid protocol.
//!
//! O_EXCL create 0600 + pid text + fsync. On EEXIST: parse the pid,
//! `libc::kill(pid, 0)` where ONLY ESRCH means dead (EPERM = alive); one
//! recursive retry after removing a stale/unparsable lock. Release deletes
//! only when the stored pid is ours or unparsable (never a foreign live
//! lock); idempotent via Drop. Exact error:
//! `stackhour agent already running (pid N)`.

use stackhour_core::{Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// `<data_dir>/agent.lock`.
pub fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent.lock")
}

/// A held agent lock; released (best-effort) on Drop.
#[derive(Debug)]
pub struct AgentLock {
    path: PathBuf,
    pid: u32,
    released: bool,
}

/// Is `pid` a live process?
///
/// `kill(pid, 0)` probes without signalling. ONLY `ESRCH` (no such process)
/// proves the owner is gone — `EPERM` means it exists but belongs to another
/// user, which must be treated as ALIVE. Getting this backwards would let two
/// agents run at once and double-count every heartbeat.
fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The pid recorded in an existing lock file, if it parses.
fn stored_pid(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    // `Number.parseInt` semantics: leading digits win, trailing junk ignored.
    let digits: String = text.trim_start().chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u32>().ok().filter(|p| *p > 0)
}

/// Acquire `<data_dir>/agent.lock`.
pub fn acquire(data_dir: &Path) -> Result<AgentLock> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| Error::msg(format!("cannot create {}: {e}", data_dir.display())))?;
    let path = lock_path(data_dir);
    let pid = std::process::id();
    try_acquire(&path, pid, true)
}

fn try_acquire(path: &Path, pid: u32, may_retry: bool) -> Result<AgentLock> {
    match stackhour_core::fsutil::create_excl(path, 0o600) {
        Ok(mut file) => {
            let write = file
                .write_all(pid.to_string().as_bytes())
                .and_then(|()| file.sync_all());
            if let Err(e) = write {
                // Never leave a lock file we could not stamp with our pid —
                // it would look like a live foreign lock forever.
                let _ = std::fs::remove_file(path);
                return Err(Error::msg(format!(
                    "cannot write {}: {e}",
                    path.display()
                )));
            }
            Ok(AgentLock {
                path: path.to_path_buf(),
                pid,
                released: false,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Some(owner) = stored_pid(path) {
                if pid_is_alive(owner) {
                    return Err(Error::msg(format!(
                        "stackhour agent already running (pid {owner})"
                    )));
                }
            }
            // Stale or unparsable: clear it and take the lock. Exactly ONE
            // retry, so a pathological race cannot spin forever.
            if !may_retry {
                return Err(Error::msg(format!(
                    "cannot acquire {}: lock is contended",
                    path.display()
                )));
            }
            let _ = std::fs::remove_file(path);
            try_acquire(path, pid, false)
        }
        Err(e) => Err(Error::msg(format!(
            "cannot create {}: {e}",
            path.display()
        ))),
    }
}

impl AgentLock {
    /// Explicit release (also called by Drop; idempotent).
    ///
    /// Only removes the file when it still names US (or is unreadable /
    /// unparsable). A lock re-taken by another agent after ours went stale
    /// must be left alone.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        match stored_pid(&self.path) {
            Some(owner) if owner != self.pid => {}
            _ => {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for AgentLock {
    fn drop(&mut self) {
        if !self.released {
            self.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquiring_writes_our_pid_at_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let lock = acquire(tmp.path()).unwrap();
        let path = lock_path(tmp.path());
        assert!(path.exists());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(lock);
    }

    /// The whole point of the lock: a second agent must refuse to start.
    #[test]
    fn a_live_foreign_lock_blocks_acquisition() {
        let tmp = TempDir::new().unwrap();
        // pid 1 always exists and is not us.
        std::fs::write(lock_path(tmp.path()), "1").unwrap();
        let err = acquire(tmp.path()).unwrap_err();
        assert_eq!(
            err.message(),
            "stackhour agent already running (pid 1)"
        );
        assert!(
            lock_path(tmp.path()).exists(),
            "a live foreign lock must not be deleted"
        );
    }

    /// A lock left behind by a crashed agent must not wedge the next start.
    #[test]
    fn a_stale_lock_is_reclaimed() {
        let tmp = TempDir::new().unwrap();
        // A pid that cannot be running: max pid + a wide margin.
        std::fs::write(lock_path(tmp.path()), "4194303").unwrap();
        let lock = acquire(tmp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(lock_path(tmp.path())).unwrap(),
            std::process::id().to_string()
        );
        drop(lock);
    }

    /// A truncated or garbage lock file is treated as stale, not fatal.
    #[test]
    fn an_unparsable_lock_is_reclaimed() {
        let tmp = TempDir::new().unwrap();
        for body in ["", "   ", "not-a-pid", "0"] {
            std::fs::write(lock_path(tmp.path()), body).unwrap();
            let lock = acquire(tmp.path()).expect(body);
            drop(lock);
            assert!(!lock_path(tmp.path()).exists());
        }
    }

    #[test]
    fn releasing_removes_the_file_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let mut lock = acquire(tmp.path()).unwrap();
        lock.release();
        assert!(!lock_path(tmp.path()).exists());
        lock.release();
        drop(lock);
        assert!(!lock_path(tmp.path()).exists());
    }

    /// Release must NEVER delete a lock that now belongs to someone else —
    /// otherwise a slow shutdown would unlock a freshly started agent.
    #[test]
    fn releasing_leaves_a_lock_that_was_retaken_by_another_pid() {
        let tmp = TempDir::new().unwrap();
        let mut lock = acquire(tmp.path()).unwrap();
        std::fs::write(lock_path(tmp.path()), "1").unwrap();
        lock.release();
        assert_eq!(
            std::fs::read_to_string(lock_path(tmp.path())).unwrap(),
            "1",
            "another agent's lock was stolen"
        );
    }

    /// Dropping without an explicit release still frees the lock, so the
    /// next `agent --once` can start.
    #[test]
    fn dropping_releases_and_allows_reacquisition() {
        let tmp = TempDir::new().unwrap();
        {
            let _lock = acquire(tmp.path()).unwrap();
            assert!(acquire(tmp.path()).is_err(), "reentrant acquire must fail");
        }
        let _lock = acquire(tmp.path()).unwrap();
    }

    /// Our own pid counts as live, so a second acquire in-process is refused
    /// with the exact message doctor and the CLI surface.
    #[test]
    fn reacquiring_reports_our_own_pid() {
        let tmp = TempDir::new().unwrap();
        let _lock = acquire(tmp.path()).unwrap();
        assert_eq!(
            acquire(tmp.path()).unwrap_err().message(),
            format!("stackhour agent already running (pid {})", std::process::id())
        );
    }

    #[test]
    fn pid_zero_is_never_alive() {
        assert!(!pid_is_alive(0));
        assert!(pid_is_alive(std::process::id()));
    }
}
