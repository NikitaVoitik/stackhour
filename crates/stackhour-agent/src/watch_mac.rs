//! `macApps` watcher (darwin-only; gate reason: `requires macOS`).
//!
//! ioreg HID idle seconds via /bin/sh; ONE osascript invocation for the
//! frontmost app name + window title; cfg.agent.apps lookup (string
//! shorthand already normalized at config load); projectFromTitle
//! (configured pattern, else DEFAULT_TITLE_PATTERNS: WebStorm en/em-dash,
//! Zed em-dash). User patterns are compiled with the regex crate —
//! JS-regex-subset caveat documented: incompatible patterns log once and are
//! treated as no-match, never crash. Emits a single 'human' app row.

use crate::{Gate, Watcher};
use serde_json::Value;
use stackhour_core::config::Config;
use stackhour_core::Result;

/// Subprocess-command injection seam so the logic is unit-testable on Linux:
/// (program, args) -> stdout.
pub type CmdRunner = fn(program: &str, args: &[&str]) -> std::io::Result<String>;

/// The macOS frontmost-app watcher.
#[derive(Debug, Default)]
pub struct MacWatcher {
    /// None = real subprocess execution.
    pub runner: Option<CmdRunner>,
}

impl Watcher for MacWatcher {
    fn name(&self) -> &str {
        "macApps"
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
