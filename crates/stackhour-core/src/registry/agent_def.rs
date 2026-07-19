//! Named agents (NEW): engine + model + tool policy + permission mode +
//! optional cwd + prompt template + SOUL DOCUMENT.
//!
//! The soul is a plain markdown file loaded lazily with an mtime cache: every
//! use re-stats and re-reads when changed, so editing soul.md takes effect on
//! the next prompt without a daemon restart.
//!
//! `agents/<name>/agent.toml` schema (all keys except `engine` optional):
//!
//! ```toml
//! label = "Reviewer"            # display label; defaults to the dir name
//! engine = "claude"             # required; cross-ref validated at load
//! model = "claude-opus-4"       # optional model override
//! soul = "soul.md"              # soul document, relative to the agent dir
//! skills = ["review"]           # skill ids; cross-ref validated at load
//! permission_mode = "default"   # "default" | "bypassPermissions"
//! cwd = "~/work/repo"           # optional working-dir override
//! prompt_template = "..."       # optional wrapper applied to each prompt
//!
//! [tools]
//! allow = ["Bash", "Edit"]
//! deny = ["WebSearch"]
//! ```
//!
//! Unknown keys are ignored (forward compatibility); validation errors are
//! plain strings collected into `Registry::errors` by the loader — a broken
//! agent.toml is skipped, never fatal.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Engine-specific tool allow/deny policy strings, passed through the
/// engine's declared flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// A soul document: mtime-cached markdown file.
#[derive(Debug)]
pub struct Soul {
    path: PathBuf,
    cache: Mutex<Option<(SystemTime, String)>>,
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
        Ok(content)
    }
}

/// A named agent (agents/<name>/agent.toml).
#[derive(Debug)]
pub struct AgentDef {
    pub name: String,
    pub label: String,
    /// Must name a known engine (cross-ref validated at load).
    pub engine: String,
    pub model: Option<String>,
    pub soul: Soul,
    /// Skill ids (cross-ref validated at load).
    pub skills: Vec<String>,
    /// `default` | `bypassPermissions`.
    pub permission_mode: String,
    /// Optional working-dir override (may contain `~`).
    pub cwd: Option<String>,
    /// Optional wrapper template applied to each user prompt.
    pub prompt_template: Option<String>,
    pub tools: ToolPolicy,
}

const DEFAULT_SOUL_FILE: &str = "soul.md";
const PERMISSION_MODES: &[&str] = &["default", "bypassPermissions"];

impl AgentDef {
    /// Parse an `agents/<name>/agent.toml` document (soul path resolved
    /// relative to the agent dir). Errors are collector-ready strings for
    /// `Registry::errors`; unknown keys are ignored.
    pub fn from_toml(name: &str, agent_dir: &Path, v: &toml::Value) -> Result<Self, String> {
        let table = v
            .as_table()
            .ok_or_else(|| "agent.toml must be a TOML table".to_string())?;

        let engine = match opt_string(table, "engine")? {
            Some(s) if !s.is_empty() => s,
            Some(_) => return Err("engine cannot be empty".to_string()),
            None => return Err("engine is required".to_string()),
        };

        let label = match opt_string(table, "label")? {
            Some(s) if !s.is_empty() => s,
            _ => name.to_string(),
        };

        let model = opt_string(table, "model")?.filter(|s| !s.is_empty());

        let soul_file = match opt_string(table, "soul")? {
            Some(s) if !s.is_empty() => s,
            Some(_) => return Err("soul cannot be empty".to_string()),
            None => DEFAULT_SOUL_FILE.to_string(),
        };
        // `Path::join` keeps an absolute `soul` path as-is, so both
        // `soul = "soul.md"` and an absolute override work.
        let soul = Soul::new(agent_dir.join(soul_file));

        let skills = opt_string_list(table, "skills")?.unwrap_or_default();

        let permission_mode = match opt_string(table, "permission_mode")? {
            Some(s) => {
                if !PERMISSION_MODES.contains(&s.as_str()) {
                    return Err("permission_mode must be \"default\" or \"bypassPermissions\"".to_string());
                }
                s
            }
            None => "default".to_string(),
        };

        let cwd = opt_string(table, "cwd")?.filter(|s| !s.is_empty());
        let prompt_template = opt_string(table, "prompt_template")?.filter(|s| !s.is_empty());
        let tools = tool_policy_from(table)?;

        Ok(AgentDef {
            name: name.to_string(),
            label,
            engine,
            model,
            soul,
            skills,
            permission_mode,
            cwd,
            prompt_template,
            tools,
        })
    }
}

