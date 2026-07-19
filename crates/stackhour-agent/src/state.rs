//! agent-state.json handled as a raw `serde_json::Value`.
//!
//! External edits between ticks take effect and unknown keys survive. Typed
//! accessor helpers cover the offsets maps, claudeUsageById (20000-entry
//! insertion-order cap), codexMeta, and the zed keys including the legacy
//! `zedDbMtime`. Load: ANY failure -> `{}`. Saved atomically (0600) once per
//! tick.

use serde_json::{Map, Value};
use stackhour_core::Result;
use std::path::Path;

/// Load agent-state.json; any failure (missing, unreadable, bad JSON) -> `{}`.
pub fn load_state(data_dir: &Path) -> Value {
    let _ = data_dir;
    todo!()
}

/// Atomic 0600 save of the whole state Value.
pub fn save_state(data_dir: &Path, state: &Value) -> Result<()> {
    let _ = (data_dir, state);
    todo!()
}

/// Typed accessors over the raw state Value. Each ensures the key exists
/// (`state[key] ??= {}` semantics) and returns the live map.
pub mod access {
    use super::*;

    /// An offsets map (`claudeOffsets`, `codexOffsets`, …) keyed by file path.
    pub fn offsets_mut<'a>(state: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
        let _ = (state, key);
        todo!()
    }

    /// `claudeUsageById` (message-id -> max token snapshot), capped at 20000
    /// entries in insertion order.
    pub fn usage_by_id_mut(state: &mut Value) -> &mut Map<String, Value> {
        let _ = state;
        todo!()
    }

    /// `codexMeta` (per-file head-line metadata).
    pub fn codex_meta_mut(state: &mut Value) -> &mut Map<String, Value> {
        let _ = state;
        todo!()
    }

    /// `zedThreads` (thread id -> last-seen signature).
    pub fn zed_threads_mut(state: &mut Value) -> &mut Map<String, Value> {
        let _ = state;
        todo!()
    }
}
