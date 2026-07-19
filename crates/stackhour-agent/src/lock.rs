//! agent.lock pid protocol.
//!
//! O_EXCL create 0600 + pid text + fsync. On EEXIST: parse the pid,
//! `libc::kill(pid, 0)` where ONLY ESRCH means dead (EPERM = alive); one
//! recursive retry after removing a stale/unparsable lock. Release deletes
//! only when the stored pid is ours or unparsable (never a foreign live
//! lock); idempotent via Drop. Exact error:
//! `stackhour agent already running (pid N)`.

use stackhour_core::Result;
use std::path::{Path, PathBuf};

/// A held agent lock; released (best-effort) on Drop.
#[derive(Debug)]
pub struct AgentLock {
    path: PathBuf,
    #[allow(dead_code)] // scaffold: read only by the todo!() bodies
    pid: u32,
    released: bool,
}

/// Acquire `<data_dir>/agent.lock`.
pub fn acquire(data_dir: &Path) -> Result<AgentLock> {
    let _ = data_dir;
    todo!()
}

impl AgentLock {
    /// Explicit release (also called by Drop; idempotent).
    pub fn release(&mut self) {
        todo!()
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
