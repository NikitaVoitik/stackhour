//! Bridge state.json: getUpdates offset, active target/engine, sessions.
//!
//! Legacy 'gcp'/'mac' session keys are migrated to '<target>:claude' on load
//! with the legacy keys RETAINED (a downgrade to the JS coordinator still
//! finds its sessions). NEW optional additive 'agent' field (ignored by the
//! JS coordinator). Session keys extend to '<target>:<engine>@<agent>' when
//! an agent is active. The whole file is persisted after every update.
//!
//! Port of the state half of `src/bridge/coordinator.mjs`.

use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::path::Path;

/// `s.active ||= CONFIG.defaultTarget` — the coordinator's shipped default.
pub const DEFAULT_TARGET: &str = "gcp";
/// `s.engine ||= 'claude'`.
pub const DEFAULT_ENGINE: &str = "claude";
/// The two targets whose pre-engine session keys need migrating.
const LEGACY_TARGETS: [&str; 2] = ["gcp", "mac"];

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

impl Default for BridgeState {
    fn default() -> Self {
        BridgeState {
            offset: 0,
            active: DEFAULT_TARGET.to_string(),
            engine: DEFAULT_ENGINE.to_string(),
            agent: None,
            sessions: IndexMap::new(),
            raw: json!({}),
        }
    }
}

/// Load state.json from the runtime dir (missing/corrupt -> defaults),
/// applying the legacy-key migration.
///
/// Total by design, exactly like the JS `try { … } catch { return defaults }`:
/// state.json is a resumption CACHE (a poll offset and some session ids), not
/// a source of truth. A truncated write must cost at most a replayed update,
/// never a coordinator that will not start.
pub fn load(dir: &Path) -> BridgeState {
    load_with_defaults(dir, DEFAULT_TARGET, DEFAULT_ENGINE)
}

