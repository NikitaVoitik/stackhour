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
pub mod command;
pub mod engine;
pub mod prompt;
pub mod skill;

pub use agent_def::{AgentDef, Soul, ToolPolicy};
pub use command::{CommandDef, CommandKind, RESERVED};
pub use engine::{EngineDef, PromptDelivery, ResumeStyle, StreamKind};
pub use prompt::PromptStore;
pub use skill::SkillDef;

const ENGINES_DIR: &str = "engines";
const AGENTS_DIR: &str = "agents";
const SKILLS_DIR: &str = "skills";
const COMMANDS_DIR: &str = "commands";
const PROMPTS_DIR: &str = "prompts";
const AGENT_MANIFEST: &str = "agent.toml";
const SKILL_MANIFEST: &str = "skill.toml";

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

/// Snapshot of the five subdir mtimes used for cheap hot-reload detection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DirMtimes {
    pub(crate) engines: Option<SystemTime>,
    pub(crate) agents: Option<SystemTime>,
    pub(crate) skills: Option<SystemTime>,
    pub(crate) commands: Option<SystemTime>,
    pub(crate) prompts: Option<SystemTime>,
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
    /// Registry root (= config_dir), kept for reloads.
    pub(crate) root: PathBuf,
    /// Subdir mtimes at load time, for `reload_if_changed`.
    pub(crate) mtimes: DirMtimes,
}

