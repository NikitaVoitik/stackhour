//! Bridge state.json: getUpdates offset, active target/engine, sessions.
//!
//! Legacy 'gcp'/'mac' session keys are migrated to '<target>:claude' on load
//! with the legacy keys RETAINED (a downgrade to the JS coordinator still
//! finds its sessions). NEW optional additive 'agent' field (ignored by the
//! JS coordinator). Session keys extend to '<target>:<engine>@<agent>' when
//! an agent is active. The whole file is persisted after every update.

use indexmap::IndexMap;
use serde_json::Value;
use std::path::Path;

/// state.json, typed view + raw preservation.
#[derive(Debug, Clone)]
pub struct BridgeState {
    /// getUpdates offset.
    pub offset: i64,
    /// Active target ('gcp' | 'mac').
    pub active: String,
    /// Active engine ('claude' | 'codex' | registry engine name).
    pub engine: String,
    /// NEW: active named agent, when any.
    pub agent: Option<String>,
    /// Session-key -> session id (None = cleared).
    pub sessions: IndexMap<String, Option<String>>,
    /// Raw file contents; unknown keys survive.
    pub raw: Value,
}

/// Load state.json from the runtime dir (missing/corrupt -> defaults),
/// applying the legacy-key migration.
pub fn load(dir: &Path) -> BridgeState {
    let _ = dir;
    todo!()
}

impl BridgeState {
    /// Persist the whole file.
    pub fn save(&self, dir: &Path) {
        let _ = dir;
        todo!()
    }

    /// '<target>:<engine>' — or '<target>:<engine>@<agent>' when an agent is
    /// active (additive; invisible to the JS coordinator).
    pub fn session_key(target: &str, engine: &str, agent: Option<&str>) -> String {
        let _ = (target, engine, agent);
        todo!()
    }
}