/// [`load`], with the seed target/engine supplied by the caller.
///
/// The JS reads `CONFIG.defaultTarget` here (`s.active ||= CONFIG.defaultTarget`)
/// and hardcodes `'claude'`. The coordinator passes `CoordinatorCfg.default_target`
/// and the registry's default engine, so changing either in config actually
/// changes what a fresh state.json starts as — which the hardcoded constant
/// silently would not.
pub fn load_with_defaults(dir: &Path, default_target: &str, default_engine: &str) -> BridgeState {
    let raw = std::fs::read_to_string(dir.join("state.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));

    let mut sessions: IndexMap<String, Option<String>> = IndexMap::new();
    if let Some(map) = raw.get("sessions").and_then(Value::as_object) {
        for (key, value) in map {
            sessions.insert(key.clone(), value.as_str().map(str::to_string));
        }
    }
    // Sessions created before engine selection existed were keyed by bare
    // target. Copy them onto the '<target>:claude' key and KEEP the original,
    // so downgrading to the JS coordinator still finds its session.
    for target in LEGACY_TARGETS {
        let Some(Some(legacy)) = sessions.get(target).cloned() else {
            continue;
        };
        let migrated = format!("{target}:{DEFAULT_ENGINE}");
        if !sessions.contains_key(&migrated) {
            sessions.insert(migrated, Some(legacy));
        }
    }

    BridgeState {
        // `s.offset ||= 0` — a non-numeric or absent offset restarts the poll
        // from the beginning rather than crashing.
        offset: raw.get("offset").and_then(Value::as_i64).unwrap_or(0),
        active: nonempty(raw.get("active")).unwrap_or_else(|| default_target.to_string()),
        engine: nonempty(raw.get("engine")).unwrap_or_else(|| default_engine.to_string()),
        agent: nonempty(raw.get("agent")),
        sessions,
        raw,
    }
}

/// JS `x ||= default`: a missing key, a null, and an empty string are all
/// "unset".
fn nonempty(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl BridgeState {
    /// Persist the whole file.
    ///
    /// The typed fields are written OVER the preserved raw object rather than
    /// replacing it, so a key some other version of the bridge wrote survives
    /// a round trip through this one.
    ///
    /// Failures are swallowed and reported by the caller's log, matching
    /// `catch (e) { log('saveState err', e.message) }`: losing the poll offset
    /// is recoverable, dying mid-conversation is not.
    pub fn save(&self, dir: &Path) {
        let _ = self.try_save(dir);
    }

    /// [`save`](Self::save), reporting the failure to `log` as the JS does:
    /// `saveState err <message>`. The coordinator uses this so a full disk or
    /// a read-only runtime dir is visible in coordinator.log instead of
    /// silently losing every session id.
    pub fn save_logged(&self, dir: &Path, log: &dyn Fn(&str)) {
        if let Err(e) = self.try_save(dir) {
            log(&format!("saveState err {e}"));
        }
    }

    /// The fallible form. Still never panics; the caller decides whether a
    /// failed write is worth a log line.
    pub fn try_save(&self, dir: &Path) -> std::io::Result<()> {
        let mut out: Map<String, Value> = self.raw.as_object().cloned().unwrap_or_default();
        out.insert("offset".into(), json!(self.offset));
        out.insert("active".into(), json!(self.active));
        out.insert("engine".into(), json!(self.engine));
        match &self.agent {
            // `agent` is additive and unknown to the JS coordinator; omit it
            // entirely when unset rather than writing a null it would have to
            // understand.
            Some(agent) => {
                out.insert("agent".into(), json!(agent));
            }
            None => {
                out.shift_remove("agent");
            }
        }
        let sessions: Map<String, Value> = self
            .sessions
            .iter()
            .map(|(k, v)| (k.clone(), v.as_ref().map_or(Value::Null, |s| json!(s))))
            .collect();
        out.insert("sessions".into(), Value::Object(sessions));

        // Pretty-printed with 2 spaces, matching `JSON.stringify(state, null, 2)`
        // — the file is routinely hand-inspected during support.
        let text = serde_json::to_string_pretty(&Value::Object(out))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("state.json"), text)
    }

    /// '<target>:<engine>' — or '<target>:<engine>@<agent>' when an agent is
    /// active (additive; invisible to the JS coordinator).
    pub fn session_key(target: &str, engine: &str, agent: Option<&str>) -> String {
        match agent.filter(|a| !a.is_empty()) {
            Some(agent) => format!("{target}:{engine}@{agent}"),
            None => format!("{target}:{engine}"),
        }
    }

    /// The session id for the current target/engine/agent, if any.
    pub fn session(&self) -> Option<&str> {
        let key = Self::session_key(&self.active, &self.engine, self.agent.as_deref());
        self.sessions.get(&key).and_then(Option::as_deref)
    }

    /// Set (or clear, with `None`) the session id for the current
    /// target/engine/agent.
    ///
    /// Does NOT persist. The JS `setSession` writes state.json on every call;
    /// callers here must follow with [`save`](Self::save) or
    /// [`save_logged`](Self::save_logged) or a restart loses the session.
    pub fn set_session(&mut self, value: Option<String>) {
        let key = Self::session_key(&self.active, &self.engine, self.agent.as_deref());
        self.sessions.insert(key, value);
    }

    /// The session id for an EXPLICIT target/engine/agent, ignoring what is
    /// currently active.
    ///
    /// Required by the mac lane: a result returning while the user has
    /// switched to /gcp must still be filed under `mac:<engine>`. Addressing
    /// it through [`session`](Self::session) would write the returning id onto
    /// the gcp key and corrupt both sessions.
    pub fn session_for(&self, target: &str, engine: &str, agent: Option<&str>) -> Option<&str> {
        self.sessions
            .get(&Self::session_key(target, engine, agent))
            .and_then(Option::as_deref)
    }

    /// [`set_session`](Self::set_session) for an explicit target/engine/agent.
    pub fn set_session_for(
        &mut self,
        target: &str,
        engine: &str,
        agent: Option<&str>,
        value: Option<String>,
    ) {
        self.sessions
            .insert(Self::session_key(target, engine, agent), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &TempDir, body: &str) {
        std::fs::write(dir.path().join("state.json"), body).unwrap();
    }

    #[test]
    fn a_missing_or_corrupt_state_file_loads_defaults() {
        let tmp = TempDir::new().unwrap();
        for body in ["{ truncated", "[1,2]", "null", "", "42"] {
            write(&tmp, body);
            let s = load(tmp.path());
            assert_eq!(s.offset, 0, "body: {body:?}");
            assert_eq!(s.active, "gcp");
            assert_eq!(s.engine, "claude");
            assert_eq!(s.agent, None);
            assert!(s.sessions.is_empty());
        }
        std::fs::remove_file(tmp.path().join("state.json")).unwrap();
        assert_eq!(load(tmp.path()).offset, 0);
    }

    /// Sessions predating engine selection must keep working after an upgrade,
    /// AND keep working if the user downgrades again — hence the copy rather
    /// than a rename.
    #[test]
    fn legacy_session_keys_are_migrated_but_retained() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, r#"{"sessions": {"gcp": "sess-old", "mac": "mac-old"}}"#);
        let s = load(tmp.path());
        assert_eq!(s.sessions["gcp:claude"], Some("sess-old".into()));
        assert_eq!(s.sessions["mac:claude"], Some("mac-old".into()));
        assert_eq!(
            s.sessions["gcp"],
            Some("sess-old".into()),
            "the legacy key must survive for a downgrade"
        );
    }

    /// An already-migrated key wins: re-running the migration must not
    /// resurrect a stale pre-upgrade session over a newer one.
    #[test]
    fn migration_never_overwrites_an_existing_engine_key() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, r#"{"sessions": {"gcp": "stale", "gcp:claude": "current"}}"#);
        assert_eq!(load(tmp.path()).sessions["gcp:claude"], Some("current".into()));
    }

    #[test]
    fn session_keys_extend_with_an_agent() {
        assert_eq!(BridgeState::session_key("gcp", "claude", None), "gcp:claude");
        assert_eq!(
            BridgeState::session_key("mac", "codex", Some("oracle")),
            "mac:codex@oracle"
        );
        // An empty agent name is no agent, not an `@` suffix.
        assert_eq!(BridgeState::session_key("gcp", "claude", Some("")), "gcp:claude");
    }

    /// Switching engine or agent must not leak the other's session id — that
    /// is what the compound key exists for.
    #[test]
    fn sessions_are_scoped_per_target_engine_and_agent() {
        let mut s = BridgeState::default();
        s.set_session(Some("a".into()));
        s.engine = "codex".into();
        assert_eq!(s.session(), None, "codex inherited claude's session");
        s.set_session(Some("b".into()));
        s.agent = Some("oracle".into());
        assert_eq!(s.session(), None, "the agent inherited the bare session");
        s.set_session(Some("c".into()));

        s.engine = "claude".into();
        s.agent = None;
        assert_eq!(s.session(), Some("a"));
    }

    /// Unknown keys must survive a load/save round trip: another version of
    /// the bridge may be writing them.
    #[test]
    fn save_preserves_unknown_keys_and_round_trips() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            r#"{"offset": 7, "active": "mac", "engine": "codex",
                "somethingElse": {"kept": true}, "sessions": {"mac:codex": "s1"}}"#,
        );
        let mut s = load(tmp.path());
        assert_eq!(
            (s.offset, s.active.as_str(), s.engine.as_str()),
            (7, "mac", "codex")
        );
        s.offset = 9;
        s.save(tmp.path());

        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join("state.json")).unwrap()).unwrap();
        assert_eq!(raw["somethingElse"]["kept"], true, "an unknown key was dropped");
        assert_eq!(raw["offset"], 9);
        assert_eq!(raw["sessions"]["mac:codex"], "s1");
        assert!(
            raw.get("agent").is_none(),
            "an unset agent must not be written as null for the JS coordinator"
        );

        let again = load(tmp.path());
        assert_eq!(again.offset, 9);
        assert_eq!(again.sessions["mac:codex"], Some("s1".into()));
    }

    /// A cleared session is a JSON null, distinct from an absent key.
    #[test]
    fn a_cleared_session_round_trips_as_null() {
        let tmp = TempDir::new().unwrap();
        let mut s = BridgeState::default();
        s.set_session(Some("x".into()));
        s.set_session(None);
        s.save(tmp.path());
        let reloaded = load(tmp.path());
        assert_eq!(reloaded.sessions.get("gcp:claude"), Some(&None));
        assert_eq!(reloaded.session(), None);
    }

    /// `s.active ||= CONFIG.defaultTarget` — the seed comes from config, so a
    /// user who sets `defaultTarget: "mac"` gets a mac-first fresh state.
    #[test]
    fn a_fresh_state_seeds_from_the_supplied_defaults() {
        let tmp = TempDir::new().unwrap();
        let s = load_with_defaults(tmp.path(), "mac", "codex");
        assert_eq!((s.active.as_str(), s.engine.as_str()), ("mac", "codex"));
        // An existing file still wins over the defaults.
        write(&tmp, r#"{"active": "gcp", "engine": "claude"}"#);
        let s = load_with_defaults(tmp.path(), "mac", "codex");
        assert_eq!((s.active.as_str(), s.engine.as_str()), ("gcp", "claude"));
    }

    /// pollResults writes `setSession('mac', engine, id)` with an EXPLICIT
    /// target. If it went through the active-target setter, a user who
    /// switched to /gcp mid-flight would have the Mac's session written onto
    /// the gcp key.
    #[test]
    fn explicit_session_addressing_ignores_the_active_target() {
        let mut s = BridgeState::default(); // active = gcp
        s.set_session(Some("gcp-session".into()));
        s.set_session_for("mac", "codex", None, Some("mac-session".into()));

        assert_eq!(s.session(), Some("gcp-session"), "the gcp key was clobbered");
        assert_eq!(s.session_for("mac", "codex", None), Some("mac-session"));
        assert_eq!(s.session_for("mac", "claude", None), None);
        assert_eq!(s.sessions.len(), 2);
    }

    /// A failed write must be reportable, not silent: losing every session id
    /// with no log line is the hard-to-notice regression.
    #[test]
    fn a_failed_save_is_reported_to_the_log() {
        let tmp = TempDir::new().unwrap();
        // A FILE where the runtime dir should be: create_dir_all fails.
        let blocked = tmp.path().join("not-a-dir");
        std::fs::write(&blocked, "x").unwrap();

        let mut lines: Vec<String> = Vec::new();
        {
            let sink = std::cell::RefCell::new(&mut lines);
            BridgeState::default().save_logged(&blocked, &|l: &str| sink.borrow_mut().push(l.to_string()));
        }
        assert_eq!(lines.len(), 1, "expected one log line, got {lines:?}");
        assert!(
            lines[0].starts_with("saveState err "),
            "wrong prefix: {}",
            lines[0]
        );
        // ...and the happy path stays quiet.
        let mut ok: Vec<String> = Vec::new();
        {
            let sink = std::cell::RefCell::new(&mut ok);
            BridgeState::default().save_logged(tmp.path(), &|l: &str| sink.borrow_mut().push(l.to_string()));
        }
        assert!(ok.is_empty(), "a successful save logged: {ok:?}");
    }

    #[test]
    fn an_active_agent_is_persisted() {
        let tmp = TempDir::new().unwrap();
        let mut s = BridgeState {
            agent: Some("oracle".into()),
            ..BridgeState::default()
        };
        s.save(tmp.path());
        assert_eq!(load(tmp.path()).agent.as_deref(), Some("oracle"));

        s.agent = None;
        s.save(tmp.path());
        assert_eq!(load(tmp.path()).agent, None);
    }
}
