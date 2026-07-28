//! Layer-2 config-directory registry (NEW in the Rust rewrite).
//!
//! Root = `StoragePaths::config_dir`. Scans `engines/`, `agents/`, `skills/`,
//! `commands/`, `prompts/`; merges over embedded built-ins (precedence:
//! built-ins < directory files; the reserved built-in commands can never be
//! shadowed — `CommandDef::from_toml` rejects `RESERVED` names). Per-file
//! validation errors and cross-reference errors (agent→engine, agent→skills,
//! command→agent/template) are collected into `Registry::errors` — bad files
//! are skipped, NEVER fatal to a daemon. The library itself does not log;
//! callers (coordinator log, doctor checks) surface `errors`. Absent
//! directories = built-ins only = byte-exact legacy behaviour.
//!
//! Hot reload is deliberately cheap: `reload_if_changed` re-stats the mtimes
//! of the five subdirectories only. Adding/removing/renaming entries in a
//! subdir bumps its mtime and triggers a full reload; edits to file CONTENTS
//! inside nested entry dirs (e.g. `agents/x/agent.toml`) do not — soul,
//! skill-body and prompt files have their own per-file mtime caches, so prose
//! edits still take effect without a reload.
//!
//! Directory convention (documented in configSchema):
//! - `engines/<name>.toml`         -> [`EngineDef`]
//! - `agents/<name>/agent.toml`    -> [`AgentDef`] (soul relative to the dir)
//! - `skills/<name>/skill.toml`    -> [`SkillDef`]
//! - `commands/<name>.toml`        -> [`CommandDef`]
//! - `prompts/<name>.md`           -> [`PromptStore`] overrides
//!
//! Dot-prefixed entries (editor artifacts) are ignored everywhere; entries
//! are processed in sorted filename order so the resulting `IndexMap`s are
//! deterministic.

use indexmap::IndexMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub mod agent_def;
pub mod args;
pub mod command;
pub mod cycle;
pub mod defaults;
pub mod engine;
pub mod error;
pub mod prompt;
pub mod skill;
pub mod toml_util;

pub use agent_def::{AgentDef, Soul, ToolPolicy};
pub use args::{bind_args, ArgSpec};
pub use command::{CommandDef, CommandKind, RESERVED};
pub use cycle::detect_cycles;
pub use engine::{ArgvVars, EngineDef, PromptDelivery, ResumeStyle, StreamKind};
pub use error::FieldError;
pub use prompt::PromptStore;
pub use skill::SkillDef;

const ENGINES_DIR: &str = "engines";
const AGENTS_DIR: &str = "agents";
const SKILLS_DIR: &str = "skills";
const COMMANDS_DIR: &str = "commands";
const PROMPTS_DIR: &str = "prompts";
const AGENT_MANIFEST: &str = "agent.toml";
const SKILL_MANIFEST: &str = "skill.toml";
/// The legacy settings file. Only its optional `bridge` object is read here.
const CONFIG_JSON: &str = "config.json";
/// The default target of the LEGACY roster, used when nothing configures one.
const DEFAULT_TARGET: &str = "gcp";
/// The engine selected when nothing configures one — matches the JS
/// coordinator's initial state, so an unconfigured user sees no change.
const DEFAULT_ENGINE: &str = "claude";

/// One runnable target the bridge can switch to.
///
/// DELIBERATE DIVERGENCE from the Node bridge: the JS coordinator hardcoded
/// the gcp+mac pair everywhere. The registry is now loaded with a roster of
/// these, so the switch commands, the target validation and the help text all
/// follow whatever targets a deployment actually has. The legacy entry points
/// ([`load`], [`load_with`]) still load the historical pair via
/// [`legacy_targets`], so an unconfigured bridge is byte-identical to before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSpec {
    /// Roster name (`gcp`, `mac`, …). Doubles as the switch-command name, so
    /// it should satisfy Telegram's command-name rule.
    pub name: String,
    /// Human label ("GCP", "Mac", …), used on buttons and in help lines.
    pub label: String,
    /// `"local"` = runs on the coordinator's own box; anything else is a
    /// worker / remote lane.
    pub kind: String,
}

impl TargetSpec {
    pub fn new(name: impl Into<String>, label: impl Into<String>, kind: impl Into<String>) -> Self {
        TargetSpec {
            name: name.into(),
            label: label.into(),
            kind: kind.into(),
        }
    }

    /// Whether this target runs on the coordinator's own box.
    pub fn is_local(&self) -> bool {
        self.kind == "local"
    }

    /// The emoji on this target's button and `/help` line. Inherited from the
    /// legacy pair, where the LOCAL box was the GCP cloud instance (☁️) and
    /// the remote worker was the Mac (🖥️).
    pub(crate) fn emoji(&self) -> &'static str {
        if self.is_local() {
            "☁️"
        } else {
            "🖥️"
        }
    }

    /// The "run on …" phrase used in descriptions, toasts and help lines.
    /// The two legacy names keep their historical wording byte-for-byte; any
    /// other target reads as its label.
    pub(crate) fn phrase(&self) -> &str {
        match self.name.as_str() {
            "mac" => "the Mac",
            "gcp" => "the GCP box",
            _ => &self.label,
        }
    }
}

/// The historical gcp+mac roster every legacy entry point loads with.
///
/// Listed mac-first because that is the shipped command-table order (/mac
/// precedes /gcp; buttons 20 and 21). `(known: …)` error tails are sorted
/// like every other known-list in this module, so their wording stays
/// "gcp, mac" regardless.
pub fn legacy_targets() -> Vec<TargetSpec> {
    vec![
        TargetSpec::new("mac", "Mac", "remote"),
        TargetSpec::new("gcp", "GCP", "local"),
    ]
}

/// Sorted target-name list used for `(known: a, b)` error tails.
pub(crate) fn sorted_target_names(targets: &[TargetSpec]) -> Vec<&str> {
    let mut names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    names
}

/// Which registry surface a validation error belongs to; determines the
/// doctor check name (`registry`, `engine-<id>`, `agent-<name>`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryEntityKind {
    Registry,
    Engine,
    Agent,
    Skill,
    Command,
    Prompt,
}

/// One skipped/broken registry entry, surfaced by `stackhour doctor` and the
/// bridge doctor.
#[derive(Debug, Clone)]
pub struct RegistryError {
    pub kind: RegistryEntityKind,
    /// Entity id/name ("" for directory-level errors).
    pub name: String,
    /// Offending file, when known.
    pub file: Option<PathBuf>,
    /// Human-readable validation / cross-reference error.
    pub message: String,
}

