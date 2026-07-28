//! Skills — reusable CAPABILITY packs, not just prompt fragments.
//!
//! A skill bundles the five things a repeatable task needs: prose that goes
//! into the system prompt, an argument schema, a prompt template that turns
//! those arguments into the turn's user prompt, a tool/permission and env
//! policy, and pre/post shell hooks. Skills compose (`uses`) and may name a
//! default agent, so `/review pr 123` can be one file rather than one
//! hand-typed paragraph.
//!
//! `skills/<name>/skill.toml` schema (only `description` is required):
//!
//! ```toml
//! description = "How to review pull requests"  # required, non-empty
//! body = "skill.md"        # markdown body, relative to the skill dir
//!                          # (default "skill.md"; absolute paths allowed)
//! agent = "reviewer"       # default agent for this skill (cross-ref checked)
//! template = "review"      # prompt template rendered with the bound args to
//!                          # form the turn's user prompt. Absent = the raw
//!                          # argument string is used verbatim.
//! uses = ["git-hygiene"]   # skills composed into this one; cycle-detected
//!
//! [[args]]                 # same ArgSpec as commands (see `args.rs`)
//! name = "pr"
//! required = true
//! description = "pull request number"
//!
//! [tools]                  # optional additions merged into the agent's
//! allow = ["Bash"]         # ToolPolicy for the engine spawn
//! deny = ["WebSearch"]
//!
//! [env]                    # optional env vars merged into the spawn
//! REVIEW_MODE = "strict"
//!
//! [hooks]                  # FIXED argv, NEVER shell-interpolated
//! pre = ["gh", "pr", "checkout", "{{arg:pr}}"]
//! post = ["git", "checkout", "-"]
//! timeout_seconds = 60
//! ```
//!
//! Hook argv placeholders are substituted PER ARGV ELEMENT — `{{arg:<name>}}`,
//! `{{skill}}`, `{{agent}}`, `{{cwd}}` — so a value containing spaces or shell
//! metacharacters stays one argument and can never be re-parsed as a command.
//! A `pre` hook exiting non-zero aborts the turn and its stderr is surfaced to
//! the chat; a `post` hook exiting non-zero is logged only.
//!
//! Unknown keys are ignored (forward compatibility). Validation errors are
//! [`FieldError`]s naming the key, collected into `Registry::errors` by the
//! loader — a broken skill.toml is skipped, never fatal.
//!
//! BACKWARD COMPATIBILITY: every key added here is optional and every default
//! reproduces the previous behaviour exactly. A skill.toml with only
//! `description` (or no skills dir at all) behaves byte-for-byte as before.

use indexmap::{IndexMap, IndexSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use super::agent_def::ToolPolicy;
use super::args::{self, ArgSpec};
use super::cycle::MAX_DEPTH;
use super::error::FieldError;
use super::toml_util::{self, Table};
use super::Registry;

const DEFAULT_BODY_FILE: &str = "skill.md";
/// Hooks are killed after this many seconds unless `timeout_seconds` says
/// otherwise. Sixty seconds is long enough for a checkout or a test shard and
/// short enough that a hung hook does not wedge the chat.
pub const DEFAULT_HOOK_TIMEOUT_SECONDS: u64 = 60;

/// Pre/post shell hooks for a skill. Both are FIXED argv (never a shell
/// string), so there is no quoting or injection surface at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillHooks {
    /// Runs BEFORE the turn. Non-zero exit aborts the turn.
    pub pre: Vec<String>,
    /// Runs AFTER the turn. Non-zero exit is logged, not fatal.
    pub post: Vec<String>,
    /// Wall-clock cap per hook.
    pub timeout_seconds: u64,
}

