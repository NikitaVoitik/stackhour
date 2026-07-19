//! Skills (NEW): reusable capability packs — a description, a markdown body
//! appended under `## Skills` in the composed system prompt (mtime-cached
//! like Soul), and optional tool allow/deny additions and env vars merged
//! into the engine spawn.
//!
//! `skills/<name>/skill.toml` schema (only `description` is required):
//!
//! ```toml
//! description = "How to review pull requests"  # required, non-empty
//! body = "skill.md"        # markdown body, relative to the skill dir
//!                          # (default "skill.md"; absolute paths allowed)
//!
//! [tools]                  # optional additions merged into the agent's
//! allow = ["Bash"]         # ToolPolicy for the engine spawn
//! deny = ["WebSearch"]
//!
//! [env]                    # optional env vars merged into the spawn
//! REVIEW_MODE = "strict"
//! ```
//!
//! Unknown keys are ignored (forward compatibility); validation errors are
//! plain strings collected into `Registry::errors` by the loader — a broken
//! skill.toml is skipped, never fatal.

use super::agent_def::ToolPolicy;
use indexmap::IndexMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

const DEFAULT_BODY_FILE: &str = "skill.md";

/// A skill definition (skills/<name>/skill.toml).
#[derive(Debug)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    /// Optional prose body (skill.md), re-read on mtime change at each use.
    pub body_path: Option<PathBuf>,
    pub tools: ToolPolicy,
    pub env: IndexMap<String, String>,
    /// Per-file mtime cache for `body()` (same discipline as `Soul`).
    body_cache: Mutex<Option<(SystemTime, String)>>,
}

impl SkillDef {
    /// Build a skill programmatically (tests / future callers). `from_toml`
    /// is the loader entry point.
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
            tools,
            env,
            body_cache: Mutex::new(None),
        }
    }

    /// Parse a `skills/<name>/skill.toml` document (body path resolved
    /// relative to the skill dir). Errors are collector-ready strings for
    /// `Registry::errors`; unknown keys are ignored.
    pub fn from_toml(name: &str, skill_dir: &Path, v: &toml::Value) -> Result<Self, String> {
        let table = v
            .as_table()
            .ok_or_else(|| "skill.toml must be a TOML table".to_string())?;

        let description = match opt_string(table, "description")? {
            Some(s) if !s.is_empty() => s,
            Some(_) => return Err("description cannot be empty".to_string()),
            None => return Err("description is required".to_string()),
        };

        let body_file = match opt_string(table, "body")? {
            Some(s) if !s.is_empty() => s,
            Some(_) => return Err("body cannot be empty".to_string()),
            None => DEFAULT_BODY_FILE.to_string(),
        };
        // `Path::join` keeps an absolute `body` path as-is, so both
        // `body = "skill.md"` and an absolute override work. A missing file
        // is an empty body (see `body()`), so `skill.md` may be written
        // AFTER the skill is loaded and still takes effect — creating a file
        // inside `skills/<name>/` does not bump the `skills/` dir mtime, so
        // an existence check here could never be cleared without a reload.
        let body_path = Some(skill_dir.join(body_file));

        let tools = tool_policy_from(table)?;
        let env = env_from(table)?;

        Ok(SkillDef::new(
            name.to_string(),
            description,
            body_path,
            tools,
            env,
        ))
    }

    /// The markdown body ("" when no body file is declared), mtime-cached.
    ///
    /// Re-stats the file on EVERY call and re-reads only when the mtime
    /// changed since the cached read — prose edits take effect on the next
    /// prompt without a registry reload (mirrors `Soul::text`). A missing
    /// file is an empty body; any other I/O failure propagates.
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
        Ok(content)
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