impl std::fmt::Display for RegistryError {
    /// The one-line form printed by `stackhour bridge doctor`:
    /// `agents/reviewer/agent.toml: key `engine`: references unknown engine
    /// 'gpt5' (known: claude, codex)`. Falls back to the entity name when no
    /// file is known (env-layer and directory-level errors).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.file {
            Some(file) => write!(f, "{}: {}", file.display(), self.message),
            None if !self.name.is_empty() => write!(f, "{}: {}", self.name, self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Snapshot of the five subdir mtimes used for cheap hot-reload detection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DirMtimes {
    pub(crate) engines: Option<SystemTime>,
    pub(crate) agents: Option<SystemTime>,
    pub(crate) skills: Option<SystemTime>,
    pub(crate) commands: Option<SystemTime>,
    pub(crate) prompts: Option<SystemTime>,
    /// config.json's mtime, so edits to the `bridge` defaults object reload
    /// on the same tick as everything else.
    pub(crate) config_json: Option<SystemTime>,
}

/// Where the env layer reads from. A [`Registry`] keeps its source so that
/// `reload_if_changed` resolves defaults the same way the original load did —
/// tests can pin a fixed environment without touching the process env.
#[derive(Debug, Clone)]
pub enum EnvSource {
    /// `std::env::var` (production).
    Process,
    /// A fixed map (tests, and callers that already parsed their env).
    Fixed(IndexMap<String, String>),
}

impl EnvSource {
    /// JS-truthy lookup: an unset OR empty variable falls through to the
    /// lower layer, matching `env.X || default` in the Node implementation.
    fn get(&self, key: &str) -> Option<String> {
        let raw = match self {
            EnvSource::Process => std::env::var(key).ok(),
            EnvSource::Fixed(map) => map.get(key).cloned(),
        };
        raw.filter(|v| !v.trim().is_empty())
    }

    /// Build a fixed source from pairs (test helper).
    pub fn fixed(pairs: &[(&str, &str)]) -> Self {
        EnvSource::Fixed(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }
}

/// The scalar defaults resolved across all four layers.
///
/// These are *starting points*, not constraints: the bridge's own runtime
/// state (which the user changes with /claude, /codex, /mac, /gcp) still wins
/// once a session exists. They decide what a FRESH state looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDefaults {
    /// Default agent name, or None for "no agent" — which is exactly today's
    /// behaviour, so an unconfigured bridge is unchanged.
    pub agent: Option<String>,
    /// Default engine name. Always set; falls back to `claude`.
    pub engine: String,
    /// Default target (`gcp` | `mac`). Always set; falls back to `gcp`.
    pub target: String,
}

impl Default for ResolvedDefaults {
    fn default() -> Self {
        ResolvedDefaults {
            agent: None,
            engine: DEFAULT_ENGINE.to_string(),
            target: DEFAULT_TARGET.to_string(),
        }
    }
}

/// The loaded registry: built-ins merged with directory files.
#[derive(Debug)]
pub struct Registry {
    pub engines: IndexMap<String, EngineDef>,
    pub agents: IndexMap<String, AgentDef>,
    pub skills: IndexMap<String, SkillDef>,
    pub commands: IndexMap<String, CommandDef>,
    pub prompts: PromptStore,
    /// Validation / cross-reference errors from skipped entries.
    pub errors: Vec<RegistryError>,
    /// Scalar defaults resolved across embedded < config.json < env.
    pub defaults: ResolvedDefaults,
    /// The target roster this registry was loaded with, in roster order.
    pub targets: Vec<TargetSpec>,
    /// The fallback default target, replayed on reload ([`Registry::defaults`]
    /// holds the RESOLVED value, which config.json / env may have overridden).
    pub(crate) default_target: String,
    /// Registry root (= config_dir), kept for reloads.
    pub(crate) root: PathBuf,
    /// Subdir mtimes at load time, for `reload_if_changed`.
    pub(crate) mtimes: DirMtimes,
    /// Env layer source, replayed on reload.
    pub(crate) env: EnvSource,
}

/// Load the registry from `config_dir` using the process environment.
///
/// Never fails: bad files become `errors` entries; a missing directory yields
/// built-ins only, which is byte-for-byte today's behaviour.
pub fn load(config_dir: &Path) -> Registry {
    load_with(config_dir, EnvSource::Process)
}

/// `load` with an explicit env layer. Used by tests and by callers that have
/// already captured their environment. Loads the legacy gcp+mac roster.
pub fn load_with(config_dir: &Path, env: EnvSource) -> Registry {
    load_with_targets(config_dir, env, &legacy_targets(), DEFAULT_TARGET)
}

/// `load_with` with an explicit target roster and fallback default target.
///
/// Everything target-shaped is derived from the roster: the generated switch
/// commands (see [`command::builtin_commands_for`]), `target` validation in
/// command files, `bridge.defaultTarget` / `STACKHOUR_TARGET` validation, and
/// the generated `/help` target lines.
pub fn load_with_targets(
    config_dir: &Path,
    env: EnvSource,
    targets: &[TargetSpec],
    default_target: &str,
) -> Registry {
    let root = config_dir.to_path_buf();
    // Stat BEFORE scanning: a change landing mid-scan yields differing
    // mtimes on the next `reload_if_changed` stat, so it is never missed.
    let mtimes = stat_dir_mtimes(&root);
    let mut errors: Vec<RegistryError> = Vec::new();

    // ------------------------------------------------------------------
    // Layer 1+3, engines: built-ins first; directory files override by name.
    // IndexMap::insert on an existing key replaces the value and KEEPS the
    // slot, so an overridden built-in stays where it was in /help.
    // ------------------------------------------------------------------
    let mut engines: IndexMap<String, EngineDef> = IndexMap::new();
    for builtin in [engine::builtin_claude(), engine::builtin_codex()] {
        engines.insert(builtin.name.clone(), builtin);
    }
    for (name, path) in scan_toml_files(&root.join(ENGINES_DIR), &mut errors) {
        match read_toml(&path).and_then(|v| EngineDef::from_toml(&name, &v)) {
            Ok(def) => {
                engines.insert(name, def);
            }
            Err(message) => errors.push(RegistryError {
                kind: RegistryEntityKind::Engine,
                name,
                file: Some(path),
                message,
            }),
        }
    }

    // ------------------------------------------------------------------
    // Prompts. Loaded FIRST of the cross-referenced entities because both
    // skills and commands may name a template. The store resolves built-ins
    // and on-disk overrides, and a stem that matches no built-in defines a
    // NEW template.
    // ------------------------------------------------------------------
    let prompts = PromptStore::new_with_targets(
        if root.is_dir() {
            Some(root.join(PROMPTS_DIR))
        } else {
            None
        },
        targets,
    );

    // ------------------------------------------------------------------
    // Skills. Before agents, because agents reference skills and a skill
    // dropped for a cycle must invalidate the agents that named it.
    // ------------------------------------------------------------------
    let mut skills: IndexMap<String, SkillDef> = IndexMap::new();
    let mut skill_files: IndexMap<String, PathBuf> = IndexMap::new();
    for (name, manifest, dir) in scan_manifest_dirs(
        &root.join(SKILLS_DIR),
        SKILL_MANIFEST,
        RegistryEntityKind::Skill,
        &mut errors,
    ) {
        match read_toml(&manifest).and_then(|v| SkillDef::from_toml(&name, &dir, &v)) {
            Ok(def) => {
                skill_files.insert(name.clone(), manifest);
                skills.insert(name, def);
            }
            Err(message) => errors.push(RegistryError {
                kind: RegistryEntityKind::Skill,
                name,
                file: Some(manifest),
                message,
            }),
        }
    }
    apply_cycle_check(
        &mut skills,
        skill_uses,
        "skills",
        "uses",
        RegistryEntityKind::Skill,
        &skill_files,
        &mut errors,
    );

    // ------------------------------------------------------------------
    // Agents. Cross-referenced against engines, skills and prompts.
    // ------------------------------------------------------------------
    let mut agents: IndexMap<String, AgentDef> = IndexMap::new();
    let mut agent_files: IndexMap<String, PathBuf> = IndexMap::new();
    for (name, manifest, dir) in scan_manifest_dirs(
        &root.join(AGENTS_DIR),
        AGENT_MANIFEST,
        RegistryEntityKind::Agent,
        &mut errors,
    ) {
        match read_toml(&manifest).and_then(|v| AgentDef::from_toml(&name, &dir, &v)) {
            Ok(def) => {
                agent_files.insert(name.clone(), manifest);
                agents.insert(name, def);
            }
            Err(message) => errors.push(RegistryError {
                kind: RegistryEntityKind::Agent,
                name,
                file: Some(manifest),
                message,
            }),
        }
    }
    // Cycles BEFORE cross-reference: an agent dropped for an `extends` cycle
    // must not also be reported for the references it inherits.
    apply_cycle_check(
        &mut agents,
        agent_extends,
        "agents",
        "extends",
        RegistryEntityKind::Agent,
        &agent_files,
        &mut errors,
    );
    // Inheritance BEFORE cross-reference: an agent inherits the engine and
    // skills its references are checked against, so an `extends` child that
    // declares neither must be flattened first or it is rejected for an
    // engine it does in fact have.
    agent_def::resolve_inheritance(&mut agents, &mut errors);
    let engine_names = keys_of(&engines);
    let skill_names = keys_of(&skills);
    cross_reference_agents(
        &mut agents,
        &agent_files,
        &engine_names,
        &skill_names,
        &prompts,
        &mut errors,
    );

    // ------------------------------------------------------------------
    // Commands: the shipped table with user files substituted IN PLACE.
    // User files are validated on their own first, so a broken user command
    // leaves the shipped one it would have replaced intact.
    // ------------------------------------------------------------------
    let mut user_commands: IndexMap<String, CommandDef> = IndexMap::new();
    let mut command_files: IndexMap<String, PathBuf> = IndexMap::new();
    for (name, path) in scan_toml_files(&root.join(COMMANDS_DIR), &mut errors) {
        match read_toml(&path).and_then(|v| CommandDef::from_toml_with_targets(&name, &v, targets)) {
            Ok(def) => {
                command_files.insert(name.clone(), path);
                user_commands.insert(name, def);
            }
            Err(message) => errors.push(RegistryError {
                kind: RegistryEntityKind::Command,
                name,
                file: Some(path),
                message,
            }),
        }
    }
    let mut commands = command::effective_table_for(&user_commands, targets);
    apply_cycle_check(
        &mut commands,
        command_steps,
        "commands",
        "steps",
        RegistryEntityKind::Command,
        &command_files,
        &mut errors,
    );
    let agent_names = keys_of(&agents);
    cross_reference_commands(
        &mut commands,
        &command_files,
        &agent_names,
        &engine_names,
        &skill_names,
        &prompts,
        &mut errors,
    );
    // Both passes above `shift_remove` a rejected entry from the MERGED
    // table. For a USER file that is right, but when the user file was
    // overriding a shipped command it would delete `/gcp` outright instead of
    // reverting to the built-in — the exact opposite of the documented "a
    // broken user command leaves the shipped one it would have replaced
    // intact". Put the built-ins back, in their original slots.
    restore_dropped_builtin_commands(&mut commands, targets);

    // ------------------------------------------------------------------
    // Layers 2 and 4: config.json's `bridge` object, then env.
    // ------------------------------------------------------------------
    let defaults = resolve_defaults(
        &root,
        &env,
        &agents,
        &engines,
        targets,
        default_target,
        &mut errors,
    );

    Registry {
        engines,
        agents,
        skills,
        commands,
        prompts,
        errors,
        defaults,
        targets: targets.to_vec(),
        default_target: default_target.to_string(),
        root,
        mtimes,
        env,
    }
}

// ---------------------------------------------------------------------------
// Composition-graph edges
//
// One accessor per graph, so wiring a newly-landed manifest key into cycle
// detection is a ONE-LINE change here rather than a new call site. Each
// returns the names this entity composes IN, in declaration order.
// ---------------------------------------------------------------------------

/// Re-insert any shipped command that validation dropped, restoring both the
/// definition and its slot in the shipped order. The shipped table includes
/// one generated switch command per roster target, so EVERY target's switch
/// command is guaranteed-restored — not just the legacy `/gcp` and `/mac`.
///
/// `Registry.commands` must always be the EFFECTIVE table a consumer can act
/// on. Leaving a hole here made the field a trap: it happened to look right
/// only because both current consumers redundantly re-applied
/// `command::effective_table`, and the next consumer (a doctor listing the
/// effective table, say) would have silently lost `/gcp`.
fn restore_dropped_builtin_commands(commands: &mut IndexMap<String, CommandDef>, targets: &[TargetSpec]) {
    let builtins = command::builtin_commands_for(targets);
    if builtins.keys().all(|name| commands.contains_key(name)) {
        return;
    }
    // Rebuild rather than patch, so the restored entry lands back in its
    // shipped position instead of being appended after the user's commands.
    let mut rebuilt = builtins;
    for (name, def) in commands.iter() {
        rebuilt.insert(name.clone(), def.clone());
    }
    *commands = rebuilt;
}

/// Agents compose via `extends` (AGENTS pillar).
///
/// `resolve_inheritance` would also catch a cycle, but only as the generic
/// "unresolvable inheritance chain" — running it through the shared cycle
/// checker is what gets agents the same named `a -> b -> a` path that skills
/// and commands report.
fn agent_extends(def: &AgentDef) -> Vec<String> {
    def.extends.clone().into_iter().collect()
}

/// Skills compose via `uses` (SKILLS pillar).
fn skill_uses(def: &SkillDef) -> Vec<String> {
    def.uses.clone()
}

/// Commands compose via `steps` (kind = "sequence").
fn command_steps(def: &CommandDef) -> Vec<String> {
    def.steps.clone()
}

/// Run [`cycle::check`] over an entity map, drop every node on a cycle (or
/// past the depth cap) and record one error per dropped node.
fn apply_cycle_check<T, F>(
    entries: &mut IndexMap<String, T>,
    edges_of: F,
    label: &str,
    key: &str,
    kind: RegistryEntityKind,
    files: &IndexMap<String, PathBuf>,
    errors: &mut Vec<RegistryError>,
) where
    F: Fn(&T) -> Vec<String>,
{
    for (name, err) in cycle::check(entries, edges_of, label, key) {
        errors.push(RegistryError {
            kind,
            message: err.message(),
            file: files.get(&name).cloned(),
            name: name.clone(),
        });
        entries.shift_remove(&name);
    }
}

/// Sorted, de-duplicated key list used for the `(known: a, b, c)` tails.
fn keys_of<T>(map: &IndexMap<String, T>) -> Vec<String> {
    let mut out: Vec<String> = map.keys().cloned().collect();
    out.sort();
    out
}

fn known_slice(names: &[String]) -> Vec<&str> {
    names.iter().map(String::as_str).collect()
}

// ---------------------------------------------------------------------------
// Layer 2 (config.json) + layer 4 (env)
// ---------------------------------------------------------------------------

/// Resolve the scalar defaults across embedded < config.json < env.
///
/// Unknown names are reported (naming the file and key) and IGNORED, falling
/// back to the lower layer, because a typo in a default must not take the
/// bridge down.
fn resolve_defaults(
    root: &Path,
    env: &EnvSource,
    agents: &IndexMap<String, AgentDef>,
    engines: &IndexMap<String, EngineDef>,
    targets: &[TargetSpec],
    default_target: &str,
    errors: &mut Vec<RegistryError>,
) -> ResolvedDefaults {
    let mut out = ResolvedDefaults {
        target: default_target.to_string(),
        ..ResolvedDefaults::default()
    };
    let config_json = root.join(CONFIG_JSON);

    // --- Layer 2: the optional "bridge" object in config.json. ---------
    if let Some(bridge) = read_bridge_object(&config_json, errors) {
        let mut take = |key: &str| -> Option<String> {
            match bridge.get(key) {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
                Some(_) => {
                    errors.push(RegistryError {
                        kind: RegistryEntityKind::Registry,
                        name: String::new(),
                        file: Some(config_json.clone()),
                        message: FieldError::key(format!("bridge.{key}"), "must be a non-empty string")
                            .message(),
                    });
                    None
                }
            }
        };
        let (agent, engine, target) = (take("defaultAgent"), take("defaultEngine"), take("defaultTarget"));
        apply_default_agent(
            &mut out,
            agent,
            agents,
            &config_json,
            "bridge.defaultAgent",
            errors,
        );
        apply_default_engine(
            &mut out,
            engine,
            engines,
            &config_json,
            "bridge.defaultEngine",
            errors,
        );
        apply_default_target(
            &mut out,
            target,
            targets,
            &config_json,
            "bridge.defaultTarget",
            errors,
        );
    }

    // --- Layer 4: env, highest precedence. -----------------------------
    let env_file = PathBuf::from("<env>");
    apply_default_agent(
        &mut out,
        env.get("STACKHOUR_AGENT"),
        agents,
        &env_file,
        "STACKHOUR_AGENT",
        errors,
    );
    apply_default_engine(
        &mut out,
        env.get("STACKHOUR_ENGINE"),
        engines,
        &env_file,
        "STACKHOUR_ENGINE",
        errors,
    );
    apply_default_target(
        &mut out,
        env.get("STACKHOUR_TARGET"),
        targets,
        &env_file,
        "STACKHOUR_TARGET",
        errors,
    );
    out
}

fn apply_default_agent(
    out: &mut ResolvedDefaults,
    value: Option<String>,
    agents: &IndexMap<String, AgentDef>,
    file: &Path,
    key: &str,
    errors: &mut Vec<RegistryError>,
) {
    let Some(name) = value else { return };
    if agents.contains_key(&name) {
        out.agent = Some(name);
        return;
    }
    let known = keys_of(agents);
    errors.push(defaults_error(
        file,
        FieldError::key(key, format!("references unknown agent '{name}'")).with_known(&known_slice(&known)),
    ));
}

fn apply_default_engine(
    out: &mut ResolvedDefaults,
    value: Option<String>,
    engines: &IndexMap<String, EngineDef>,
    file: &Path,
    key: &str,
    errors: &mut Vec<RegistryError>,
) {
    let Some(name) = value else { return };
    if engines.contains_key(&name) {
        out.engine = name;
        return;
    }
    let known = keys_of(engines);
    errors.push(defaults_error(
        file,
        FieldError::key(key, format!("references unknown engine '{name}'")).with_known(&known_slice(&known)),
    ));
}

fn apply_default_target(
    out: &mut ResolvedDefaults,
    value: Option<String>,
    targets: &[TargetSpec],
    file: &Path,
    key: &str,
    errors: &mut Vec<RegistryError>,
) {
    let Some(name) = value else { return };
    if targets.iter().any(|t| t.name == name) {
        out.target = name;
        return;
    }
    // DELIBERATE DIVERGENCE from the Node throw string (`must be "gcp" or
    // "mac"`): the roster is no longer a fixed pair, so the error names
    // whatever roster this registry was loaded with, sorted like every other
    // known-list tail.
    let quoted: Vec<String> = sorted_target_names(targets)
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect();
    errors.push(defaults_error(
        file,
        FieldError::key(
            key,
            format!("must be one of {} (got '{name}')", quoted.join(", ")),
        ),
    ));
}

fn defaults_error(file: &Path, err: FieldError) -> RegistryError {
    RegistryError {
        kind: RegistryEntityKind::Registry,
        name: String::new(),
        file: Some(file.to_path_buf()),
        message: err.message(),
    }
}

/// Read the optional `bridge` object out of config.json.
///
/// A MISSING file is silent — that is the overwhelmingly common case and the
/// backward-compatible one. An unreadable or malformed file is reported and
/// treated as absent; the registry must never refuse to load because of it.
fn read_bridge_object(
    path: &Path,
    errors: &mut Vec<RegistryError>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => {
            errors.push(defaults_error(
                path,
                FieldError::file_level(format!("cannot read file: {e}")),
            ));
            return None;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            errors.push(defaults_error(
                path,
                FieldError::file_level(format!("invalid JSON: {e}")),
            ));
            return None;
        }
    };
    match value.get("bridge") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(map)) => Some(map.clone()),
        Some(_) => {
            errors.push(defaults_error(
                path,
                FieldError::key("bridge", "must be an object"),
            ));
            None
        }
    }
}

