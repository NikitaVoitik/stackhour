//! `claude` watcher: ~/.claude/projects/**/*.jsonl (depth-4 walk).
//!
//! Cheap size<=offset skip WITHOUT the truncation clamp (quirk kept — this
//! path differs from tail.rs's generic behaviour); 3600s past window with no
//! future bound; entrypoint -> source mapping; isHumanPrompt rules
//! (tool_result content excludes, isSidechain excludes); per-message-id
//! max-based token dedup with >=0-floored deltas and the 20000-entry
//! insertion-order cap; tokenFields attached to the FIRST tool_use file row
//! only, else the fallback app row; /edit|write/i tool-name -> is_write;
//! pruneOffsets.

use crate::{Gate, Watcher};
use serde_json::Value;
use stackhour_core::config::Config;
use stackhour_core::Result;

/// The Claude Code session-log watcher.
#[derive(Debug, Default)]
pub struct ClaudeWatcher;

impl Watcher for ClaudeWatcher {
    fn name(&self) -> &str {
        "claude"
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
