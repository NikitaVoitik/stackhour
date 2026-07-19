//! agent-state.json handled as a raw `serde_json::Value`.
//!
//! External edits between ticks take effect and unknown keys survive. Typed
//! accessor helpers cover the offsets maps, claudeUsageById (20000-entry
//! insertion-order cap), codexMeta, and the zed keys including the legacy
//! `zedDbMtime`. Load: ANY failure -> `{}`. Saved atomically (0600) once per
//! tick.

use serde_json::{json, Map, Value};
use stackhour_core::{Error, Result};
use std::path::{Path, PathBuf};

/// `claudeUsageById` is bounded so a long-lived agent cannot grow its state
/// file without limit; the OLDEST inserted ids are evicted first.
pub const USAGE_BY_ID_CAP: usize = 20_000;

/// `<data_dir>/agent-state.json`.
pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent-state.json")
}

/// Load agent-state.json; any failure (missing, unreadable, bad JSON) -> `{}`.
///
/// Deliberately total: state is a CACHE of watcher offsets, never a source of
/// truth. A corrupt file must degrade into a full rescan, not a crash loop.
pub fn load_state(data_dir: &Path) -> Value {
    std::fs::read_to_string(state_path(data_dir))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        // A non-object (e.g. `[]` or `3`) would break every `state.x` access.
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// Atomic 0600 save of the whole state Value.
pub fn save_state(data_dir: &Path, state: &Value) -> Result<()> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| Error::msg(format!("cannot create {}: {e}", data_dir.display())))?;
    let path = state_path(data_dir);
    let text = serde_json::to_string(state)?;
    stackhour_core::fsutil::atomic_write_0600(&path, text.as_bytes())
        .map_err(|e| Error::msg(format!("cannot write {}: {e}", path.display())))
}

/// Typed accessors over the raw state Value. Each ensures the key exists
/// (`state[key] ??= {}` semantics) and returns the live map.
pub mod access {
    use super::{Map, Value, USAGE_BY_ID_CAP};

    /// `state[key] ??= {}` then hand back the live map. A non-object value
    /// under `key` is replaced, leaving a usable object behind.
    fn object_mut<'a>(state: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
        if !state.is_object() {
            *state = Value::Object(Map::new());
        }
        let root = state.as_object_mut().expect("just coerced to an object");
        if !root.get(key).is_some_and(Value::is_object) {
            root.insert(key.to_string(), Value::Object(Map::new()));
        }
        root.get_mut(key)
            .and_then(Value::as_object_mut)
            .expect("just inserted an object")
    }

    /// An offsets map (`claudeOffsets`, `codexOffsets`, …) keyed by file path.
    pub fn offsets_mut<'a>(state: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
        object_mut(state, key)
    }

    /// `claudeUsageById` (message-id -> max token snapshot), capped at 20000
    /// entries in insertion order.
    pub fn usage_by_id_mut(state: &mut Value) -> &mut Map<String, Value> {
        object_mut(state, "claudeUsageById")
    }

    /// Evict the oldest entries until `claudeUsageById` is within the cap.
    /// Call after a batch of inserts; serde_json's preserve_order feature
    /// makes "oldest" mean "first inserted", matching JS object key order.
    pub fn trim_usage_by_id(state: &mut Value) {
        let map = usage_by_id_mut(state);
        while map.len() > USAGE_BY_ID_CAP {
            let Some(oldest) = map.keys().next().cloned() else {
                break;
            };
            map.shift_remove(&oldest);
        }
    }

    /// `codexMeta` (per-file head-line metadata).
    pub fn codex_meta_mut(state: &mut Value) -> &mut Map<String, Value> {
        object_mut(state, "codexMeta")
    }

    /// `zedThreads` (thread id -> last-seen signature).
    pub fn zed_threads_mut(state: &mut Value) -> &mut Map<String, Value> {
        object_mut(state, "zedThreads")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_missing_state_file_loads_as_an_empty_object() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(load_state(tmp.path()), json!({}));
    }

    /// State is a cache: corruption must trigger a rescan, never a crash.
    #[test]
    fn corrupt_or_non_object_state_degrades_to_empty() {
        let tmp = TempDir::new().unwrap();
        for body in ["{ truncated", "[1,2,3]", "42", "null", ""] {
            std::fs::write(state_path(tmp.path()), body).unwrap();
            assert_eq!(load_state(tmp.path()), json!({}), "body: {body:?}");
        }
    }

    /// Unknown keys written by a newer agent (or by hand) must survive a
    /// save/load cycle untouched.
    #[test]
    fn unknown_keys_round_trip() {
        let tmp = TempDir::new().unwrap();
        let state = json!({ "filesLastScan": 1.5, "somethingNew": { "a": [1, 2] } });
        save_state(tmp.path(), &state).unwrap();
        assert_eq!(load_state(tmp.path()), state);
    }

    /// The state file records file paths the user is working on.
    #[test]
    fn the_state_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        save_state(tmp.path(), &json!({})).unwrap();
        let mode = std::fs::metadata(state_path(tmp.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn save_state_creates_a_missing_data_dir() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b");
        save_state(&nested, &json!({ "x": 1 })).unwrap();
        assert_eq!(load_state(&nested), json!({ "x": 1 }));
    }

    #[test]
    fn accessors_create_missing_maps_in_place() {
        let mut state = json!({});
        access::offsets_mut(&mut state, "claudeOffsets").insert("/f".into(), json!(12));
        access::codex_meta_mut(&mut state).insert("/g".into(), json!({ "id": "x" }));
        access::zed_threads_mut(&mut state).insert("t1".into(), json!("sig"));
        assert_eq!(state["claudeOffsets"]["/f"], 12);
        assert_eq!(state["codexMeta"]["/g"]["id"], "x");
        assert_eq!(state["zedThreads"]["t1"], "sig");
    }

    /// A scalar squatting on an object key must not wedge the accessor.
    #[test]
    fn accessors_replace_a_non_object_value() {
        let mut state = json!({ "claudeOffsets": 7 });
        access::offsets_mut(&mut state, "claudeOffsets").insert("/f".into(), json!(1));
        assert_eq!(state["claudeOffsets"], json!({ "/f": 1 }));
    }

    /// The usage cache is bounded, and eviction is oldest-first.
    #[test]
    fn usage_by_id_is_capped_evicting_the_oldest_first() {
        let mut state = json!({});
        {
            let map = access::usage_by_id_mut(&mut state);
            for i in 0..(USAGE_BY_ID_CAP + 5) {
                map.insert(format!("id{i}"), json!(i));
            }
        }
        access::trim_usage_by_id(&mut state);
        let map = access::usage_by_id_mut(&mut state);
        assert_eq!(map.len(), USAGE_BY_ID_CAP);
        assert!(!map.contains_key("id0"), "the oldest id must be evicted");
        assert!(!map.contains_key("id4"));
        assert!(map.contains_key("id5"), "the 6th id is the new oldest");
        assert!(map.contains_key(&format!("id{}", USAGE_BY_ID_CAP + 4)));
    }

    #[test]
    fn trimming_an_under_cap_map_is_a_no_op() {
        let mut state = json!({ "claudeUsageById": { "a": 1 } });
        access::trim_usage_by_id(&mut state);
        assert_eq!(state["claudeUsageById"], json!({ "a": 1 }));
    }
}