impl Registry {
    /// Re-stat the five subdirs plus config.json; when any mtime changed,
    /// reload in place and return true. Called by the coordinator before
    /// dispatching each update, and by doctor.
    ///
    /// A full rebuild is deliberate: it is cheap (a handful of small files)
    /// and it cannot drift, which incremental reload always eventually does.
    pub fn reload_if_changed(&mut self) -> bool {
        if !self.changed_on_disk() {
            return false;
        }
        *self = self.rebuild();
        true
    }

    /// Load a fresh registry from the same root, replaying the same env
    /// layer, target roster and default target. This is what a caller that
    /// shares the registry behind an `Arc` (and therefore cannot take `&mut`)
    /// uses to produce the replacement snapshot.
    pub fn rebuild(&self) -> Registry {
        load_with_targets(&self.root, self.env.clone(), &self.targets, &self.default_target)
    }

    /// Six `stat` calls: has anything the loader watches changed since this
    /// registry was built? Pure — makes no change. Exposed so a caller that
    /// shares the registry behind an `Arc` (and therefore cannot take `&mut`)
    /// can still decide whether a rebuild is needed.
    pub fn changed_on_disk(&self) -> bool {
        stat_dir_mtimes(&self.root) != self.mtimes
    }

    /// The registry root (= config dir).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The env layer this registry was loaded with, so a caller rebuilding it
    /// from scratch reproduces the same precedence.
    pub fn env_source(&self) -> &EnvSource {
        &self.env
    }
}