impl SkillHooks {
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }

    /// Parse the optional `[hooks]` table.
    fn from_table(table: &Table) -> Result<Self, FieldError> {
        let Some(hooks) = toml_util::opt_table(table, "hooks")? else {
            return Ok(SkillHooks {
                pre: Vec::new(),
                post: Vec::new(),
                timeout_seconds: DEFAULT_HOOK_TIMEOUT_SECONDS,
            });
        };
        let argv = |key: &str| -> Result<Vec<String>, FieldError> {
            let list = toml_util::string_list(hooks, key).map_err(|e| e.under("hooks"))?;
            // An empty array is "no hook", but an argv whose PROGRAM is blank
            // would spawn nothing and is always a typo.
            if list.iter().any(|s| s.trim().is_empty()) {
                return Err(FieldError::nested(
                    "hooks",
                    key,
                    "argv entries must be non-empty strings",
                ));
            }
            Ok(list)
        };
        let timeout_seconds = toml_util::opt_u64(hooks, "timeout_seconds", DEFAULT_HOOK_TIMEOUT_SECONDS)
            .map_err(|e| e.under("hooks"))?;
        if timeout_seconds == 0 {
            return Err(FieldError::nested(
                "hooks",
                "timeout_seconds",
                "must be at least 1 second",
            ));
        }
        Ok(SkillHooks {
            pre: argv("pre")?,
            post: argv("post")?,
            timeout_seconds,
        })
    }
}

/// A skill definition (skills/<name>/skill.toml).
#[derive(Debug)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    /// Optional prose body (skill.md), re-read on mtime change at each use.
    pub body_path: Option<PathBuf>,
    /// Default agent for this skill (cross-ref checked by the loader).
    pub agent: Option<String>,
    /// Prompt template rendered with the bound args to form the user prompt
    /// (cross-ref checked by the loader).
    pub template: Option<String>,
    /// Skills composed into this one, in declaration order. Cycle-detected.
    pub uses: Vec<String>,
    pub tools: ToolPolicy,
    pub env: IndexMap<String, String>,
    /// Positional argument schema (empty = legacy raw-string behaviour).
    pub args: Vec<ArgSpec>,
    pub hooks: SkillHooks,
    /// Per-file mtime cache for `body()` (same discipline as `Soul`).
    body_cache: Mutex<Option<(SystemTime, String)>>,
}

impl SkillDef {
    /// Build a minimal skill programmatically (tests / embedded defaults).
    /// Use the `with_*` builders for the optional halves.
    pub fn new(
        name: String,
        description: String,
        body_path: Option<PathBuf>,
        tools: ToolPolicy,
        env: IndexMap<String, String>,
    ) -> Self {
        SkillDef {
            name,
            description,
            body_path,
            agent: None,
            template: None,
            uses: Vec::new(),
            tools,
            env,
            args: Vec::new(),
            hooks: SkillHooks {
                timeout_seconds: DEFAULT_HOOK_TIMEOUT_SECONDS,
                ..SkillHooks::default()
            },
            body_cache: Mutex::new(None),
        }
    }

    /// Builder: the skills this one composes in.
    #[must_use]
    pub fn with_uses(mut self, uses: Vec<String>) -> Self {
        self.uses = uses;
        self
    }

    /// Builder: default agent and prompt template.
    #[must_use]
    pub fn with_bindings(mut self, agent: Option<String>, template: Option<String>) -> Self {
        self.agent = agent;
        self.template = template;
        self
    }

    /// Builder: argument schema and hooks.
    #[must_use]
    pub fn with_args(mut self, specs: Vec<ArgSpec>, hooks: SkillHooks) -> Self {
        self.args = specs;
        self.hooks = hooks;
        self
    }

