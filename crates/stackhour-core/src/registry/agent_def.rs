//! Named agents: engine + model + effort + tool policy + permission mode +
//! optional cwd + prompt template + a SOUL DOCUMENT, optionally composed from
//! a base agent and a stack of overlays.
//!
//! A soul is plain markdown the user edits by hand. It is loaded lazily with
//! an mtime cache: every use re-stats and re-reads when changed, so editing
//! `soul.md` takes effect on the next prompt without a daemon restart. The
//! same discipline applies to every overlay.
//!
//! `agents/<name>/agent.toml` schema (every key except `engine` optional, and
//! even `engine` is optional when `extends` supplies one):
//!
//! ```toml
//! label = "Reviewer"            # display label; defaults to the dir name
//! engine = "claude"             # required unless inherited; cross-ref checked
//! model = "claude-opus-4"       # optional model override
//! effort = "high"               # "low" | "medium" | "high"; engines without
//!                               # effort_args ignore it
//! soul = "soul.md"              # soul document, relative to the agent dir
//! overlays = ["terse.md"]       # appended after the soul, in order
//! extends = "base"              # inherit another agent's fields and soul
//! skills = ["review"]           # skill ids; cross-ref validated at load
//! permission_mode = "default"   # "default" | "bypassPermissions"
//! cwd = "~/work/repo"           # optional working-dir override
//! prompt_template = "review"    # a PROMPT TEMPLATE NAME (cross-ref checked)
//!
//! [tools]
//! allow = ["Bash", "Edit"]
//! deny = ["WebSearch"]
//! ```
//!
//! Unknown keys are ignored (forward compatibility). Validation errors are
//! [`FieldError`]s naming the key; the loader attaches the file path and
//! collects them into `Registry::errors` — a broken agent.toml is skipped,
//! never fatal.
//!
//! ## Inheritance (`extends`)
//!
//! [`resolve_inheritance`] is the loader's entry point. It resolves SCALARS
//! only — engine, model, effort, permission_mode, cwd, prompt_template — plus
//! `skills` (parent-first union) and `[tools]` (union, with deny beating
//! allow). It deliberately LEAVES `extends` in place afterwards, because soul
//! composition walks the same chain lazily in [`composed_soul`]: souls are
//! re-read from disk on every use, so they cannot be flattened at load time
//! without losing hot reload.
//!
//! `label` is NOT inherited: it defaults to the agent's own directory name, so
//! a child of `reviewer` is not silently also called "Reviewer".

use indexmap::{IndexMap, IndexSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use super::cycle::MAX_DEPTH;
use super::error::FieldError;
use super::toml_util::{opt_enum, opt_nonempty_string, opt_table, root_table, string_list};
use super::{Registry, RegistryEntityKind, RegistryError};

/// Engine-specific tool allow/deny policy strings, passed through the
/// engine's declared flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl ToolPolicy {
    /// Merge `parent` UNDER `self` (inheritance): both lists are unioned,
    /// parent entries first, and `deny` wins — anything denied by either side
    /// is removed from `allow`.
    ///
    /// Union rather than replace because a child that adds one more denied
    /// tool should not silently re-enable everything its base denied.
    #[must_use]
    pub fn inheriting_from(&self, parent: &ToolPolicy) -> ToolPolicy {
        let allow = union_preserving_order(&parent.allow, &self.allow);
        let deny = union_preserving_order(&parent.deny, &self.deny);
        ToolPolicy {
            allow: allow.into_iter().filter(|a| !deny.contains(a)).collect(),
            deny,
        }
    }
}

/// `first` then `second`, de-duplicated, order preserved.
fn union_preserving_order(first: &[String], second: &[String]) -> Vec<String> {
    let mut seen: IndexSet<String> = IndexSet::new();
    for s in first.iter().chain(second.iter()) {
        seen.insert(s.clone());
    }
    seen.into_iter().collect()
}

/// A soul document: an mtime-cached markdown file.
#[derive(Debug)]
pub struct Soul {
    path: PathBuf,
    cache: Mutex<Option<(SystemTime, String)>>,
}

/// Cloning a soul copies the path and the CURRENT cache snapshot. The clone
/// re-stats independently from then on, which is what an in-flight job wants:
/// it keeps reading the same file, and a registry reload underneath it cannot
/// swap the path out from under it.
impl Clone for Soul {
    fn clone(&self) -> Self {
        let cached = self.cache.lock().unwrap_or_else(|p| p.into_inner()).clone();
        Soul {
            path: self.path.clone(),
            cache: Mutex::new(cached),
        }
    }
}