/// Stat the five subdirectories' mtimes plus config.json's (missing or
/// unstattable -> None). config.json is included so an edit to its `bridge`
/// defaults object reloads on the same tick as a new commands/*.toml.
fn stat_dir_mtimes(root: &Path) -> DirMtimes {
    let m = |sub: &str| -> Option<SystemTime> {
        std::fs::metadata(root.join(sub))
            .ok()
            .and_then(|md| md.modified().ok())
    };
    DirMtimes {
        engines: m(ENGINES_DIR),
        agents: m(AGENTS_DIR),
        skills: m(SKILLS_DIR),
        commands: m(COMMANDS_DIR),
        prompts: m(PROMPTS_DIR),
        config_json: m(CONFIG_JSON),
    }
}

/// List `<dir>/<name>.toml` regular files as `(stem, path)`, sorted by stem.
/// Missing dir -> empty (silent); any other read_dir failure -> one
/// directory-level `Registry`-kind error. Dotfiles, non-`.toml` names and
/// non-files are skipped silently.
fn scan_toml_files(dir: &Path, errors: &mut Vec<RegistryError>) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            if e.kind() != ErrorKind::NotFound {
                errors.push(dir_error(dir, &e));
            }
            return out;
        }
    };
    for entry in rd.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name.starts_with('.')
            || std::path::Path::new(&file_name).extension() != Some(std::ffi::OsStr::new("toml"))
        {
            continue;
        }
        let stem = file_name[..file_name.len() - ".toml".len()].to_string();
        if stem.is_empty() {
            continue;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        out.push((stem, path));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// List `<dir>/<name>/` subdirectories as `(name, manifest_path, entry_dir)`,
/// sorted by name. A subdirectory without its manifest file records a
/// `missing <manifest>` error under the entity kind. Non-directories and
/// dotfiles are skipped silently; missing dir -> empty (silent).
fn scan_manifest_dirs(
    dir: &Path,
    manifest: &str,
    kind: RegistryEntityKind,
    errors: &mut Vec<RegistryError>,
) -> Vec<(String, PathBuf, PathBuf)> {
    let mut out: Vec<(String, PathBuf, PathBuf)> = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            if e.kind() != ErrorKind::NotFound {
                errors.push(dir_error(dir, &e));
            }
            return out;
        }
    };
    let mut names: Vec<(String, PathBuf)> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        names.push((name, path));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, entry_dir) in names {
        let manifest_path = entry_dir.join(manifest);
        if manifest_path.is_file() {
            out.push((name, manifest_path, entry_dir));
        } else {
            errors.push(RegistryError {
                kind,
                name,
                file: Some(manifest_path),
                message: format!("missing {manifest}"),
            });
        }
    }
    out
}

fn dir_error(dir: &Path, e: &std::io::Error) -> RegistryError {
    RegistryError {
        kind: RegistryEntityKind::Registry,
        name: String::new(),
        file: Some(dir.to_path_buf()),
        message: format!("cannot read directory: {e}"),
    }
}

/// Read + parse one TOML document; errors are collector-ready strings.
fn read_toml(path: &Path) -> Result<toml::Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read file: {e}"))?;
    text.parse::<toml::Value>()
        .map_err(|e| format!("invalid TOML: {e}"))
}

/// Drop agents whose engine, skill or prompt-template reference is unknown,
/// recording one error PER broken reference (an agent with a bad engine AND a
/// bad skill yields two errors, because fixing one at a time is miserable).
/// Remaining entries keep their relative order.
fn cross_reference_agents(
    agents: &mut IndexMap<String, AgentDef>,
    files: &IndexMap<String, PathBuf>,
    engine_names: &[String],
    skill_names: &[String],
    prompts: &PromptStore,
    errors: &mut Vec<RegistryError>,
) {
    let mut bad: Vec<String> = Vec::new();
    for (name, def) in agents.iter() {
        let mut errs: Vec<FieldError> = Vec::new();
        if !engine_names.iter().any(|e| e == &def.engine) {
            errs.push(
                FieldError::key("engine", format!("references unknown engine '{}'", def.engine))
                    .with_known(&known_slice(engine_names)),
            );
        }
        for skill in &def.skills {
            if !skill_names.iter().any(|s| s == skill) {
                errs.push(
                    FieldError::key("skills", format!("references unknown skill '{skill}'"))
                        .with_known(&known_slice(skill_names)),
                );
            }
        }
        if let Some(template) = &def.prompt_template {
            if !prompts.has(template) {
                errs.push(FieldError::key(
                    "prompt_template",
                    format!("references unknown prompt template '{template}'"),
                ));
            }
        }
        if !errs.is_empty() {
            push_all(RegistryEntityKind::Agent, name, files, errs, errors);
            bad.push(name.clone());
        }
    }
    for name in bad {
        agents.shift_remove(&name);
    }
}

/// Drop commands whose `agent`, `engine`, `skill`, `steps` or (kind=prompt)
/// `template` reference is unknown, one error per broken reference.
/// Remaining entries keep their relative order — including the shipped
/// commands, whose slots decide `/help` and keyboard layout.
fn cross_reference_commands(
    commands: &mut IndexMap<String, CommandDef>,
    files: &IndexMap<String, PathBuf>,
    agent_names: &[String],
    engine_names: &[String],
    skill_names: &[String],
    prompts: &PromptStore,
    errors: &mut Vec<RegistryError>,
) {
    // The step targets are checked against the table as it stands BEFORE any
    // drops, so two sequence commands referencing each other's siblings do
    // not cascade into a pile of confusing follow-on errors.
    let command_names: Vec<String> = commands.keys().cloned().collect();

    let mut bad: Vec<String> = Vec::new();
    for (name, def) in commands.iter() {
        let mut errs: Vec<FieldError> = Vec::new();
        if let Some(agent) = &def.agent {
            if !agent_names.iter().any(|a| a == agent) {
                errs.push(
                    FieldError::key("agent", format!("references unknown agent '{agent}'"))
                        .with_known(&known_slice(agent_names)),
                );
            }
        }
        if let Some(engine) = &def.engine {
            if !engine_names.iter().any(|e| e == engine) {
                errs.push(
                    FieldError::key("engine", format!("references unknown engine '{engine}'"))
                        .with_known(&known_slice(engine_names)),
                );
            }
        }
        if let Some(skill) = &def.skill {
            if !skill_names.iter().any(|s| s == skill) {
                errs.push(
                    FieldError::key("skill", format!("references unknown skill '{skill}'"))
                        .with_known(&known_slice(skill_names)),
                );
            }
        }
        if def.kind == CommandKind::Prompt {
            if let Some(template) = &def.template {
                if !prompts.has(template) {
                    errs.push(FieldError::key(
                        "template",
                        format!("references unknown prompt template '{template}'"),
                    ));
                }
            }
        }
        for step in &def.steps {
            if !command_names.iter().any(|c| c == step) {
                errs.push(
                    FieldError::key("steps", format!("references unknown command '{step}'"))
                        .with_known(&known_slice(&command_names)),
                );
            }
        }
        if !errs.is_empty() {
            push_all(RegistryEntityKind::Command, name, files, errs, errors);
            bad.push(name.clone());
        }
    }
    for name in bad {
        commands.shift_remove(&name);
    }
}