/// Load the registry from `config_dir`. Never fails: bad files become
/// `errors` entries; a missing directory yields built-ins only.
pub fn load(config_dir: &Path) -> Registry {
    let root = config_dir.to_path_buf();
    // Stat BEFORE scanning: a change landing mid-scan yields differing
    // mtimes on the next `reload_if_changed` stat, so it is never missed.
    let mtimes = stat_dir_mtimes(&root);
    let mut errors: Vec<RegistryError> = Vec::new();

    // --- Engines: built-ins first; directory files override by name. ---
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

    // --- Skills (no built-ins). ---
    let mut skills: IndexMap<String, SkillDef> = IndexMap::new();
    for (name, manifest, dir) in scan_manifest_dirs(
        &root.join(SKILLS_DIR),
        SKILL_MANIFEST,
        RegistryEntityKind::Skill,
        &mut errors,
    ) {
        match read_toml(&manifest).and_then(|v| SkillDef::from_toml(&name, &dir, &v)) {
            Ok(def) => {
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

    // --- Agents (no built-ins); cross-ref validated against engines+skills. ---
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
    cross_reference_agents(
        &mut agents,
        &agent_files,
        |e| engines.contains_key(e),
        |s| skills.contains_key(s),
        &mut errors,
    );

    // --- Prompts: built-ins + optional on-disk overrides. ---
    let prompts = PromptStore::new(if root.is_dir() {
        Some(root.join(PROMPTS_DIR))
    } else {
        None
    });

    // --- Commands: user files only (built-in verbs are hardcoded in the
    // coordinator and their RESERVED names are rejected by from_toml). ---
    let mut commands: IndexMap<String, CommandDef> = IndexMap::new();
    let mut command_files: IndexMap<String, PathBuf> = IndexMap::new();
    for (name, path) in scan_toml_files(&root.join(COMMANDS_DIR), &mut errors) {
        match read_toml(&path).and_then(|v| CommandDef::from_toml(&name, &v)) {
            Ok(def) => {
                command_files.insert(name.clone(), path);
                commands.insert(name, def);
            }
            Err(message) => errors.push(RegistryError {
                kind: RegistryEntityKind::Command,
                name,
                file: Some(path),
                message,
            }),
        }
    }
    cross_reference_commands(
        &mut commands,
        &command_files,
        |a| agents.contains_key(a),
        |t| prompts.has(t),
        &mut errors,
    );

    Registry {
        engines,
        agents,
        skills,
        commands,
        prompts,
        errors,
        root,
        mtimes,
    }
}

impl Registry {
    /// Re-stat the five subdirs; when any mtime changed, reload in place and
    /// return true. Called by the coordinator before dispatching each update
    /// and by doctor.
    pub fn reload_if_changed(&mut self) -> bool {
        let current = stat_dir_mtimes(&self.root);
        if current == self.mtimes {
            return false;
        }
        let root = self.root.clone();
        *self = load(&root);
        true
    }
}

/// Stat the five subdirectories' mtimes (missing/unstattable -> None).
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
        if file_name.starts_with('.') || !file_name.ends_with(".toml") {
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

/// Drop agents whose engine or any skill reference is unknown, recording one
/// error PER broken reference (an agent with a bad engine AND a bad skill
/// yields two errors). Remaining entries keep their relative order.
fn cross_reference_agents<FE, FS>(
    agents: &mut IndexMap<String, AgentDef>,
    files: &IndexMap<String, PathBuf>,
    engine_exists: FE,
    skill_exists: FS,
    errors: &mut Vec<RegistryError>,
) where
    FE: Fn(&str) -> bool,
    FS: Fn(&str) -> bool,
{
    let mut bad: Vec<String> = Vec::new();
    for (name, def) in agents.iter() {
        let mut msgs: Vec<String> = Vec::new();
        if !engine_exists(&def.engine) {
            msgs.push(format!("references unknown engine '{}'", def.engine));
        }
        for skill in &def.skills {
            if !skill_exists(skill) {
                msgs.push(format!("references unknown skill '{skill}'"));
            }
        }
        if !msgs.is_empty() {
            for message in msgs {
                errors.push(RegistryError {
                    kind: RegistryEntityKind::Agent,
                    name: name.clone(),
                    file: files.get(name).cloned(),
                    message,
                });
            }
            bad.push(name.clone());
        }
    }
    for name in bad {
        agents.shift_remove(&name);
    }
}

/// Drop commands whose `agent` reference or (kind=prompt) `template`
/// reference is unknown, one error per broken reference. Remaining entries
/// keep their relative order.
fn cross_reference_commands<FA, FT>(
    commands: &mut IndexMap<String, CommandDef>,
    files: &IndexMap<String, PathBuf>,
    agent_exists: FA,
    template_exists: FT,
    errors: &mut Vec<RegistryError>,
) where
    FA: Fn(&str) -> bool,
    FT: Fn(&str) -> bool,
{
    let mut bad: Vec<String> = Vec::new();
    for (name, def) in commands.iter() {
        let mut msgs: Vec<String> = Vec::new();
        if let Some(agent) = &def.agent {
            if !agent_exists(agent) {
                msgs.push(format!("references unknown agent '{agent}'"));
            }
        }
        if def.kind == CommandKind::Prompt {
            if let Some(template) = &def.template {
                if !template_exists(template) {
                    msgs.push(format!("references unknown prompt template '{template}'"));
                }
            }
        }
        if !msgs.is_empty() {
            for message in msgs {
                errors.push(RegistryError {
                    kind: RegistryEntityKind::Command,
                    name: name.clone(),
                    file: files.get(name).cloned(),
                    message,
                });
            }
            bad.push(name.clone());
        }
    }
    for name in bad {
        commands.shift_remove(&name);
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
        AgentDef {
            name: name.to_string(),
            label: name.to_string(),
            engine: engine.to_string(),
            model: None,
            soul: Soul::new(PathBuf::from("/nonexistent/soul.md")),
            skills: skills.iter().map(|s| s.to_string()).collect(),
            permission_mode: "default".to_string(),
            cwd: None,
            prompt_template: None,
            tools: ToolPolicy::default(),
        }
    }

    fn mk_command(name: &str, kind: CommandKind, agent: Option<&str>, template: Option<&str>) -> CommandDef {
        CommandDef {
            command: name.to_string(),
            description: String::new(),
            kind,
            template: template.map(str::to_string),
            agent: agent.map(str::to_string),
            engine: None,
            target: None,
            argv: None,
            confirm: false,
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
            |e| e == "claude" || e == "codex",
            |s| s == "s1",
            &mut errors,
        );

        let names: Vec<&str> = agents.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["a", "c"]);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, RegistryEntityKind::Agent);
        assert_eq!(errors[0].name, "b");
        assert_eq!(errors[0].message, "references unknown engine 'ghost'");
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
        cross_reference_agents(&mut agents, &files, |_| false, |s| s == "s1", &mut errors);

        assert!(agents.is_empty());
        let msgs: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            msgs,
            vec![
                "references unknown engine 'ghost'",
                "references unknown skill 's2'"
            ]
        );
        assert!(errors
            .iter()
            .all(|e| e.name == "x" && e.kind == RegistryEntityKind::Agent));
    }

    // ---- cross_reference_commands ----

    #[test]
    fn cross_ref_commands_validates_agent_and_template() {
        let mut commands: IndexMap<String, CommandDef> = IndexMap::new();
        commands.insert(
            "ok".into(),
            mk_command("ok", CommandKind::Prompt, None, Some("deploy")),
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
        let files = IndexMap::new();

        let mut errors = Vec::new();
        cross_reference_commands(
            &mut commands,
            &files,
            |a| a == "reviewer",
            |t| t == "deploy",
            &mut errors,
        );

        let names: Vec<&str> = commands.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["ok", "shellish"]);
        let msgs: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            msgs,
            vec![
                "references unknown agent 'ghost'",
                "references unknown prompt template 'nope'"
            ]
        );
        assert!(errors.iter().all(|e| e.kind == RegistryEntityKind::Command));
    }

    #[test]
    fn cross_ref_commands_known_agent_passes() {
        let mut commands: IndexMap<String, CommandDef> = IndexMap::new();
        commands.insert(
            "review".into(),
            mk_command("review", CommandKind::Prompt, Some("reviewer"), Some("deploy")),
        );
        let files = IndexMap::new();
        let mut errors = Vec::new();
        cross_reference_commands(&mut commands, &files, |a| a == "reviewer", |_| true, &mut errors);
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
}