/// Optional string key: missing -> None; wrong type -> error.
fn opt_string(table: &toml::value::Table, key: &str) -> Result<Option<String>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// Optional array-of-strings key: missing -> None; wrong element type or a
/// blank element -> error.
fn opt_string_list(table: &toml::value::Table, key: &str) -> Result<Option<Vec<String>>, String> {
    let value = match table.get(key) {
        None => return Ok(None),
        Some(v) => v,
    };
    let arr = value
        .as_array()
        .ok_or_else(|| format!("{key} must be an array of strings"))?;
    let mut out: Vec<String> = Vec::with_capacity(arr.len());
    for item in arr {
        match item.as_str() {
            Some(s) if !s.is_empty() => out.push(s.to_string()),
            Some(_) => return Err(format!("{key} entries cannot be empty")),
            None => return Err(format!("{key} must be an array of strings")),
        }
    }
    Ok(Some(out))
}

/// Parse the optional `[tools]` table into a `ToolPolicy`.
fn tool_policy_from(table: &toml::value::Table) -> Result<ToolPolicy, String> {
    let value = match table.get("tools") {
        None => return Ok(ToolPolicy::default()),
        Some(v) => v,
    };
    let tools = value
        .as_table()
        .ok_or_else(|| "tools must be a table".to_string())?;
    Ok(ToolPolicy {
        allow: opt_string_list(tools, "allow")
            .map_err(|e| format!("tools.{e}"))?
            .unwrap_or_default(),
        deny: opt_string_list(tools, "deny")
            .map_err(|e| format!("tools.{e}"))?
            .unwrap_or_default(),
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

    // ---- AgentDef::from_toml ----

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
soul = "identity.md"
skills = ["review", "security"]
permission_mode = "bypassPermissions"
cwd = "~/work/repo"
prompt_template = "Review this: {{prompt}}"

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
        assert_eq!(def.soul.path(), dir.path().join("identity.md").as_path());
        assert_eq!(def.skills, vec!["review".to_string(), "security".to_string()]);
        assert_eq!(def.permission_mode, "bypassPermissions");
        assert_eq!(def.cwd.as_deref(), Some("~/work/repo"));
        assert_eq!(def.prompt_template.as_deref(), Some("Review this: {{prompt}}"));
        assert_eq!(
            def.tools,
            ToolPolicy {
                allow: vec!["Bash".to_string(), "Edit".to_string()],
                deny: vec!["WebSearch".to_string()],
            }
        );
    }

    #[test]
    fn from_toml_minimal_defaults() {
        let dir = tmpdir();
        let def = parse("helper", dir.path(), "engine = \"codex\"\n").expect("parse");
        assert_eq!(def.name, "helper");
        assert_eq!(def.label, "helper"); // defaults to the dir name
        assert_eq!(def.engine, "codex");
        assert_eq!(def.model, None);
        assert_eq!(def.soul.path(), dir.path().join("soul.md").as_path());
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
    fn from_toml_missing_engine() {
        let dir = tmpdir();
        assert_eq!(
            parse("a", dir.path(), "label = \"x\"\n").unwrap_err(),
            "engine is required"
        );
    }

    #[test]
    fn from_toml_empty_engine() {
        let dir = tmpdir();
        assert_eq!(
            parse("a", dir.path(), "engine = \"\"\n").unwrap_err(),
            "engine cannot be empty"
        );
    }

    #[test]
    fn from_toml_wrong_types() {
        let dir = tmpdir();
        assert_eq!(
            parse("a", dir.path(), "engine = 5\n").unwrap_err(),
            "engine must be a string"
        );
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\nlabel = 3\n").unwrap_err(),
            "label must be a string"
        );
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\nskills = \"review\"\n").unwrap_err(),
            "skills must be an array of strings"
        );
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\nskills = [1]\n").unwrap_err(),
            "skills must be an array of strings"
        );
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\nskills = [\"\"]\n").unwrap_err(),
            "skills entries cannot be empty"
        );
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\ntools = 1\n").unwrap_err(),
            "tools must be a table"
        );
        assert_eq!(
            parse(
                "a",
                dir.path(),
                "engine = \"claude\"\n[tools]\nallow = \"Bash\"\n"
            )
            .unwrap_err(),
            "tools.allow must be an array of strings"
        );
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\n[tools]\ndeny = [7]\n").unwrap_err(),
            "tools.deny must be an array of strings"
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
            "permission_mode must be \"default\" or \"bypassPermissions\""
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
    fn from_toml_empty_optionals_fall_back() {
        let dir = tmpdir();
        let def = parse(
            "a",
            dir.path(),
            "engine = \"claude\"\nlabel = \"\"\nmodel = \"\"\ncwd = \"\"\nprompt_template = \"\"\n",
        )
        .expect("parse");
        assert_eq!(def.label, "a");
        assert_eq!(def.model, None);
        assert_eq!(def.cwd, None);
        assert_eq!(def.prompt_template, None);
    }

    #[test]
    fn from_toml_empty_soul_rejected() {
        let dir = tmpdir();
        assert_eq!(
            parse("a", dir.path(), "engine = \"claude\"\nsoul = \"\"\n").unwrap_err(),
            "soul cannot be empty"
        );
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
        // read_toml always yields a table for a valid document, but the
        // guard must still hold for direct callers.
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
}