    /// Parse a `skills/<name>/skill.toml` document (body path resolved
    /// relative to the skill dir), returning the TYPED error.
    ///
    /// `from_toml` is the loader-facing wrapper that flattens this to a
    /// `String`, matching the other registry parsers and `read_toml`'s
    /// signature; prefer this one when you want the key back.
    pub fn try_from_toml(name: &str, skill_dir: &Path, v: &toml::Value) -> Result<Self, FieldError> {
        let table = toml_util::root_table(v, "skill.toml")?;

        let description = toml_util::req_string(table, "description")?;

        let body_file =
            toml_util::opt_nonempty_string(table, "body")?.unwrap_or_else(|| DEFAULT_BODY_FILE.to_string());
        // `Path::join` keeps an absolute `body` path as-is, so both
        // `body = "skill.md"` and an absolute override work. A missing file
        // is an empty body (see `body()`), so `skill.md` may be written
        // AFTER the skill is loaded and still takes effect — creating a file
        // inside `skills/<name>/` does not bump the `skills/` dir mtime, so
        // an existence check here could never be cleared without a reload.
        let body_path = Some(skill_dir.join(body_file));

        let agent = toml_util::opt_nonempty_string(table, "agent")?;
        let template = toml_util::opt_nonempty_string(table, "template")?;

        let uses = toml_util::string_list(table, "uses")?;
        if let Some(bad) = uses.iter().find(|u| u.trim().is_empty()) {
            let _ = bad;
            return Err(FieldError::key("uses", "entries must be non-empty strings"));
        }
        if uses.iter().any(|u| u == name) {
            return Err(FieldError::key(
                "uses",
                format!("skill '{name}' cannot use itself"),
            ));
        }
        if let Some(dupe) = first_duplicate(&uses) {
            return Err(FieldError::key("uses", format!("duplicate entry '{dupe}'")));
        }

        let tools = tool_policy_from(table)?;
        let env = toml_util::string_map(table, "env")?;
        let specs = args::parse_arg_specs(table)?;
        let hooks = SkillHooks::from_table(table)?;

        Ok(SkillDef {
            name: name.to_string(),
            description,
            body_path,
            agent,
            template,
            uses,
            tools,
            env,
            args: specs,
            hooks,
            body_cache: Mutex::new(None),
        })
    }

    /// Loader entry point: [`SkillDef::try_from_toml`] with the error
    /// flattened to the `String` the registry's error collector carries.
    pub fn from_toml(name: &str, skill_dir: &Path, v: &toml::Value) -> Result<Self, String> {
        SkillDef::try_from_toml(name, skill_dir, v).map_err(String::from)
    }

