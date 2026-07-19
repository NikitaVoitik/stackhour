//! `codex` watcher: ~/.codex/sessions/**/rollout-*.jsonl.
//!
//! codexMeta head-line protocol (read_first_json_line None swallowed,
//! retried while cwd/source are still unknown); sourceFromOriginator
//! mapping; turn_context cwd/model updates; token_count math (input as-is,
//! output+reasoning summed into tokens_out, cost computed with the cached
//! split subtracted from input); changes / patch.changes -> per-file
//! is_write rows WITHOUT token fields; event_msg user_message -> human row;
//! NO branch key on rows; pruneOffsets applied to BOTH the offsets and
//! codexMeta maps.

use crate::{Gate, Watcher};
use serde_json::Value;
use stackhour_core::config::Config;
use stackhour_core::Result;

/// The Codex rollout-log watcher.
#[derive(Debug, Default)]
pub struct CodexWatcher;

impl Watcher for CodexWatcher {
    fn name(&self) -> &str {
        "codex"
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