impl Soul {
    pub fn new(path: PathBuf) -> Self {
        Soul {
            path,
            cache: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current soul text; re-stats the file on EVERY call and re-reads only
    /// when the mtime changed since the cached read.
    ///
    /// A missing file is an empty soul (`Ok("")`), so an agent can be created
    /// before its soul.md exists and the document appears on the next prompt
    /// once written — no registry reload required (creating a file inside
    /// `agents/<name>/` does not bump the `agents/` dir mtime, so a hard
    /// error here could never be cleared without touching the parent dir).
    /// Any other I/O failure propagates.
    pub fn text(&self) -> io::Result<String> {
        // A poisoned lock only means another thread panicked mid-read; the
        // cache is a plain value and stays usable.
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        let mtime = match std::fs::metadata(&self.path) {
            Ok(md) => md.modified()?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Drop any stale cache so a deleted-then-recreated file is
                // re-read even if the new mtime happens to match the old one.
                *cache = None;
                return Ok(String::new());
            }
            Err(e) => return Err(e),
        };
        if let Some((cached_mtime, content)) = cache.as_ref() {
            if *cached_mtime == mtime {
                return Ok(content.clone());
            }
        }
        let content = std::fs::read_to_string(&self.path)?;
        *cache = Some((mtime, content.clone()));
        drop(cache);
        Ok(content)
    }
}

/// A named agent (`agents/<name>/agent.toml`).
///
/// `Clone` so a job can take a snapshot at spawn time: a registry reload
/// mid-job must never mutate the agent the job is running under.
#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub label: String,
    /// Must name a known engine (cross-ref validated at load). Empty only in
    /// the window between parsing an agent that inherits its engine via
    /// `extends` and [`resolve_inheritance`] filling it in.
    pub engine: String,
    pub model: Option<String>,
    /// Reasoning effort: `low` | `medium` | `high`. Spliced through the
    /// engine's `effort_args`; engines that declare none ignore it.
    pub effort: Option<String>,
    pub soul: Soul,
    /// Extra markdown appended after the soul, in declaration order. Each is
    /// mtime-cached exactly like the soul.
    pub overlays: Vec<Soul>,
    /// Parent agent name; see the module docs for what is inherited.
    pub extends: Option<String>,
    /// Skill ids (cross-ref validated at load).
    pub skills: Vec<String>,
    /// `default` | `bypassPermissions`.
    pub permission_mode: String,
    /// Optional working-dir override (may contain `~`).
    pub cwd: Option<String>,
    /// Optional PROMPT TEMPLATE NAME wrapping each user prompt (cross-ref
    /// validated against the PromptStore, not inline template text).
    pub prompt_template: Option<String>,
    pub tools: ToolPolicy,
    /// The agent's own directory, so the loader can name `<dir>/agent.toml`
    /// in errors raised after parsing (inheritance, cross-reference).
    pub dir: PathBuf,
    /// Whether `permission_mode` was written in this file. Without it an
    /// inheriting agent could not tell "unset" from an explicit "default".
    permission_mode_explicit: bool,
}

const DEFAULT_SOUL_FILE: &str = "soul.md";
const AGENT_MANIFEST: &str = "agent.toml";
const PERMISSION_MODES: &[&str] = &["default", "bypassPermissions"];
const EFFORTS: &[&str] = &["low", "medium", "high"];

impl AgentDef {
    /// A minimal agent built programmatically (tests and future callers).
    /// `from_toml` is the loader entry point.
    pub fn new(name: &str, agent_dir: &Path, engine: &str) -> Self {
        AgentDef {
            name: name.to_string(),
            label: name.to_string(),
            engine: engine.to_string(),
            model: None,
            effort: None,
            soul: Soul::new(agent_dir.join(DEFAULT_SOUL_FILE)),
            overlays: Vec::new(),
            extends: None,
            skills: Vec::new(),
            permission_mode: "default".to_string(),
            cwd: None,
            prompt_template: None,
            tools: ToolPolicy::default(),
            dir: agent_dir.to_path_buf(),
            permission_mode_explicit: false,
        }
    }

    /// Builder helper for tests: attach skill ids.
    #[must_use]
    pub fn with_skills(mut self, skills: Vec<String>) -> Self {
        self.skills = skills;
        self
    }

    /// The manifest path this agent was (or would be) loaded from.
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join(AGENT_MANIFEST)
    }

    /// Parse an `agents/<name>/agent.toml` document. Soul and overlay paths
    /// resolve relative to `agent_dir`; unknown keys are ignored.
    pub fn try_from_toml(name: &str, agent_dir: &Path, v: &toml::Value) -> Result<Self, FieldError> {
        let table = root_table(v, AGENT_MANIFEST)?;

        let extends = opt_nonempty_string(table, "extends")?;
        if extends.as_deref() == Some(name) {
            return Err(FieldError::key(
                "extends",
                format!("agent '{name}' cannot extend itself"),
            ));
        }

        // `engine` is required, EXCEPT when the agent inherits one. Leaving
        // it empty here is the documented hand-off to `resolve_inheritance`,
        // which errors if the chain never supplies one.
        let engine = match opt_nonempty_string(table, "engine")? {
            Some(s) => s,
            None if extends.is_some() => String::new(),
            None => {
                return Err(FieldError::key(
                    "engine",
                    "is required and must be a non-empty string (or inherit one with `extends`)",
                ))
            }
        };

        let label = opt_nonempty_string(table, "label")?.unwrap_or_else(|| name.to_string());
        let model = opt_nonempty_string(table, "model")?;
        let effort = opt_enum(table, "effort", EFFORTS)?;

        let soul_file = opt_nonempty_string(table, "soul")?.unwrap_or_else(|| DEFAULT_SOUL_FILE.to_string());
        // `Path::join` keeps an absolute path as-is, so both
        // `soul = "soul.md"` and an absolute override work.
        let soul = Soul::new(agent_dir.join(relative_doc(&soul_file, "soul")?));

        let mut overlays = Vec::new();
        for (i, file) in string_list(table, "overlays")?.iter().enumerate() {
            if file.trim().is_empty() {
                return Err(FieldError::key(
                    format!("overlays[{i}]"),
                    "must be a non-empty file name",
                ));
            }
            overlays.push(Soul::new(
                agent_dir.join(relative_doc(file, &format!("overlays[{i}]"))?),
            ));
        }

        let skills = string_list(table, "skills")?;
        for (i, skill) in skills.iter().enumerate() {
            if skill.trim().is_empty() {
                return Err(FieldError::key(
                    format!("skills[{i}]"),
                    "must be a non-empty skill name",
                ));
            }
        }

        let permission_mode_explicit = table.contains_key("permission_mode");
        let permission_mode =
            opt_enum(table, "permission_mode", PERMISSION_MODES)?.unwrap_or_else(|| "default".to_string());

        let cwd = opt_nonempty_string(table, "cwd")?;
        let prompt_template = opt_nonempty_string(table, "prompt_template")?;
        let tools = tool_policy_from(table)?;

        Ok(AgentDef {
            name: name.to_string(),
            label,
            engine,
            model,
            effort,
            soul,
            overlays,
            extends,
            skills,
            permission_mode,
            cwd,
            prompt_template,
            tools,
            dir: agent_dir.to_path_buf(),
            permission_mode_explicit,
        })
    }

    /// `try_from_toml` with the stringly error the loader's `read_toml` chain
    /// still uses.
    pub fn from_toml(name: &str, agent_dir: &Path, v: &toml::Value) -> Result<Self, String> {
        AgentDef::try_from_toml(name, agent_dir, v).map_err(|e| e.message())
    }

    /// Fill this agent's unset fields from `parent`. Idempotent for the
    /// fields it touches, so calling it twice is harmless.
    fn inherit_from(&mut self, parent: &AgentDef) {
        if self.engine.is_empty() {
            self.engine = parent.engine.clone();
        }
        if self.model.is_none() {
            self.model = parent.model.clone();
        }
        if self.effort.is_none() {
            self.effort = parent.effort.clone();
        }
        if self.cwd.is_none() {
            self.cwd = parent.cwd.clone();
        }
        if self.prompt_template.is_none() {
            self.prompt_template = parent.prompt_template.clone();
        }
        if !self.permission_mode_explicit {
            self.permission_mode = parent.permission_mode.clone();
            self.permission_mode_explicit = parent.permission_mode_explicit;
        }
        self.skills = union_preserving_order(&parent.skills, &self.skills);
        self.tools = self.tools.inheriting_from(&parent.tools);
    }
}

