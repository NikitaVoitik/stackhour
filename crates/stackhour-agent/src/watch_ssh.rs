//! `ssh` watcher (linux-only; gate reason: `requires Linux`).
//!
//! Numeric /dev/pts entries; atime idle gate; `ps -t pts/N -o pid=,stat=`
//! preferring the foreground '+' process, else the last; /proc/<pid>/cwd
//! readlink; one 'ssh'-source human row per active pty.

use crate::{Gate, Watcher};
use serde_json::Value;
use stackhour_core::config::Config;
use stackhour_core::Result;

/// The Linux ssh/pty watcher.
#[derive(Debug, Default)]
pub struct SshWatcher;

impl Watcher for SshWatcher {
    fn name(&self) -> &str {
        "ssh"
    }

    fn gate(&self, cfg: &Config) -> Gate {
        let _ = cfg;
        todo!()
    }

    fn input_marker(&self, state: &Value) -> Option<String> {
        let _ = state;
        todo!()
    }

    fn run(&mut self, cfg: &Config, state: &mut Value, now: f64) -> Result<Vec<Value>> {
        let _ = (cfg, state, now);
        todo!()
    }
}