/// Record every cross-reference error for one entity, attaching its file.
fn push_all(
    kind: RegistryEntityKind,
    name: &str,
    files: &IndexMap<String, PathBuf>,
    errs: Vec<FieldError>,
    errors: &mut Vec<RegistryError>,
) {
    for err in errs {
        errors.push(RegistryError {
            kind,
            name: name.to_string(),
            file: files.get(name).cloned(),
            message: err.message(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn mk_agent(name: &str, engine: &str, skills: &[&str]) -> AgentDef {
        // AgentDef carries a private field (permission_mode_explicit), so it
        // is built through its constructor rather than a struct literal.
        AgentDef::new(name, Path::new("/nonexistent"), engine)
            .with_skills(skills.iter().map(|s| s.to_string()).collect())
    }

    fn mk_command(name: &str, kind: CommandKind, agent: Option<&str>, template: Option<&str>) -> CommandDef {
        CommandDef {
            command: name.to_string(),
            description: String::new(),
            kind,
            template: template.map(str::to_string),
            agent: agent.map(str::to_string),
            ..CommandDef::default()
        }
    }

    // ---- stat_dir_mtimes ----

    #[test]
    fn mtimes_missing_root_all_none() {
        let m = stat_dir_mtimes(Path::new("/nonexistent/stackhour-registry-test"));
        assert_eq!(m, DirMtimes::default());
    }

    #[test]
    fn mtimes_track_present_subdirs_only() {
        let dir = tmpdir();
        fs::create_dir(dir.path().join("engines")).unwrap();
        fs::create_dir(dir.path().join("prompts")).unwrap();
        let m = stat_dir_mtimes(dir.path());
        assert!(m.engines.is_some());
        assert!(m.prompts.is_some());
        assert!(m.agents.is_none());
        assert!(m.skills.is_none());
        assert!(m.commands.is_none());
        assert_ne!(m, DirMtimes::default());
    }

    #[test]
    fn mtimes_change_when_a_subdir_appears() {
        let dir = tmpdir();
        let before = stat_dir_mtimes(dir.path());
        fs::create_dir(dir.path().join("commands")).unwrap();
        let after = stat_dir_mtimes(dir.path());
        assert_ne!(before, after);
    }

    // ---- scan_toml_files ----

    #[test]
    fn scan_toml_files_sorts_and_filters() {
        let dir = tmpdir();
        fs::write(dir.path().join("b.toml"), "x = 1\n").unwrap();
        fs::write(dir.path().join("a.toml"), "x = 1\n").unwrap();
        fs::write(dir.path().join("notes.txt"), "no\n").unwrap();
        fs::write(dir.path().join(".hidden.toml"), "no\n").unwrap();
        // A directory named like a toml file is skipped (not a regular file).
        fs::create_dir(dir.path().join("d.toml")).unwrap();
        // A bare ".toml" has an empty stem and is skipped.
        fs::write(dir.path().join(".toml"), "no\n").unwrap();

        let mut errors = Vec::new();
        let entries = scan_toml_files(dir.path(), &mut errors);
        assert!(errors.is_empty());
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(entries[0].1, dir.path().join("a.toml"));
    }

    #[test]
    fn scan_toml_files_missing_dir_is_silent() {
        let dir = tmpdir();
        let mut errors = Vec::new();
        let entries = scan_toml_files(&dir.path().join("engines"), &mut errors);
        assert!(entries.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn scan_toml_files_unreadable_dir_records_registry_error() {
        let dir = tmpdir();
        // A regular FILE where the subdir should be -> read_dir fails with a
        // non-NotFound error -> one directory-level Registry error.
        let path = dir.path().join("engines");
        fs::write(&path, "not a dir\n").unwrap();
        let mut errors = Vec::new();
        let entries = scan_toml_files(&path, &mut errors);
        assert!(entries.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, RegistryEntityKind::Registry);
        assert_eq!(errors[0].name, "");
        assert_eq!(errors[0].file.as_deref(), Some(path.as_path()));
        assert!(errors[0].message.starts_with("cannot read directory:"));
    }

    // ---- scan_manifest_dirs ----

    #[test]
    fn scan_manifest_dirs_finds_manifests_and_flags_missing() {
        let dir = tmpdir();
        let agents = dir.path().join("agents");
        fs::create_dir_all(agents.join("zeta")).unwrap();
        fs::write(agents.join("zeta/agent.toml"), "engine = 'claude'\n").unwrap();
        fs::create_dir_all(agents.join("alpha")).unwrap();
        fs::write(agents.join("alpha/agent.toml"), "engine = 'claude'\n").unwrap();
        fs::create_dir_all(agents.join("broken")).unwrap(); // no manifest
        fs::create_dir_all(agents.join(".git")).unwrap(); // dotdir skipped
        fs::write(agents.join("stray.txt"), "no\n").unwrap(); // file skipped

        let mut errors = Vec::new();
        let entries = scan_manifest_dirs(&agents, AGENT_MANIFEST, RegistryEntityKind::Agent, &mut errors);
        let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        assert_eq!(entries[0].1, agents.join("alpha/agent.toml"));
        assert_eq!(entries[0].2, agents.join("alpha"));

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, RegistryEntityKind::Agent);
        assert_eq!(errors[0].name, "broken");
        assert_eq!(errors[0].message, "missing agent.toml");
        assert_eq!(
            errors[0].file.as_deref(),
            Some(agents.join("broken/agent.toml").as_path())
        );
    }

    #[test]
    fn scan_manifest_dirs_missing_dir_is_silent() {
        let dir = tmpdir();
        let mut errors = Vec::new();
        let entries = scan_manifest_dirs(
            &dir.path().join("skills"),
            SKILL_MANIFEST,
            RegistryEntityKind::Skill,
            &mut errors,
        );
        assert!(entries.is_empty());
        assert!(errors.is_empty());
    }

    // ---- read_toml ----

    #[test]
    fn read_toml_parses_valid_document() {
        let dir = tmpdir();
        let path = dir.path().join("ok.toml");
        fs::write(&path, "label = \"hi\"\n[tools]\nallow = [\"a\"]\n").unwrap();
        let v = read_toml(&path).expect("parse");
        assert_eq!(v.get("label").and_then(|x| x.as_str()), Some("hi"));
    }

    #[test]
    fn read_toml_invalid_document() {
        let dir = tmpdir();
        let path = dir.path().join("bad.toml");
        fs::write(&path, "= not toml\n").unwrap();
        let err = read_toml(&path).unwrap_err();
        assert!(err.starts_with("invalid TOML:"), "got: {err}");
    }

    #[test]
    fn read_toml_missing_file() {
        let err = read_toml(Path::new("/nonexistent/x.toml")).unwrap_err();
        assert!(err.starts_with("cannot read file:"), "got: {err}");
    }

    // ---- cross_reference_agents ----

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn no_prompts() -> PromptStore {
        PromptStore::new(None)
    }

    #[test]
    fn cross_ref_agents_drops_broken_keeps_valid_in_order() {
        let mut agents: IndexMap<String, AgentDef> = IndexMap::new();
        agents.insert("a".into(), mk_agent("a", "claude", &["s1"]));
        agents.insert("b".into(), mk_agent("b", "ghost", &[]));
        agents.insert("c".into(), mk_agent("c", "codex", &[]));
        let mut files: IndexMap<String, PathBuf> = IndexMap::new();
        files.insert("b".into(), PathBuf::from("/cfg/agents/b/agent.toml"));

        let mut errors = Vec::new();
        cross_reference_agents(
            &mut agents,
            &files,
            &names(&["claude", "codex"]),
            &names(&["s1"]),
            &no_prompts(),
            &mut errors,
        );

        let got: Vec<&str> = agents.keys().map(String::as_str).collect();
        assert_eq!(got, vec!["a", "c"]);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, RegistryEntityKind::Agent);
        assert_eq!(errors[0].name, "b");
        assert_eq!(
            errors[0].message,
            "key `engine`: references unknown engine 'ghost' (known: claude, codex)"
        );
        assert_eq!(
            errors[0].file.as_deref(),
            Some(Path::new("/cfg/agents/b/agent.toml"))
        );
    }

    #[test]
    fn cross_ref_agents_reports_every_broken_reference() {
        let mut agents: IndexMap<String, AgentDef> = IndexMap::new();
        agents.insert("x".into(), mk_agent("x", "ghost", &["s1", "s2"]));
        let files = IndexMap::new();

        let mut errors = Vec::new();
        cross_reference_agents(
            &mut agents,
            &files,
            &[],
            &names(&["s1"]),
            &no_prompts(),
            &mut errors,
        );

        assert!(agents.is_empty());
        let msgs: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            msgs,
            vec![
                "key `engine`: references unknown engine 'ghost'",
                "key `skills`: references unknown skill 's2' (known: s1)"
            ]
        );
        assert!(errors
            .iter()
            .all(|e| e.name == "x" && e.kind == RegistryEntityKind::Agent));
    }

    #[test]
    fn cross_ref_agents_validates_the_prompt_template_name() {
        let mut agents: IndexMap<String, AgentDef> = IndexMap::new();
        let mut def = mk_agent("x", "claude", &[]);
        def.prompt_template = Some("nope".into());
        agents.insert("x".into(), def);
        let mut errors = Vec::new();
        cross_reference_agents(
            &mut agents,
            &IndexMap::new(),
            &names(&["claude"]),
            &[],
            &no_prompts(),
            &mut errors,
        );
        assert!(agents.is_empty());
        assert_eq!(
            errors[0].message,
            "key `prompt_template`: references unknown prompt template 'nope'"
        );

        // A BUILT-IN template name passes.
        let mut agents: IndexMap<String, AgentDef> = IndexMap::new();
        let mut def = mk_agent("x", "claude", &[]);
        def.prompt_template = Some("system".into());
        agents.insert("x".into(), def);
        let mut errors = Vec::new();
        cross_reference_agents(
            &mut agents,
            &IndexMap::new(),
            &names(&["claude"]),
            &[],
            &no_prompts(),
            &mut errors,
        );
        assert_eq!(agents.len(), 1);
        assert!(errors.is_empty());
    }

    // ---- cross_reference_commands ----

    #[test]
    fn cross_ref_commands_validates_agent_and_template() {
        let mut commands: IndexMap<String, CommandDef> = IndexMap::new();
        commands.insert(
            "ok".into(),
            mk_command("ok", CommandKind::Prompt, None, Some("system")),
        );
        commands.insert(
            "badagent".into(),
            mk_command("badagent", CommandKind::Agent, Some("ghost"), None),
        );
        commands.insert(
            "badtpl".into(),
            mk_command("badtpl", CommandKind::Prompt, None, Some("nope")),
        );
        // Non-prompt kinds never have their template cross-checked.
        commands.insert(
            "shellish".into(),
            mk_command("shellish", CommandKind::Shell, None, Some("nope")),
        );

        let mut errors = Vec::new();
        cross_reference_commands(
            &mut commands,
            &IndexMap::new(),
            &names(&["reviewer"]),
            &names(&["claude"]),
            &[],
            &no_prompts(),
            &mut errors,
        );

        let got: Vec<&str> = commands.keys().map(String::as_str).collect();
        assert_eq!(got, vec!["ok", "shellish"]);
        let msgs: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            msgs,
            vec![
                "key `agent`: references unknown agent 'ghost' (known: reviewer)",
                "key `template`: references unknown prompt template 'nope'"
            ]
        );
        assert!(errors.iter().all(|e| e.kind == RegistryEntityKind::Command));
    }

    #[test]
    fn cross_ref_commands_validates_engine_skill_and_steps() {
        let mut commands: IndexMap<String, CommandDef> = IndexMap::new();
        commands.insert(
            "a".into(),
            CommandDef {
                command: "a".into(),
                engine: Some("ghost".into()),
                skill: Some("nope".into()),
                steps: names(&["missing"]),
                ..CommandDef::default()
            },
        );
        let mut errors = Vec::new();
        cross_reference_commands(
            &mut commands,
            &IndexMap::new(),
            &[],
            &names(&["claude"]),
            &names(&["review"]),
            &no_prompts(),
            &mut errors,
        );
        assert!(commands.is_empty());
        let msgs: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            msgs,
            vec![
                "key `engine`: references unknown engine 'ghost' (known: claude)",
                "key `skill`: references unknown skill 'nope' (known: review)",
                "key `steps`: references unknown command 'missing' (known: a)"
            ]
        );
    }

    #[test]
    fn cross_ref_commands_known_agent_passes() {
        let mut commands: IndexMap<String, CommandDef> = IndexMap::new();
        commands.insert(
            "review".into(),
            mk_command("review", CommandKind::Prompt, Some("reviewer"), Some("system")),
        );
        let mut errors = Vec::new();
        cross_reference_commands(
            &mut commands,
            &IndexMap::new(),
            &names(&["reviewer"]),
            &names(&["claude"]),
            &[],
            &no_prompts(),
            &mut errors,
        );
        assert_eq!(commands.len(), 1);
        assert!(errors.is_empty());
    }

    // ---- merge precedence (IndexMap semantics load() relies on) ----

    #[test]
    fn overriding_insert_replaces_value_keeps_position() {
        // load() inserts built-ins first, then directory files under the same
        // key: the value must be replaced while the entry keeps its slot.
        let mut m: IndexMap<String, i32> = IndexMap::new();
        m.insert("claude".into(), 1);
        m.insert("codex".into(), 2);
        m.insert("claude".into(), 99); // user engines/claude.toml override
        assert_eq!(m.get("claude"), Some(&99));
        let keys: Vec<&str> = m.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["claude", "codex"]);
    }

    // ---------------------------------------------------------------
    // Layer 1 only: the BACKWARD-COMPATIBILITY GATE.
    //
    // A user with no config directory (or an empty one, or one holding
    // only the legacy config.json) must get exactly the shipped bridge.
    // ---------------------------------------------------------------

    #[test]
    fn a_missing_config_dir_yields_the_shipped_bridge_and_no_errors() {
        let reg = load(Path::new("/nonexistent/stackhour-config-dir"));
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        assert!(reg.agents.is_empty());
        assert!(reg.skills.is_empty());
        assert_eq!(reg.engines.keys().collect::<Vec<_>>(), vec!["claude", "codex"]);
        assert_eq!(reg.defaults, ResolvedDefaults::default());
    }

    #[test]
    fn an_empty_config_dir_is_indistinguishable_from_a_missing_one() {
        let dir = tmpdir();
        let empty = load_with(dir.path(), EnvSource::fixed(&[]));
        let missing = load_with(
            Path::new("/nonexistent/stackhour-config-dir"),
            EnvSource::fixed(&[]),
        );
        assert!(empty.errors.is_empty(), "{:?}", empty.errors);
        assert_eq!(
            empty.commands.keys().collect::<Vec<_>>(),
            missing.commands.keys().collect::<Vec<_>>()
        );
        assert_eq!(empty.defaults, missing.defaults);
    }

    #[test]
    fn the_shipped_command_table_is_reproduced_exactly_by_an_empty_dir() {
        let dir = tmpdir();
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        let shipped = command::builtin_commands();
        assert_eq!(
            reg.commands.keys().collect::<Vec<_>>(),
            shipped.keys().collect::<Vec<_>>(),
            "the shipped verbs, and their ORDER, decide /help and the keyboard"
        );
        for (name, def) in &shipped {
            assert_eq!(
                reg.commands[name].description, def.description,
                "description drift on /{name}"
            );
        }
    }

    #[test]
    fn the_built_in_engines_are_byte_identical_to_the_code_definitions() {
        let dir = tmpdir();
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        let vars = engine::ArgvVars {
            session_id: Some("sid"),
            model: Some("m"),
            live_status: true,
            ..Default::default()
        };
        assert_eq!(
            reg.engines["claude"].assemble_argv(&vars),
            engine::builtin_claude().assemble_argv(&vars)
        );
        assert_eq!(
            reg.engines["codex"].assemble_argv(&vars),
            engine::builtin_codex().assemble_argv(&vars)
        );
    }

    #[test]
    fn a_legacy_config_json_without_a_bridge_object_changes_nothing() {
        let dir = tmpdir();
        fs::write(
            dir.path().join("config.json"),
            r#"{"apiUrl":"https://example.test","token":"secret"}"#,
        )
        .unwrap();
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        assert_eq!(reg.defaults, ResolvedDefaults::default());
    }

    // ---------------------------------------------------------------
    // Layer 3: a valid custom entity takes effect.
    // ---------------------------------------------------------------

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn a_custom_engine_agent_skill_and_command_all_take_effect() {
        let dir = tmpdir();
        write(
            dir.path(),
            "engines/ollama.toml",
            "bin = \"ollama\"\nlabel = \"Ollama\"\nkind = \"plain-lines\"\nargs = [\"run\", \"-\"]\n",
        );
        write(
            dir.path(),
            "skills/review/skill.toml",
            "description = \"Review carefully\"\n",
        );
        write(dir.path(), "skills/review/skill.md", "Read before you write.\n");
        write(
            dir.path(),
            "agents/reviewer/agent.toml",
            "label = \"Reviewer\"\nengine = \"ollama\"\nskills = [\"review\"]\n",
        );
        write(dir.path(), "agents/reviewer/soul.md", "Be terse.\n");
        write(dir.path(), "prompts/deploy.md", "Deploy to {{env}}.\n");
        write(
            dir.path(),
            "commands/deploy.toml",
            "description = \"Deploy\"\nkind = \"prompt\"\ntemplate = \"deploy\"\nagent = \"reviewer\"\n",
        );

        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        assert_eq!(reg.engines["ollama"].label, "Ollama");
        assert_eq!(reg.agents["reviewer"].engine, "ollama");
        assert_eq!(reg.skills["review"].description, "Review carefully");
        assert_eq!(reg.commands["deploy"].description, "Deploy");
        // A prompt that shadows NO built-in is still usable by a command.
        assert!(reg.prompts.has("deploy"));
        assert_eq!(
            reg.prompts.render("deploy", &[("env", "prod")]),
            "Deploy to prod.\n"
        );
    }

    #[test]
    fn a_user_engine_replaces_the_built_in_and_keeps_its_slot() {
        let dir = tmpdir();
        write(
            dir.path(),
            "engines/claude.toml",
            "bin = \"my-claude\"\nlabel = \"Mine\"\nkind = \"plain-lines\"\nargs = [\"-\"]\n",
        );
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        assert_eq!(reg.engines["claude"].bin, "my-claude");
        assert_eq!(
            reg.engines.keys().collect::<Vec<_>>(),
            vec!["claude", "codex"],
            "an override must not move the entry"
        );
    }

    #[test]
    fn a_user_prompt_overrides_the_built_in_of_the_same_name() {
        let dir = tmpdir();
        write(dir.path(), "prompts/help.md", "MY HELP\n");
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert_eq!(reg.prompts.render("help", &[]), "MY HELP\n");
    }

    // ---------------------------------------------------------------
    // Layer 3: an invalid entity produces the right error and is SKIPPED,
    // without taking anything else down.
    // ---------------------------------------------------------------

    #[test]
    fn an_agent_naming_an_unknown_engine_is_dropped_with_a_file_and_key() {
        let dir = tmpdir();
        write(dir.path(), "agents/reviewer/agent.toml", "engine = \"gpt5\"\n");
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert!(reg.agents.is_empty(), "a broken agent must be skipped");
        assert_eq!(reg.errors.len(), 1);
        let err = &reg.errors[0];
        assert_eq!(err.kind, RegistryEntityKind::Agent);
        assert_eq!(err.name, "reviewer");
        assert_eq!(
            err.file.as_deref(),
            Some(dir.path().join("agents/reviewer/agent.toml").as_path())
        );
        assert_eq!(
            err.message,
            "key `engine`: references unknown engine 'gpt5' (known: claude, codex)"
        );
        // ...and the rest of the registry is intact.
        assert_eq!(reg.engines.len(), 2);
    }

    #[test]
    fn a_malformed_toml_file_is_skipped_not_fatal() {
        let dir = tmpdir();
        write(dir.path(), "engines/bad.toml", "= not toml\n");
        write(
            dir.path(),
            "engines/good.toml",
            "bin = \"g\"\nkind = \"plain-lines\"\n",
        );
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert!(
            reg.engines.contains_key("good"),
            "one bad file must not stop the scan"
        );
        assert!(!reg.engines.contains_key("bad"));
        assert_eq!(reg.errors.len(), 1);
        assert_eq!(reg.errors[0].name, "bad");
        assert!(reg.errors[0].message.starts_with("invalid TOML:"));
    }

    #[test]
    fn a_skill_dir_without_its_manifest_is_reported_by_name() {
        let dir = tmpdir();
        fs::create_dir_all(dir.path().join("skills/halfdone")).unwrap();
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert_eq!(reg.errors.len(), 1);
        assert_eq!(reg.errors[0].kind, RegistryEntityKind::Skill);
        assert_eq!(reg.errors[0].name, "halfdone");
        assert_eq!(reg.errors[0].message, "missing skill.toml");
    }

    // ---------------------------------------------------------------
    // Layer 2: the "bridge" object in config.json.
    // ---------------------------------------------------------------

    #[test]
    fn the_bridge_object_sets_the_scalar_defaults() {
        let dir = tmpdir();
        write(dir.path(), "agents/reviewer/agent.toml", "engine = \"claude\"\n");
        write(dir.path(), "agents/reviewer/soul.md", "x\n");
        fs::write(
            dir.path().join("config.json"),
            r#"{"token":"t","bridge":{"defaultAgent":"reviewer","defaultEngine":"codex","defaultTarget":"mac"}}"#,
        )
        .unwrap();
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        assert_eq!(reg.defaults.agent.as_deref(), Some("reviewer"));
        assert_eq!(reg.defaults.engine, "codex");
        assert_eq!(reg.defaults.target, "mac");
    }

    #[test]
    fn an_unknown_default_is_reported_and_falls_back_rather_than_failing() {
        let dir = tmpdir();
        fs::write(
            dir.path().join("config.json"),
            r#"{"bridge":{"defaultAgent":"ghost","defaultEngine":"nope","defaultTarget":"moon"}}"#,
        )
        .unwrap();
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        let msgs: Vec<&str> = reg.errors.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            msgs,
            vec![
                "key `bridge.defaultAgent`: references unknown agent 'ghost'",
                "key `bridge.defaultEngine`: references unknown engine 'nope' (known: claude, codex)",
                "key `bridge.defaultTarget`: must be one of \"gcp\", \"mac\" (got 'moon')",
            ]
        );
        assert!(reg
            .errors
            .iter()
            .all(|e| e.file.as_deref() == Some(dir.path().join("config.json").as_path())));
        assert_eq!(reg.defaults, ResolvedDefaults::default());
    }

    #[test]
    fn a_malformed_config_json_is_reported_and_treated_as_absent() {
        let dir = tmpdir();
        fs::write(dir.path().join("config.json"), "{not json").unwrap();
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert_eq!(reg.errors.len(), 1);
        assert!(reg.errors[0].message.starts_with("invalid JSON:"));
        assert_eq!(reg.defaults, ResolvedDefaults::default());
    }

    #[test]
    fn a_non_object_bridge_key_is_reported() {
        let dir = tmpdir();
        fs::write(dir.path().join("config.json"), r#"{"bridge":"nope"}"#).unwrap();
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert_eq!(reg.errors[0].message, "key `bridge`: must be an object");
    }

    #[test]
    fn a_wrong_typed_default_is_reported_with_its_dotted_key() {
        let dir = tmpdir();
        fs::write(
            dir.path().join("config.json"),
            r#"{"bridge":{"defaultTarget":7}}"#,
        )
        .unwrap();
        let reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert_eq!(
            reg.errors[0].message,
            "key `bridge.defaultTarget`: must be a non-empty string"
        );
    }

    // ---------------------------------------------------------------
    // Layer 4: env wins over config.json.
    // ---------------------------------------------------------------

    #[test]
    fn env_overrides_the_config_json_defaults() {
        let dir = tmpdir();
        fs::write(
            dir.path().join("config.json"),
            r#"{"bridge":{"defaultEngine":"claude","defaultTarget":"gcp"}}"#,
        )
        .unwrap();
        let reg = load_with(
            dir.path(),
            EnvSource::fixed(&[("STACKHOUR_ENGINE", "codex"), ("STACKHOUR_TARGET", "mac")]),
        );
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        assert_eq!(reg.defaults.engine, "codex");
        assert_eq!(reg.defaults.target, "mac");
    }

    #[test]
    fn an_empty_env_var_falls_through_to_the_lower_layer() {
        let dir = tmpdir();
        fs::write(
            dir.path().join("config.json"),
            r#"{"bridge":{"defaultTarget":"mac"}}"#,
        )
        .unwrap();
        let reg = load_with(dir.path(), EnvSource::fixed(&[("STACKHOUR_TARGET", "  ")]));
        assert_eq!(reg.defaults.target, "mac");
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
    }

    #[test]
    fn a_bad_env_default_is_reported_against_the_env_pseudo_file() {
        let dir = tmpdir();
        let reg = load_with(dir.path(), EnvSource::fixed(&[("STACKHOUR_TARGET", "moon")]));
        assert_eq!(reg.errors.len(), 1);
        assert_eq!(reg.errors[0].file.as_deref(), Some(Path::new("<env>")));
        assert_eq!(
            reg.errors[0].message,
            "key `STACKHOUR_TARGET`: must be one of \"gcp\", \"mac\" (got 'moon')"
        );
        assert_eq!(reg.defaults.target, DEFAULT_TARGET);
    }

    // ---------------------------------------------------------------
    // The target roster (DELIBERATE DIVERGENCE from the Node bridge:
    // targets are no longer the hardcoded gcp+mac pair).
    // ---------------------------------------------------------------

    fn moon_sun() -> Vec<TargetSpec> {
        vec![
            TargetSpec::new("moon", "Moonbase", "local"),
            TargetSpec::new("sun", "Sunspot", "worker"),
        ]
    }

    #[test]
    fn a_custom_roster_generates_its_switch_commands_and_default() {
        let dir = tmpdir();
        let reg = load_with_targets(dir.path(), EnvSource::fixed(&[]), &moon_sun(), "moon");
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        assert_eq!(
            reg.commands.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["claude", "codex", "moon", "sun", "ship", "where", "new", "stop", "menu", "help"]
        );
        assert_eq!(reg.commands["moon"].kind, CommandKind::Target);
        assert_eq!(reg.commands["moon"].button_order, Some(20));
        assert_eq!(reg.commands["sun"].button_order, Some(21));
        assert_eq!(reg.defaults.target, "moon");
        assert_eq!(reg.targets, moon_sun());
        // The generated help lists the roster, not the legacy pair.
        let help = reg.prompts.render("help", &[]);
        assert!(help.contains("☁️ /moon — run on Moonbase"), "got: {help}");
        assert!(help.contains("🖥️ /sun — run on Sunspot"), "got: {help}");
        assert!(!help.contains("/gcp"), "got: {help}");
    }

    #[test]
    fn a_custom_roster_validates_the_default_target_against_itself() {
        let dir = tmpdir();
        fs::write(
            dir.path().join("config.json"),
            r#"{"bridge":{"defaultTarget":"sun"}}"#,
        )
        .unwrap();
        let reg = load_with_targets(dir.path(), EnvSource::fixed(&[]), &moon_sun(), "moon");
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        assert_eq!(reg.defaults.target, "sun");

        // An unknown name is reported against the roster and falls back.
        let reg = load_with_targets(
            dir.path(),
            EnvSource::fixed(&[("STACKHOUR_TARGET", "gcp")]),
            &moon_sun(),
            "moon",
        );
        assert_eq!(reg.errors.len(), 1);
        assert_eq!(
            reg.errors[0].message,
            "key `STACKHOUR_TARGET`: must be one of \"moon\", \"sun\" (got 'gcp')"
        );
        assert_eq!(reg.defaults.target, "sun", "falls back to the config.json layer");
    }

    #[test]
    fn a_custom_roster_target_command_names_pass_command_validation() {
        let dir = tmpdir();
        write(
            dir.path(),
            "commands/warp.toml",
            "description = \"Warp\"\nkind = \"target\"\ntarget = \"sun\"\n",
        );
        let reg = load_with_targets(dir.path(), EnvSource::fixed(&[]), &moon_sun(), "moon");
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        assert_eq!(reg.commands["warp"].target.as_deref(), Some("sun"));
    }

    #[test]
    fn every_roster_switch_command_is_guaranteed_restored() {
        // A broken user override of a GENERATED switch command must revert to
        // the generated one, exactly as /gcp used to.
        let dir = tmpdir();
        write(
            dir.path(),
            "commands/moon.toml",
            "description = \"Mine\"\nkind = \"prompt\"\ntemplate = \"no-such-template\"\n",
        );
        let reg = load_with_targets(dir.path(), EnvSource::fixed(&[]), &moon_sun(), "moon");
        assert_eq!(reg.errors.len(), 1, "{:?}", reg.errors);
        let moon = &reg.commands["moon"];
        assert_eq!(moon.kind, CommandKind::Target, "the generated command is back");
        assert_eq!(moon.target.as_deref(), Some("moon"));
        assert_eq!(reg.commands.get_index_of("moon"), Some(2), "…in its shipped slot");
    }

    #[test]
    fn reload_replays_the_same_target_roster() {
        let dir = tmpdir();
        let mut reg = load_with_targets(dir.path(), EnvSource::fixed(&[]), &moon_sun(), "moon");
        write(
            dir.path(),
            "engines/ollama.toml",
            "bin = \"ollama\"\nkind = \"plain-lines\"\n",
        );
        assert!(reg.reload_if_changed());
        assert!(reg.engines.contains_key("ollama"));
        assert_eq!(reg.targets, moon_sun(), "the roster must survive reload");
        assert_eq!(reg.defaults.target, "moon");
        assert!(reg.commands.contains_key("sun"));
    }

    // ---------------------------------------------------------------
    // Reload.
    // ---------------------------------------------------------------

    #[test]
    fn reload_is_a_no_op_when_nothing_changed() {
        let dir = tmpdir();
        let mut reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert!(!reg.reload_if_changed());
    }

    #[test]
    fn reload_picks_up_a_new_entity_file() {
        let dir = tmpdir();
        let mut reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert!(!reg.engines.contains_key("ollama"));
        write(
            dir.path(),
            "engines/ollama.toml",
            "bin = \"ollama\"\nkind = \"plain-lines\"\n",
        );
        assert!(reg.reload_if_changed());
        assert!(reg.engines.contains_key("ollama"));
    }

    #[test]
    fn reload_picks_up_an_edit_to_the_config_json_bridge_object() {
        let dir = tmpdir();
        fs::write(dir.path().join("config.json"), r#"{"bridge":{}}"#).unwrap();
        let mut reg = load_with(dir.path(), EnvSource::fixed(&[]));
        assert_eq!(reg.defaults.target, DEFAULT_TARGET);

        // Rewrite with a mtime far enough in the future that a coarse
        // filesystem clock cannot hide the change.
        fs::write(
            dir.path().join("config.json"),
            r#"{"bridge":{"defaultTarget":"mac"}}"#,
        )
        .unwrap();
        let future = SystemTime::now() + std::time::Duration::from_secs(5);
        let f = fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("config.json"))
            .unwrap();
        f.set_times(fs::FileTimes::new().set_modified(future)).unwrap();

        assert!(reg.reload_if_changed(), "config.json must be watched");
        assert_eq!(reg.defaults.target, "mac");
    }

    #[test]
    fn reload_replays_the_same_env_layer() {
        let dir = tmpdir();
        let mut reg = load_with(dir.path(), EnvSource::fixed(&[("STACKHOUR_TARGET", "mac")]));
        assert_eq!(reg.defaults.target, "mac");
        write(
            dir.path(),
            "engines/ollama.toml",
            "bin = \"ollama\"\nkind = \"plain-lines\"\n",
        );
        assert!(reg.reload_if_changed());
        assert_eq!(reg.defaults.target, "mac", "the env layer must survive reload");
    }

    // ---------------------------------------------------------------
    // Cycle detection, wired through the loader.
    // ---------------------------------------------------------------

    #[test]
    fn a_command_sequence_cycle_drops_both_commands() {
        let mut commands: IndexMap<String, CommandDef> = IndexMap::new();
        for (name, step) in [("a", "b"), ("b", "a")] {
            commands.insert(
                name.into(),
                CommandDef {
                    command: name.into(),
                    kind: CommandKind::Sequence,
                    steps: vec![step.to_string()],
                    ..CommandDef::default()
                },
            );
        }
        let mut errors = Vec::new();
        apply_cycle_check(
            &mut commands,
            command_steps,
            "commands",
            "steps",
            RegistryEntityKind::Command,
            &IndexMap::new(),
            &mut errors,
        );
        assert!(commands.is_empty());
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].message, "key `steps`: commands: cycle a -> b -> a");
    }
}