/// A soul/overlay path must stay INSIDE the agent directory: these are file
/// names the user typed, and `../../../etc/passwd` is not a soul document.
/// Absolute paths are allowed on purpose (a deliberate shared soul), but
/// traversal out of a relative path is not.
fn relative_doc(file: &str, key: &str) -> Result<PathBuf, FieldError> {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        return Ok(path);
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(FieldError::key(
            key,
            format!("must not escape the agent directory (got '{file}')"),
        ));
    }
    Ok(path)
}

/// Parse the optional `[tools]` table into a [`ToolPolicy`].
fn tool_policy_from(table: &toml::value::Table) -> Result<ToolPolicy, FieldError> {
    let Some(tools) = opt_table(table, "tools")? else {
        return Ok(ToolPolicy::default());
    };
    Ok(ToolPolicy {
        allow: string_list(tools, "allow").map_err(|e| e.under("tools"))?,
        deny: string_list(tools, "deny").map_err(|e| e.under("tools"))?,
    })
}

// ---------------------------------------------------------------------------
// Inheritance resolution (loader entry point)
// ---------------------------------------------------------------------------

/// Resolve every agent's `extends` chain in place.
///
/// Called by the loader AFTER the shared cycle check has already dropped
/// agents on an `extends` cycle, and BEFORE cross-referencing — because an
/// agent inherits the engine and skills its references are checked against.
///
/// It is nevertheless defensive about both failure modes it can still see:
///
/// * a parent that does not exist (or that was itself dropped) — one error
///   per orphan, repeated until the set is stable, so an entire dangling
///   sub-tree is reported rather than just its root;
/// * a chain that survived the cycle check (the check is not wired, or the
///   chain is deeper than [`MAX_DEPTH`]) — the leftovers are dropped with an
///   error rather than looping forever.
///
/// `extends` is deliberately left populated afterwards: [`composed_soul`]
/// walks the same chain lazily so that souls stay hot-reloadable.
pub fn resolve_inheritance(agents: &mut IndexMap<String, AgentDef>, errors: &mut Vec<RegistryError>) {
    // --- 1. Drop agents whose parent is unknown, transitively. ---
    loop {
        let known: IndexSet<String> = agents.keys().cloned().collect();
        let orphans: Vec<(String, String)> = agents
            .iter()
            .filter_map(|(name, def)| {
                let parent = def.extends.as_ref()?;
                (!known.contains(parent)).then(|| (name.clone(), parent.clone()))
            })
            .collect();
        if orphans.is_empty() {
            break;
        }
        let known_names: Vec<&str> = known.iter().map(String::as_str).collect();
        for (name, parent) in orphans {
            if let Some(def) = agents.shift_remove(&name) {
                errors.push(agent_error(
                    &def,
                    FieldError::key("extends", format!("references unknown agent '{parent}'"))
                        .with_known(&known_names),
                ));
            }
        }
    }

    // --- 2. Resolve parents before children. ---
    let mut resolved: IndexSet<String> = agents
        .iter()
        .filter(|(_, def)| def.extends.is_none())
        .map(|(name, _)| name.clone())
        .collect();

    // Each pass resolves at least one agent or we are done; the loop is
    // bounded by the number of agents, so a cycle cannot spin forever.
    loop {
        let ready: Vec<String> = agents
            .iter()
            .filter(|(name, def)| {
                !resolved.contains(*name) && def.extends.as_ref().is_some_and(|p| resolved.contains(p))
            })
            .map(|(name, _)| name.clone())
            .collect();
        if ready.is_empty() {
            break;
        }
        for name in ready {
            let parent_name = agents[&name].extends.clone().expect("ready implies extends");
            let parent = agents[&parent_name].clone();
            if let Some(def) = agents.get_mut(&name) {
                def.inherit_from(&parent);
            }
            resolved.insert(name);
        }
    }

    // --- 3. Anything still unresolved is on a cycle the loader did not
    //        already drop (or past the depth cap). Drop it, loudly. ---
    let stuck: Vec<String> = agents
        .keys()
        .filter(|name| !resolved.contains(*name))
        .cloned()
        .collect();
    for name in stuck {
        if let Some(def) = agents.shift_remove(&name) {
            errors.push(agent_error(
                &def,
                FieldError::key(
                    "extends",
                    format!(
                        "agents: unresolvable inheritance chain (a cycle, or deeper than {MAX_DEPTH} levels)"
                    ),
                ),
            ));
        }
    }

    // --- 4. A chain that never supplied an engine. ---
    let engineless: Vec<String> = agents
        .iter()
        .filter(|(_, def)| def.engine.is_empty())
        .map(|(name, _)| name.clone())
        .collect();
    for name in engineless {
        if let Some(def) = agents.shift_remove(&name) {
            errors.push(agent_error(
                &def,
                FieldError::key(
                    "engine",
                    "is required: no agent in the `extends` chain declares one",
                ),
            ));
        }
    }
}