/// Parse the optional `[env]` table (string values only). Keys keep the
/// parser's deterministic order (the vanilla `toml` crate sorts table keys,
/// so the map is alphabetical by variable name).
fn env_from(table: &toml::value::Table) -> Result<IndexMap<String, String>, String> {
    let value = match table.get("env") {
        None => return Ok(IndexMap::new()),
        Some(v) => v,
    };
    let env_table = value
        .as_table()
        .ok_or_else(|| "env must be a table".to_string())?;
    let mut out: IndexMap<String, String> = IndexMap::with_capacity(env_table.len());
    for (key, val) in env_table {
        match val.as_str() {
            Some(s) => {
                out.insert(key.clone(), s.to_string());
            }
            None => return Err(format!("env.{key} must be a string")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn parse(name: &str, dir: &Path, text: &str) -> Result<SkillDef, String> {
        let v: toml::Value = text.parse().expect("valid TOML in test");
        SkillDef::from_toml(name, dir, &v)
    }

    /// Force a distinct mtime on `path` (coarse-mtime filesystems would
    /// otherwise make back-to-back writes indistinguishable).
    fn bump_mtime(path: &Path, secs_forward: u64) {
        let f = fs::File::options().write(true).open(path).expect("open");
        let new = SystemTime::now() + Duration::from_secs(secs_forward);
        f.set_modified(new).expect("set_modified");
    }

    // ---- SkillDef::from_toml ----

    #[test]
    fn from_toml_full_document() {
        let dir = tmpdir();
        let def = parse(
            "review",
            dir.path(),
            r#"
description = "How to review pull requests"
body = "notes.md"

[tools]
allow = ["Bash", "Edit"]
deny = ["WebSearch"]

[env]
REVIEW_MODE = "strict"
ANOTHER = "x"
"#,
        )
        .expect("parse");

        assert_eq!(def.name, "review");
        assert_eq!(def.description, "How to review pull requests");
        assert_eq!(
            def.body_path.as_deref(),
            Some(dir.path().join("notes.md").as_path())
        );
        assert_eq!(
            def.tools,
            ToolPolicy {
                allow: vec!["Bash".to_string(), "Edit".to_string()],
                deny: vec!["WebSearch".to_string()],
            }
        );
        assert_eq!(def.env.get("REVIEW_MODE").map(String::as_str), Some("strict"));
        assert_eq!(def.env.get("ANOTHER").map(String::as_str), Some("x"));
        assert_eq!(def.env.len(), 2);
    }

    #[test]
    fn from_toml_minimal_defaults() {
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
    fn from_toml_missing_description() {
        let dir = tmpdir();
        assert_eq!(
            parse("s", dir.path(), "body = \"skill.md\"\n").unwrap_err(),
            "description is required"
        );
    }

    #[test]
    fn from_toml_empty_description() {
        let dir = tmpdir();
        assert_eq!(
            parse("s", dir.path(), "description = \"\"\n").unwrap_err(),
            "description cannot be empty"
        );
    }

    #[test]
    fn from_toml_empty_body_rejected() {
        let dir = tmpdir();
        assert_eq!(
            parse("s", dir.path(), "description = \"d\"\nbody = \"\"\n").unwrap_err(),
            "body cannot be empty"
        );
    }

    #[test]
    fn from_toml_wrong_types() {
        let dir = tmpdir();
        assert_eq!(
            parse("s", dir.path(), "description = 5\n").unwrap_err(),
            "description must be a string"
        );
        assert_eq!(
            parse("s", dir.path(), "description = \"d\"\nbody = 3\n").unwrap_err(),
            "body must be a string"
        );
        assert_eq!(
            parse("s", dir.path(), "description = \"d\"\ntools = 1\n").unwrap_err(),
            "tools must be a table"
        );
        assert_eq!(
            parse(
                "s",
                dir.path(),
                "description = \"d\"\n[tools]\nallow = \"Bash\"\n"
            )
            .unwrap_err(),
            "tools.allow must be an array of strings"
        );
        assert_eq!(
            parse(
                "s",
                dir.path(),
                "description = \"d\"\n[tools]\ndeny = [7]\n"
            )
            .unwrap_err(),
            "tools.deny must be an array of strings"
        );
        assert_eq!(
            parse(
                "s",
                dir.path(),
                "description = \"d\"\n[tools]\nallow = [\"\"]\n"
            )
            .unwrap_err(),
            "tools.allow entries cannot be empty"
        );
        assert_eq!(
            parse("s", dir.path(), "description = \"d\"\nenv = 1\n").unwrap_err(),
            "env must be a table"
        );
        assert_eq!(
            parse("s", dir.path(), "description = \"d\"\n[env]\nX = 1\n").unwrap_err(),
            "env.X must be a string"
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
}