    /// The markdown body ("" when no body file is declared), mtime-cached.
    ///
    /// Re-stats the file on EVERY call and re-reads only when the mtime
    /// changed since the cached read — prose edits take effect on the next
    /// prompt without a registry reload (mirrors `Soul::text`). A missing
    /// file is an empty body; any other I/O failure propagates.
    ///
    /// This is the body of THIS skill only. For the `uses`-expanded text see
    /// [`composed_body`].
    pub fn body(&self) -> io::Result<String> {
        let path = match &self.body_path {
            Some(p) => p,
            None => return Ok(String::new()),
        };
        // A poisoned lock only means another thread panicked mid-read; the
        // cache is a plain value and stays usable.
        let mut cache = self.body_cache.lock().unwrap_or_else(|p| p.into_inner());
        let mtime = match std::fs::metadata(path) {
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
        let content = std::fs::read_to_string(path)?;
        *cache = Some((mtime, content.clone()));
        drop(cache);
        Ok(content)
    }
}

/// The composition order of `skill`: every skill it `uses`, transitively,
/// DEPENDENCY-FIRST, deduped by name, with `skill` itself last.
///
/// Dependency-first because a skill that `uses` another is refining it: the
/// general advice should be read before the specialisation that overrides it.
/// Dedup keeps the FIRST occurrence, so a skill pulled in by two different
/// paths appears once, at its earliest (most general) position.
///
/// Unknown `uses` entries are SKIPPED here rather than erroring: the loader
/// reports dangling references with a far better message and drops the entry,
/// so at runtime a missing name means "already reported". A cycle that somehow
/// survived the loader terminates the walk instead of hanging, and depth is
/// capped at [`MAX_DEPTH`].
pub fn composition_order(reg: &Registry, skill: &str) -> Result<Vec<String>, String> {
    if !reg.skills.contains_key(skill) {
        return Err(unknown_skill_message(reg, skill));
    }
    let mut out: Vec<String> = Vec::new();
    let mut done: IndexSet<String> = IndexSet::new();
    visit(reg, skill, 0, &mut done, &mut out)?;
    Ok(out)
}

fn visit(
    reg: &Registry,
    name: &str,
    depth: usize,
    done: &mut IndexSet<String>,
    out: &mut Vec<String>,
) -> Result<(), String> {
    if done.contains(name) {
        return Ok(());
    }
    if depth >= MAX_DEPTH {
        return Err(format!(
            "skill '{name}': composition nests deeper than {MAX_DEPTH} levels (`uses`)"
        ));
    }
    // Mark BEFORE descending: a cycle that escaped the loader then terminates
    // here instead of recursing forever.
    done.insert(name.to_string());
    if let Some(def) = reg.skills.get(name) {
        for dep in &def.uses {
            if reg.skills.contains_key(dep) {
                visit(reg, dep, depth + 1, done, out)?;
            }
        }
    }
    out.push(name.to_string());
    Ok(())
}

/// The `uses`-expanded markdown body of `skill`: each constituent skill's
/// body in dependency order, deduped by name, joined by a blank line.
///
/// Each constituent keeps its OWN mtime cache, so composition re-runs per call
/// (cheap: a stat per file) and an edit to any one body takes effect on the
/// next prompt with no reload.
pub fn composed_body(reg: &Registry, skill: &str) -> io::Result<String> {
    let order = composition_order(reg, skill).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut parts: Vec<String> = Vec::with_capacity(order.len());
    for name in &order {
        let Some(def) = reg.skills.get(name) else {
            continue;
        };
        let body = def.body()?;
        if !body.trim().is_empty() {
            parts.push(body.trim_end().to_string());
        }
    }
    Ok(parts.join("\n\n"))
}

/// "unknown skill 'x' (known: a, b)" — the one message shape for a missing
/// skill, so a typo reads the same from a command, an agent or another skill.
pub fn unknown_skill_message(reg: &Registry, skill: &str) -> String {
    let known: Vec<&str> = reg.skills.keys().map(String::as_str).collect();
    if known.is_empty() {
        format!("unknown skill '{skill}' (no skills are defined)")
    } else {
        format!("unknown skill '{skill}' (known: {})", known.join(", "))
    }
}

fn first_duplicate(items: &[String]) -> Option<&str> {
    let mut seen: IndexSet<&str> = IndexSet::new();
    items
        .iter()
        .find(|i| !seen.insert(i.as_str()))
        .map(String::as_str)
}

/// Parse the optional `[tools]` table into a `ToolPolicy`.
fn tool_policy_from(table: &Table) -> Result<ToolPolicy, FieldError> {
    let Some(tools) = toml_util::opt_table(table, "tools")? else {
        return Ok(ToolPolicy::default());
    };
    Ok(ToolPolicy {
        allow: toml_util::string_list(tools, "allow").map_err(|e| e.under("tools"))?,
        deny: toml_util::string_list(tools, "deny").map_err(|e| e.under("tools"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn parse(name: &str, dir: &Path, text: &str) -> Result<SkillDef, FieldError> {
        let v: toml::Value = text.parse().expect("valid TOML in test");
        SkillDef::try_from_toml(name, dir, &v)
    }

    fn err(name: &str, dir: &Path, text: &str) -> String {
        parse(name, dir, text).unwrap_err().to_string()
    }

    /// Force a distinct mtime on `path` (coarse-mtime filesystems would
    /// otherwise make back-to-back writes indistinguishable).
    fn bump_mtime(path: &Path, secs_forward: u64) {
        let f = fs::File::options().write(true).open(path).expect("open");
        let new = SystemTime::now() + Duration::from_secs(secs_forward);
        f.set_modified(new).expect("set_modified");
    }

    // ---- DEFAULTS-ONLY: the legacy shape still parses identically ----

    #[test]
    fn from_toml_minimal_defaults_are_the_legacy_behaviour() {
        let dir = tmpdir();
        let def = parse("s", dir.path(), "description = \"d\"\n").expect("parse");
        assert_eq!(def.name, "s");
        assert_eq!(def.description, "d");
        // Body defaults to skill.md next to skill.toml.
        assert_eq!(
            def.body_path.as_deref(),
            Some(dir.path().join("skill.md").as_path())
        );
        assert_eq!(def.tools, ToolPolicy::default());
        assert!(def.env.is_empty());
        // Every NEW key defaults to "absent", i.e. exactly the old behaviour.
        assert_eq!(def.agent, None);
        assert_eq!(def.template, None);
        assert!(def.uses.is_empty());
        assert!(def.args.is_empty());
        assert!(def.hooks.is_empty());
        assert_eq!(def.hooks.timeout_seconds, DEFAULT_HOOK_TIMEOUT_SECONDS);
    }

    #[test]
    fn from_toml_full_document() {
        let dir = tmpdir();
        let def = parse(
            "review",
            dir.path(),
            r#"
description = "How to review pull requests"
body = "notes.md"
agent = "reviewer"
template = "review-prompt"
uses = ["git-hygiene", "house-style"]

[[args]]
name = "pr"
required = true
description = "pull request number"

[[args]]
name = "focus"
rest = true

[tools]
allow = ["Bash", "Edit"]
deny = ["WebSearch"]

[env]
REVIEW_MODE = "strict"
ANOTHER = "x"

[hooks]
pre = ["gh", "pr", "checkout", "{{arg:pr}}"]
post = ["git", "checkout", "-"]
timeout_seconds = 120
"#,
        )
        .expect("parse");

        assert_eq!(def.name, "review");
        assert_eq!(def.description, "How to review pull requests");
        assert_eq!(
            def.body_path.as_deref(),
            Some(dir.path().join("notes.md").as_path())
        );
        assert_eq!(def.agent.as_deref(), Some("reviewer"));
        assert_eq!(def.template.as_deref(), Some("review-prompt"));
        assert_eq!(def.uses, vec!["git-hygiene", "house-style"]);
        assert_eq!(
            def.tools,
            ToolPolicy {
                allow: vec!["Bash".to_string(), "Edit".to_string()],
                deny: vec!["WebSearch".to_string()],
            }
        );
        assert_eq!(def.env.get("REVIEW_MODE").map(String::as_str), Some("strict"));
        assert_eq!(def.env.len(), 2);
        assert_eq!(def.args.len(), 2);
        assert_eq!(def.args[0].name, "pr");
        assert!(def.args[0].required);
        assert!(def.args[1].rest);
        assert_eq!(def.hooks.pre, vec!["gh", "pr", "checkout", "{{arg:pr}}"]);
        assert_eq!(def.hooks.post, vec!["git", "checkout", "-"]);
        assert_eq!(def.hooks.timeout_seconds, 120);
    }

    #[test]
    fn from_toml_absolute_body_path_wins() {
        let dir = tmpdir();
        let def = parse(
            "s",
            dir.path(),
            "description = \"d\"\nbody = \"/etc/stackhour/shared-skill.md\"\n",
        )
        .expect("parse");
        assert_eq!(
            def.body_path.as_deref(),
            Some(Path::new("/etc/stackhour/shared-skill.md"))
        );
    }

    #[test]
    fn from_toml_unknown_keys_ignored() {
        let dir = tmpdir();
        let def = parse(
            "s",
            dir.path(),
            "description = \"d\"\nfuture_key = true\n[extra]\nx = 1\n",
        )
        .expect("parse");
        assert_eq!(def.description, "d");
    }

    // ---- INVALID: every message names the key and what was expected ----

    #[test]
    fn missing_and_blank_description() {
        let dir = tmpdir();
        assert_eq!(
            err("s", dir.path(), "body = \"skill.md\"\n"),
            "key `description`: is required and must be a non-empty string"
        );
        assert_eq!(
            err("s", dir.path(), "description = \"\"\n"),
            "key `description`: must be a non-empty string"
        );
        assert_eq!(
            err("s", dir.path(), "description = 5\n"),
            "key `description`: must be a string, got an integer"
        );
    }

    #[test]
    fn blank_body_is_rejected() {
        let dir = tmpdir();
        assert_eq!(
            err("s", dir.path(), "description = \"d\"\nbody = \"\"\n"),
            "key `body`: must be a non-empty string"
        );
    }

    #[test]
    fn wrong_typed_tables_and_lists() {
        let dir = tmpdir();
        let d = dir.path();
        assert_eq!(
            err("s", d, "description = \"d\"\ntools = 1\n"),
            "key `tools`: must be a table, got an integer"
        );
        assert_eq!(
            err("s", d, "description = \"d\"\n[tools]\nallow = \"Bash\"\n"),
            "key `tools.allow`: must be an array of strings, got a string"
        );
        assert_eq!(
            err("s", d, "description = \"d\"\nenv = 1\n"),
            "key `env`: must be a table, got an integer"
        );
        assert_eq!(
            err("s", d, "description = \"d\"\n[env]\nX = 1\n"),
            "key `env.X`: must be a string, got an integer"
        );
        assert_eq!(
            err("s", d, "description = \"d\"\nuses = \"other\"\n"),
            "key `uses`: must be an array of strings, got a string"
        );
    }

    #[test]
    fn uses_rejects_self_reference_and_duplicates() {
        let dir = tmpdir();
        assert_eq!(
            err("review", dir.path(), "description = \"d\"\nuses = [\"review\"]\n"),
            "key `uses`: skill 'review' cannot use itself"
        );
        assert_eq!(
            err("s", dir.path(), "description = \"d\"\nuses = [\"a\", \"a\"]\n"),
            "key `uses`: duplicate entry 'a'"
        );
    }

    #[test]
    fn hook_errors_name_the_hooks_key() {
        let dir = tmpdir();
        let d = dir.path();
        assert_eq!(
            err("s", d, "description = \"d\"\nhooks = 3\n"),
            "key `hooks`: must be a table, got an integer"
        );
        assert_eq!(
            err("s", d, "description = \"d\"\n[hooks]\npre = \"gh pr checkout\"\n"),
            "key `hooks.pre`: must be an array of strings, got a string"
        );
        assert_eq!(
            err("s", d, "description = \"d\"\n[hooks]\npre = [\"gh\", \"  \"]\n"),
            "key `hooks.pre`: argv entries must be non-empty strings"
        );
        assert_eq!(
            err("s", d, "description = \"d\"\n[hooks]\ntimeout_seconds = 0\n"),
            "key `hooks.timeout_seconds`: must be at least 1 second"
        );
        assert_eq!(
            err("s", d, "description = \"d\"\n[hooks]\ntimeout_seconds = -5\n"),
            "key `hooks.timeout_seconds`: must be a non-negative integer"
        );
    }

    #[test]
    fn arg_spec_errors_come_through_with_their_index() {
        let dir = tmpdir();
        assert_eq!(
            err(
                "s",
                dir.path(),
                "description = \"d\"\n[[args]]\nname = \"a\"\nrest = true\n\n[[args]]\nname = \"b\"\n"
            ),
            "key `args[0].rest`: only the LAST argument may set rest = true ('a' is followed by 'b')"
        );
        assert_eq!(
            err(
                "s",
                dir.path(),
                "description = \"d\"\n[[args]]\nrequired = true\n"
            ),
            "key `args[0].name`: is required and must be a non-empty string"
        );
    }

    #[test]
    fn from_toml_non_table_document() {
        // read_toml always yields a table for a valid document, but the
        // guard must still hold for direct callers.
        let v = toml::Value::Integer(3);
        assert_eq!(
            SkillDef::from_toml("s", Path::new("/x"), &v).unwrap_err(),
            "skill.toml must be a TOML table"
        );
    }

    // ---- SkillDef::body ----

    #[test]
    fn body_reads_file_content() {
        let dir = tmpdir();
        fs::write(dir.path().join("skill.md"), "# Reviewing\n").unwrap();
        let def = parse("s", dir.path(), "description = \"d\"\n").expect("parse");
        assert_eq!(def.body().unwrap(), "# Reviewing\n");
    }

    #[test]
    fn body_missing_file_is_empty() {
        let dir = tmpdir();
        let def = parse("s", dir.path(), "description = \"d\"\n").expect("parse");
        assert_eq!(def.body().unwrap(), "");
    }

    #[test]
    fn body_none_path_is_empty() {
        let def = SkillDef::new(
            "s".to_string(),
            "d".to_string(),
            None,
            ToolPolicy::default(),
            IndexMap::new(),
        );
        assert_eq!(def.body().unwrap(), "");
    }

    #[test]
    fn body_caches_until_mtime_changes() {
        let dir = tmpdir();
        let path = dir.path().join("skill.md");
        fs::write(&path, "v1").unwrap();
        let def = parse("s", dir.path(), "description = \"d\"\n").expect("parse");
        assert_eq!(def.body().unwrap(), "v1");

        // Rewrite the content but pin the mtime back to the cached value:
        // body() must serve the (stale) cache — proof it is mtime-driven.
        let cached_mtime = {
            let guard = def.body_cache.lock().unwrap();
            guard.as_ref().expect("cache populated").0
        };
        fs::write(&path, "v2").unwrap();
        let f = fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(cached_mtime).unwrap();
        drop(f);
        assert_eq!(def.body().unwrap(), "v1");

        // Now bump the mtime: the next call re-reads.
        bump_mtime(&path, 10);
        assert_eq!(def.body().unwrap(), "v2");
    }

    #[test]
    fn body_created_after_first_use_is_picked_up() {
        let dir = tmpdir();
        let def = parse("s", dir.path(), "description = \"d\"\n").expect("parse");
        assert_eq!(def.body().unwrap(), "");
        fs::write(dir.path().join("skill.md"), "late body").unwrap();
        assert_eq!(def.body().unwrap(), "late body");
    }

    #[test]
    fn body_deleted_file_reverts_to_empty_and_recreation_rereads() {
        let dir = tmpdir();
        let path = dir.path().join("skill.md");
        fs::write(&path, "v1").unwrap();
        let def = parse("s", dir.path(), "description = \"d\"\n").expect("parse");
        assert_eq!(def.body().unwrap(), "v1");

        fs::remove_file(&path).unwrap();
        assert_eq!(def.body().unwrap(), "");

        fs::write(&path, "v2").unwrap();
        assert_eq!(def.body().unwrap(), "v2");
    }

    // ---- composition ----

    /// Build a registry on disk from `(skill, toml, body)` triples.
    fn registry_with(skills: &[(&str, &str, &str)]) -> (tempfile::TempDir, Registry) {
        let dir = tmpdir();
        for (name, manifest, body) in skills {
            let sd = dir.path().join("skills").join(name);
            fs::create_dir_all(&sd).unwrap();
            fs::write(sd.join("skill.toml"), manifest).unwrap();
            fs::write(sd.join("skill.md"), body).unwrap();
        }
        let reg = super::super::load(dir.path());
        (dir, reg)
    }

    #[test]
    fn composition_order_is_dependency_first_and_deduped() {
        let (_d, reg) = registry_with(&[
            ("base", "description = \"base\"\n", "BASE"),
            ("mid", "description = \"mid\"\nuses = [\"base\"]\n", "MID"),
            (
                "top",
                "description = \"top\"\nuses = [\"mid\", \"base\"]\n",
                "TOP",
            ),
        ]);
        assert!(reg.errors.is_empty(), "unexpected: {:?}", reg.errors);
        assert_eq!(
            composition_order(&reg, "top").unwrap(),
            vec!["base", "mid", "top"]
        );
        assert_eq!(composed_body(&reg, "top").unwrap(), "BASE\n\nMID\n\nTOP");
    }

    #[test]
    fn composed_body_skips_empty_bodies_and_unknown_uses() {
        let (_d, reg) = registry_with(&[
            ("base", "description = \"base\"\n", "   \n"),
            (
                "top",
                // `ghost` does not exist; the loader reports it, composition
                // must not double-report or blow up.
                "description = \"top\"\nuses = [\"base\", \"ghost\"]\n",
                "TOP",
            ),
        ]);
        assert_eq!(composed_body(&reg, "top").unwrap(), "TOP");
    }

    #[test]
    fn composition_order_rejects_an_unknown_skill_by_name() {
        let (_d, reg) = registry_with(&[("base", "description = \"base\"\n", "B")]);
        assert_eq!(
            composition_order(&reg, "nope").unwrap_err(),
            "unknown skill 'nope' (known: base)"
        );
    }

    #[test]
    fn unknown_skill_message_handles_an_empty_registry() {
        let dir = tmpdir();
        let reg = super::super::load(dir.path());
        assert_eq!(
            unknown_skill_message(&reg, "x"),
            "unknown skill 'x' (no skills are defined)"
        );
    }
}