fn agent_error(def: &AgentDef, err: FieldError) -> RegistryError {
    RegistryError {
        kind: RegistryEntityKind::Agent,
        name: def.name.clone(),
        file: Some(def.manifest_path()),
        message: err.message(),
    }
}

// ---------------------------------------------------------------------------
// Soul composition
// ---------------------------------------------------------------------------

/// The full soul text for `agent`: the `extends` chain ROOT-FIRST, each
/// contributing its own `soul.md` followed by its overlays in declaration
/// order, joined by a blank line.
///
/// Every document is read through its own mtime cache, so this is cheap to
/// call per turn and picks up hand edits immediately — which is the whole
/// point of keeping `extends` unflattened after `resolve_inheritance`.
///
/// Empty documents contribute nothing (no stray blank paragraphs). Missing
/// files are empty, not errors; any other I/O failure propagates.
pub fn composed_soul(agent: &AgentDef, reg: &Registry) -> io::Result<String> {
    let mut chain: Vec<&AgentDef> = Vec::new();
    let mut seen: IndexSet<&str> = IndexSet::new();
    let mut current = Some(agent);
    while let Some(def) = current {
        if !seen.insert(def.name.as_str()) || chain.len() >= MAX_DEPTH {
            // Defence in depth: the loader drops cycles and over-deep chains,
            // but composing a prompt must never hang.
            break;
        }
        chain.push(def);
        current = def.extends.as_deref().and_then(|parent| reg.agents.get(parent));
    }
    chain.reverse();

    let mut parts: Vec<String> = Vec::new();
    for def in chain {
        for doc in std::iter::once(&def.soul).chain(def.overlays.iter()) {
            let text = doc.text()?;
            if !text.trim().is_empty() {
                parts.push(text.trim_end().to_string());
            }
        }
    }
    Ok(parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;
    use std::fs;
    use std::time::Duration;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn parse(name: &str, dir: &Path, text: &str) -> Result<AgentDef, String> {
        let v: toml::Value = text.parse().expect("valid TOML in test");
        AgentDef::from_toml(name, dir, &v)
    }

    /// Force a distinct mtime on `path` (coarse-mtime filesystems would
    /// otherwise make back-to-back writes indistinguishable).
    fn bump_mtime(path: &Path, secs_forward: u64) {
        let f = fs::File::options().write(true).open(path).expect("open");
        let new = SystemTime::now() + Duration::from_secs(secs_forward);
        f.set_modified(new).expect("set_modified");
    }

    /// A registry containing exactly these agents (everything else shipped
    /// defaults), so soul composition can be exercised without laying out a
    /// whole config tree on disk. Loading a non-existent dir is the
    /// "built-ins only" path, which is deliberately cheap.
    fn reg_with(agents: Vec<AgentDef>) -> Registry {
        let mut reg = crate::registry::load(Path::new("/nonexistent-stackhour-config"));
        reg.agents.clear();
        for def in agents {
            reg.agents.insert(def.name.clone(), def);
        }
        reg
    }

    // ---- Soul::text ----

    #[test]
    fn soul_reads_file_content() {
        let dir = tmpdir();
        let path = dir.path().join("soul.md");
        fs::write(&path, "# I am the reviewer\n").unwrap();
        let soul = Soul::new(path.clone());
        assert_eq!(soul.text().unwrap(), "# I am the reviewer\n");
        assert_eq!(soul.path(), path.as_path());
    }

    #[test]
    fn soul_missing_file_is_empty() {
        let dir = tmpdir();
        let soul = Soul::new(dir.path().join("soul.md"));
        assert_eq!(soul.text().unwrap(), "");
    }

    #[test]
    fn soul_caches_until_mtime_changes() {
        let dir = tmpdir();
        let path = dir.path().join("soul.md");
        fs::write(&path, "v1").unwrap();
        let soul = Soul::new(path.clone());
        assert_eq!(soul.text().unwrap(), "v1");

        // Rewrite the content but pin the mtime back to the cached value:
        // text() must serve the (stale) cache — proof it is mtime-driven.
        let cached_mtime = {
            let guard = soul.cache.lock().unwrap();
            guard.as_ref().expect("cache populated").0
        };
        fs::write(&path, "v2").unwrap();
        let f = fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(cached_mtime).unwrap();
        drop(f);
        assert_eq!(soul.text().unwrap(), "v1");

        // Now bump the mtime: the next call re-reads.
        bump_mtime(&path, 10);
        assert_eq!(soul.text().unwrap(), "v2");
    }

    #[test]
    fn soul_file_created_after_first_use_is_picked_up() {
        let dir = tmpdir();
        let path = dir.path().join("soul.md");
        let soul = Soul::new(path.clone());
        assert_eq!(soul.text().unwrap(), "");
        fs::write(&path, "late soul").unwrap();
        assert_eq!(soul.text().unwrap(), "late soul");
    }

    #[test]
    fn soul_deleted_file_reverts_to_empty_and_recreation_rereads() {
        let dir = tmpdir();
        let path = dir.path().join("soul.md");
        fs::write(&path, "v1").unwrap();
        let soul = Soul::new(path.clone());
        assert_eq!(soul.text().unwrap(), "v1");

        fs::remove_file(&path).unwrap();
        assert_eq!(soul.text().unwrap(), "");

        fs::write(&path, "v2").unwrap();
        assert_eq!(soul.text().unwrap(), "v2");
    }

    #[test]
    fn cloned_soul_tracks_the_same_file_independently() {
        let dir = tmpdir();
        let path = dir.path().join("soul.md");
        fs::write(&path, "v1").unwrap();
        let soul = Soul::new(path.clone());
        assert_eq!(soul.text().unwrap(), "v1");

        // A job snapshot taken mid-flight keeps reading the same document.
        let snapshot = soul.clone();
        assert_eq!(soul.path(), path.as_path());
        fs::write(&path, "v2").unwrap();
        bump_mtime(&path, 10);
        assert_eq!(snapshot.text().unwrap(), "v2");
        assert_eq!(snapshot.path(), path.as_path());
    }

    // ---- AgentDef::try_from_toml ----

    #[test]
    fn from_toml_full_document() {
        let dir = tmpdir();
        let def = parse(
            "reviewer",
            dir.path(),
            r#"
label = "Code Reviewer"
engine = "claude"
model = "claude-opus-4"
effort = "high"
soul = "identity.md"
overlays = ["terse.md", "security.md"]
extends = "base"
skills = ["review", "security"]
permission_mode = "bypassPermissions"
cwd = "~/work/repo"
prompt_template = "review-turn"

[tools]
allow = ["Bash", "Edit"]
deny = ["WebSearch"]
"#,
        )
        .expect("parse");

        assert_eq!(def.name, "reviewer");
        assert_eq!(def.label, "Code Reviewer");
        assert_eq!(def.engine, "claude");
        assert_eq!(def.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(def.effort.as_deref(), Some("high"));
        assert_eq!(def.soul.path(), dir.path().join("identity.md").as_path());
        assert_eq!(
            def.overlays.iter().map(|o| o.path()).collect::<Vec<_>>(),
            vec![
                dir.path().join("terse.md").as_path(),
                dir.path().join("security.md").as_path()
            ]
        );
        assert_eq!(def.extends.as_deref(), Some("base"));
        assert_eq!(def.skills, vec!["review".to_string(), "security".to_string()]);
        assert_eq!(def.permission_mode, "bypassPermissions");
        assert_eq!(def.cwd.as_deref(), Some("~/work/repo"));
        assert_eq!(def.prompt_template.as_deref(), Some("review-turn"));
        assert_eq!(
            def.tools,
            ToolPolicy {
                allow: vec!["Bash".to_string(), "Edit".to_string()],
                deny: vec!["WebSearch".to_string()],
            }
        );
        assert_eq!(def.manifest_path(), dir.path().join("agent.toml"));
    }

    #[test]
    fn from_toml_minimal_defaults() {
        let dir = tmpdir();
        let def = parse("helper", dir.path(), "engine = \"codex\"\n").expect("parse");
        assert_eq!(def.name, "helper");
        assert_eq!(def.label, "helper"); // defaults to the dir name
        assert_eq!(def.engine, "codex");
        assert_eq!(def.model, None);
        assert_eq!(def.effort, None);
        assert_eq!(def.soul.path(), dir.path().join("soul.md").as_path());
        assert!(def.overlays.is_empty());
        assert_eq!(def.extends, None);
        assert!(def.skills.is_empty());
        assert_eq!(def.permission_mode, "default");
        assert_eq!(def.cwd, None);
        assert_eq!(def.prompt_template, None);
        assert_eq!(def.tools, ToolPolicy::default());
    }

    #[test]
    fn from_toml_absolute_soul_path_wins() {
        let dir = tmpdir();
        let def = parse(
            "a",
            dir.path(),
            "engine = \"claude\"\nsoul = \"/etc/stackhour/shared-soul.md\"\n",
        )
        .expect("parse");
        assert_eq!(def.soul.path(), Path::new("/etc/stackhour/shared-soul.md"));
    }

    #[test]
    fn from_toml_rejects_traversal_out_of_the_agent_dir() {
        let dir = tmpdir();
        assert_eq!(
            parse(
                "a",
                dir.path(),
                "engine = \"claude\"\nsoul = \"../../etc/passwd\"\n"
            )
            .unwrap_err(),
            "key `soul`: must not escape the agent directory (got '../../etc/passwd')"
        );
        assert_eq!(
            parse(
                "a",
                dir.path(),
                "engine = \"claude\"\noverlays = [\"ok.md\", \"../x.md\"]\n"
            )
            .unwrap_err(),
            "key `overlays[1]`: must not escape the agent directory (got '../x.md')"
        );
    }

    #[test]
    fn from_toml_missing_engine_names_the_key_and_the_escape_hatch() {
        let dir = tmpdir();
        assert_eq!(
            parse("a", dir.path(), "label = \"x\"\n").unwrap_err(),
            "key `engine`: is required and must be a non-empty string (or inherit one with `extends`)"
        );
    }

    #[test]
    fn from_toml_engine_may_be_inherited() {
        let dir = tmpdir();
        let def = parse("child", dir.path(), "extends = \"base\"\n").expect("parse");
        assert_eq!(def.engine, "");
        assert_eq!(def.extends.as_deref(), Some("base"));
    }

    #[test]
    fn from_toml_empty_engine() {
        let dir = tmpdir();
        assert_eq!(
            parse("a", dir.path(), "engine = \"\"\n").unwrap_err(),
            "key `engine`: must be a non-empty string"
        );
    }

    #[test]
    fn from_toml_self_extends_is_rejected_before_the_cycle_checker() {
        let dir = tmpdir();
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\nextends = \"a\"\n").unwrap_err(),
            "key `extends`: agent 'a' cannot extend itself"
        );
    }

    #[test]
    fn from_toml_wrong_types() {
        let dir = tmpdir();
        assert_eq!(
            parse("a", dir.path(), "engine = 5\n").unwrap_err(),
            "key `engine`: must be a string, got an integer"
        );
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\nlabel = 3\n").unwrap_err(),
            "key `label`: must be a string, got an integer"
        );
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\nskills = \"review\"\n").unwrap_err(),
            "key `skills`: must be an array of strings, got a string"
        );
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\noverlays = 3\n").unwrap_err(),
            "key `overlays`: must be an array of strings, got an integer"
        );
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\ntools = 1\n").unwrap_err(),
            "key `tools`: must be a table, got an integer"
        );
        assert_eq!(
            parse(
                "a",
                dir.path(),
                "engine = \"claude\"\n[tools]\nallow = \"Bash\"\n"
            )
            .unwrap_err(),
            "key `tools.allow`: must be an array of strings, got a string"
        );
    }

    #[test]
    fn from_toml_invalid_effort_lists_the_allowed_values() {
        let dir = tmpdir();
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\neffort = \"max\"\n").unwrap_err(),
            "key `effort`: must be one of \"low\", \"medium\", \"high\" (got 'max')"
        );
    }

    #[test]
    fn from_toml_invalid_permission_mode() {
        let dir = tmpdir();
        assert_eq!(
            parse(
                "a",
                dir.path(),
                "engine = \"claude\"\npermission_mode = \"yolo\"\n"
            )
            .unwrap_err(),
            "key `permission_mode`: must be one of \"default\", \"bypassPermissions\" (got 'yolo')"
        );
        // Exact-string match: no case folding.
        assert!(parse(
            "a",
            dir.path(),
            "engine = \"claude\"\npermission_mode = \"BypassPermissions\"\n"
        )
        .is_err());
    }

    #[test]
    fn from_toml_blank_optionals_are_rejected_rather_than_silently_dropped() {
        let dir = tmpdir();
        for key in ["label", "model", "cwd", "prompt_template", "soul", "extends"] {
            let src = format!("engine = \"claude\"\n{key} = \"\"\n");
            let err = parse("a", dir.path(), &src).unwrap_err();
            assert_eq!(err, format!("key `{key}`: must be a non-empty string"));
        }
    }

    #[test]
    fn from_toml_unknown_keys_ignored() {
        let dir = tmpdir();
        let def = parse(
            "a",
            dir.path(),
            "engine = \"claude\"\nfuture_key = true\n[extra]\nx = 1\n",
        )
        .expect("parse");
        assert_eq!(def.engine, "claude");
    }

    #[test]
    fn from_toml_non_table_document() {
        let v = toml::Value::Integer(3);
        assert_eq!(
            AgentDef::from_toml("a", Path::new("/x"), &v).unwrap_err(),
            "agent.toml must be a TOML table"
        );
    }

    #[test]
    fn agent_def_soul_reads_relative_to_agent_dir() {
        let dir = tmpdir();
        fs::write(dir.path().join("soul.md"), "be kind\n").unwrap();
        let def = parse("a", dir.path(), "engine = \"claude\"\n").expect("parse");
        assert_eq!(def.soul.text().unwrap(), "be kind\n");
    }

    // ---- ToolPolicy inheritance ----

    #[test]
    fn tool_policy_unions_and_deny_beats_allow() {
        let parent = ToolPolicy {
            allow: vec!["Bash".into(), "Edit".into()],
            deny: vec!["WebSearch".into()],
        };
        let child = ToolPolicy {
            allow: vec!["Read".into(), "WebSearch".into()],
            deny: vec!["Edit".into()],
        };
        let merged = child.inheriting_from(&parent);
        // Parent order first, child appended, duplicates collapsed, and
        // anything denied by EITHER side removed from allow.
        assert_eq!(merged.allow, vec!["Bash".to_string(), "Read".to_string()]);
        assert_eq!(merged.deny, vec!["WebSearch".to_string(), "Edit".to_string()]);
    }

    #[test]
    fn tool_policy_child_cannot_re_enable_a_parent_denial() {
        let parent = ToolPolicy {
            allow: vec![],
            deny: vec!["Bash".into()],
        };
        let child = ToolPolicy {
            allow: vec!["Bash".into()],
            deny: vec![],
        };
        assert!(child.inheriting_from(&parent).allow.is_empty());
    }

    // ---- resolve_inheritance ----

    fn agent(name: &str, dir: &Path, toml_src: &str) -> AgentDef {
        parse(name, dir, toml_src).expect("parse")
    }

    fn map(defs: Vec<AgentDef>) -> IndexMap<String, AgentDef> {
        defs.into_iter().map(|d| (d.name.clone(), d)).collect()
    }

    #[test]
    fn inheritance_fills_unset_scalars_and_keeps_child_overrides() {
        let dir = tmpdir();
        let mut agents = map(vec![
            agent(
                "base",
                dir.path(),
                "engine = \"claude\"\nmodel = \"opus\"\neffort = \"low\"\ncwd = \"~/base\"\npermission_mode = \"bypassPermissions\"\nprompt_template = \"base-turn\"\n",
            ),
            agent("child", dir.path(), "extends = \"base\"\nmodel = \"sonnet\"\n"),
        ]);
        let mut errors = Vec::new();
        resolve_inheritance(&mut agents, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");

        let child = &agents["child"];
        assert_eq!(child.engine, "claude"); // inherited
        assert_eq!(child.model.as_deref(), Some("sonnet")); // child wins
        assert_eq!(child.effort.as_deref(), Some("low")); // inherited
        assert_eq!(child.cwd.as_deref(), Some("~/base"));
        assert_eq!(child.permission_mode, "bypassPermissions");
        assert_eq!(child.prompt_template.as_deref(), Some("base-turn"));
        // label is NOT inherited: it is the child's own directory name.
        assert_eq!(child.label, "child");
        // `extends` survives, so soul composition can still walk the chain.
        assert_eq!(child.extends.as_deref(), Some("base"));
    }

    #[test]
    fn inheritance_distinguishes_an_explicit_default_from_an_unset_permission_mode() {
        let dir = tmpdir();
        let mut agents = map(vec![
            agent(
                "base",
                dir.path(),
                "engine = \"claude\"\npermission_mode = \"bypassPermissions\"\n",
            ),
            agent(
                "child",
                dir.path(),
                "extends = \"base\"\npermission_mode = \"default\"\n",
            ),
        ]);
        let mut errors = Vec::new();
        resolve_inheritance(&mut agents, &mut errors);
        // An explicit "default" in the child is a real de-escalation, not an
        // absence, so it must NOT be overwritten by the parent's bypass.
        assert_eq!(agents["child"].permission_mode, "default");
    }

    #[test]
    fn inheritance_unions_skills_parent_first_without_duplicates() {
        let dir = tmpdir();
        let mut agents = map(vec![
            agent(
                "base",
                dir.path(),
                "engine = \"claude\"\nskills = [\"review\", \"style\"]\n",
            ),
            agent(
                "child",
                dir.path(),
                "extends = \"base\"\nskills = [\"style\", \"security\"]\n",
            ),
        ]);
        let mut errors = Vec::new();
        resolve_inheritance(&mut agents, &mut errors);
        assert_eq!(
            agents["child"].skills,
            vec!["review".to_string(), "style".to_string(), "security".to_string()]
        );
    }

    #[test]
    fn inheritance_is_transitive_regardless_of_declaration_order() {
        let dir = tmpdir();
        // grandchild is declared FIRST, before either of its ancestors.
        let mut agents = map(vec![
            agent("grandchild", dir.path(), "extends = \"child\"\n"),
            agent("child", dir.path(), "extends = \"base\"\nmodel = \"sonnet\"\n"),
            agent("base", dir.path(), "engine = \"claude\"\neffort = \"high\"\n"),
        ]);
        let mut errors = Vec::new();
        resolve_inheritance(&mut agents, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        let g = &agents["grandchild"];
        assert_eq!(g.engine, "claude");
        assert_eq!(g.model.as_deref(), Some("sonnet"));
        assert_eq!(g.effort.as_deref(), Some("high"));
    }

    #[test]
    fn inheritance_drops_an_agent_whose_parent_is_unknown_and_names_the_known_ones() {
        let dir = tmpdir();
        let mut agents = map(vec![
            agent("base", dir.path(), "engine = \"claude\"\n"),
            agent("child", dir.path(), "extends = \"ghost\"\n"),
        ]);
        let mut errors = Vec::new();
        resolve_inheritance(&mut agents, &mut errors);
        assert!(!agents.contains_key("child"));
        assert!(agents.contains_key("base"));
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].message,
            "key `extends`: references unknown agent 'ghost' (known: base, child)"
        );
        assert_eq!(errors[0].file, Some(dir.path().join("agent.toml")));
        assert_eq!(errors[0].name, "child");
    }

    #[test]
    fn inheritance_reports_every_agent_in_a_dangling_subtree() {
        let dir = tmpdir();
        let mut agents = map(vec![
            agent("child", dir.path(), "extends = \"ghost\"\n"),
            agent("grandchild", dir.path(), "extends = \"child\"\n"),
        ]);
        let mut errors = Vec::new();
        resolve_inheritance(&mut agents, &mut errors);
        assert!(agents.is_empty());
        let names: Vec<&str> = errors.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["child", "grandchild"]);
    }

    #[test]
    fn inheritance_drops_a_cycle_the_loader_did_not_catch() {
        let dir = tmpdir();
        // Constructed directly: try_from_toml rejects self-extends, and the
        // loader's cycle check would normally have removed these already.
        let mut a = AgentDef::new("a", dir.path(), "claude");
        a.extends = Some("b".into());
        let mut b = AgentDef::new("b", dir.path(), "claude");
        b.extends = Some("a".into());
        let mut agents = map(vec![a, b]);
        let mut errors = Vec::new();
        resolve_inheritance(&mut agents, &mut errors);
        assert!(agents.is_empty());
        assert_eq!(errors.len(), 2);
        assert!(
            errors[0].message.contains("unresolvable inheritance chain"),
            "got {}",
            errors[0].message
        );
    }

    #[test]
    fn inheritance_drops_a_chain_that_never_supplies_an_engine() {
        let dir = tmpdir();
        let mut agents = map(vec![
            agent("base", dir.path(), "extends = \"root\"\n"),
            agent("root", dir.path(), "model = \"x\"\nextends = \"leaf\"\n"),
            // `leaf` has no engine and no parent — nothing in the chain does.
            {
                let mut leaf = AgentDef::new("leaf", dir.path(), "");
                leaf.model = Some("y".into());
                leaf
            },
        ]);
        let mut errors = Vec::new();
        resolve_inheritance(&mut agents, &mut errors);
        assert!(agents.is_empty(), "{:?}", agents.keys().collect::<Vec<_>>());
        assert!(errors
            .iter()
            .all(|e| e.message.contains("no agent in the `extends` chain declares one")));
    }

    #[test]
    fn inheritance_is_a_no_op_without_extends() {
        let dir = tmpdir();
        let mut agents = map(vec![agent("solo", dir.path(), "engine = \"claude\"\n")]);
        let mut errors = Vec::new();
        resolve_inheritance(&mut agents, &mut errors);
        assert!(errors.is_empty());
        assert_eq!(agents["solo"].engine, "claude");
        assert_eq!(agents.len(), 1);
    }

    // ---- composed_soul ----

    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn composed_soul_of_a_lone_agent_is_just_its_soul() {
        let dir = tmpdir();
        write(dir.path(), "soul.md", "I am alone.\n");
        let def = agent("solo", dir.path(), "engine = \"claude\"\n");
        let reg = reg_with(vec![def.clone()]);
        assert_eq!(composed_soul(&def, &reg).unwrap(), "I am alone.");
    }

    #[test]
    fn composed_soul_appends_overlays_in_declaration_order() {
        let dir = tmpdir();
        write(dir.path(), "soul.md", "base voice");
        write(dir.path(), "terse.md", "be terse");
        write(dir.path(), "sign.md", "sign off");
        let def = agent(
            "a",
            dir.path(),
            "engine = \"claude\"\noverlays = [\"terse.md\", \"sign.md\"]\n",
        );
        let reg = reg_with(vec![def.clone()]);
        assert_eq!(
            composed_soul(&def, &reg).unwrap(),
            "base voice\n\nbe terse\n\nsign off"
        );
    }

    #[test]
    fn composed_soul_walks_the_extends_chain_root_first() {
        let root = tmpdir();
        let mid = tmpdir();
        let leaf = tmpdir();
        write(root.path(), "soul.md", "ROOT");
        write(mid.path(), "soul.md", "MID");
        write(mid.path(), "extra.md", "MID-OVERLAY");
        write(leaf.path(), "soul.md", "LEAF");

        let base = agent("base", root.path(), "engine = \"claude\"\n");
        let middle = agent(
            "middle",
            mid.path(),
            "extends = \"base\"\nengine = \"claude\"\noverlays = [\"extra.md\"]\n",
        );
        let tip = agent("tip", leaf.path(), "extends = \"middle\"\nengine = \"claude\"\n");
        let reg = reg_with(vec![base, middle, tip.clone()]);

        assert_eq!(
            composed_soul(&tip, &reg).unwrap(),
            "ROOT\n\nMID\n\nMID-OVERLAY\n\nLEAF"
        );
    }

    #[test]
    fn composed_soul_skips_empty_and_missing_documents() {
        let dir = tmpdir();
        // No soul.md at all; one blank overlay, one real one.
        write(dir.path(), "blank.md", "   \n\n");
        write(dir.path(), "real.md", "the only line");
        let def = agent(
            "a",
            dir.path(),
            "engine = \"claude\"\noverlays = [\"blank.md\", \"real.md\"]\n",
        );
        let reg = reg_with(vec![def.clone()]);
        assert_eq!(composed_soul(&def, &reg).unwrap(), "the only line");
    }

    #[test]
    fn composed_soul_of_an_empty_agent_is_empty() {
        let dir = tmpdir();
        let def = agent("a", dir.path(), "engine = \"claude\"\n");
        let reg = reg_with(vec![def.clone()]);
        assert_eq!(composed_soul(&def, &reg).unwrap(), "");
    }

    #[test]
    fn composed_soul_picks_up_a_hand_edit_without_a_reload() {
        let dir = tmpdir();
        write(dir.path(), "soul.md", "before");
        let def = agent("a", dir.path(), "engine = \"claude\"\n");
        let reg = reg_with(vec![def.clone()]);
        assert_eq!(composed_soul(&def, &reg).unwrap(), "before");

        write(dir.path(), "soul.md", "after");
        bump_mtime(&dir.path().join("soul.md"), 10);
        // Same AgentDef, same Registry, no reload: the edit is live.
        assert_eq!(composed_soul(&def, &reg).unwrap(), "after");
    }

    #[test]
    fn composed_soul_terminates_on_a_cycle_instead_of_hanging() {
        let dir = tmpdir();
        write(dir.path(), "soul.md", "S");
        let mut a = AgentDef::new("a", dir.path(), "claude");
        a.extends = Some("b".into());
        let mut b = AgentDef::new("b", dir.path(), "claude");
        b.extends = Some("a".into());
        let reg = reg_with(vec![a.clone(), b]);
        // Both souls point at the same file, so the text repeats — the
        // guarantee under test is that this RETURNS.
        assert_eq!(composed_soul(&a, &reg).unwrap(), "S\n\nS");
    }

    #[test]
    fn composed_soul_ignores_an_extends_pointing_at_a_missing_agent() {
        let dir = tmpdir();
        write(dir.path(), "soul.md", "orphan");
        let mut def = AgentDef::new("a", dir.path(), "claude");
        def.extends = Some("ghost".into());
        let reg = reg_with(vec![def.clone()]);
        assert_eq!(composed_soul(&def, &reg).unwrap(), "orphan");
    }
}
