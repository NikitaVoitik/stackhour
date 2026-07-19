//! The embedded starter config tree.
//!
//! `defaults/` next to this file is a complete, heavily commented, VALID
//! config directory. It is compiled into the binary with `include_str!` and
//! serves two purposes:
//!
//! 1. `stackhour bridge init --config-dir` materialises it, so a user starts
//!    from a real working example rather than from documentation they have to
//!    transcribe.
//! 2. It is exercised by this module's tests through the *real* loader, so the
//!    documented schema cannot drift from the implemented schema — if a pillar
//!    renames a key, the starter tree stops loading and a test goes red.
//!
//! Note what this module is NOT: it is not the built-in *entity* layer. The
//! shipped commands live in `command::builtin_commands()` and the shipped
//! engines in `engine::builtin_claude()` / `builtin_codex()`, because those
//! must exist for a user who has no config directory at all. This tree is
//! the *example*, and every file in it is safe to delete.

use std::io;
use std::path::{Path, PathBuf};

/// One file of the starter tree: (path relative to the config dir, contents).
pub type StarterFile = (&'static str, &'static str);

/// Every file of the starter tree, in creation order (parents before
/// children, so a naive writer never has to mkdir out of order).
pub const STARTER_FILES: &[StarterFile] = &[
    ("README.md", include_str!("defaults/README.md")),
    (
        "commands/deploy.toml",
        include_str!("defaults/commands/deploy.toml"),
    ),
    (
        "commands/status.toml",
        include_str!("defaults/commands/status.toml"),
    ),
    (
        "skills/review/skill.toml",
        include_str!("defaults/skills/review/skill.toml"),
    ),
    (
        "skills/review/skill.md",
        include_str!("defaults/skills/review/skill.md"),
    ),
    (
        "agents/reviewer/agent.toml",
        include_str!("defaults/agents/reviewer/agent.toml"),
    ),
    (
        "agents/reviewer/soul.md",
        include_str!("defaults/agents/reviewer/soul.md"),
    ),
    (
        "engines/ollama.toml",
        include_str!("defaults/engines/ollama.toml"),
    ),
    ("prompts/deploy.md", include_str!("defaults/prompts/deploy.md")),
    ("prompts/help.md", include_str!("defaults/prompts/help.md")),
];

/// What `materialize` did with one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    /// The file did not exist and was created.
    Created(PathBuf),
    /// The file already existed and was left ALONE.
    Skipped(PathBuf),
}

impl Written {
    pub fn path(&self) -> &Path {
        match self {
            Written::Created(p) | Written::Skipped(p) => p,
        }
    }
    pub fn was_created(&self) -> bool {
        matches!(self, Written::Created(_))
    }
}

/// Write the starter tree into `config_dir`.
///
/// NEVER overwrites: an existing file is reported as [`Written::Skipped`] and
/// left byte-for-byte alone, so running `bridge init` twice — or running it on
/// a directory a user has already edited — cannot destroy their work. Missing
/// parent directories are created. `config.json` is deliberately NOT part of
/// the tree: it is the legacy settings file and is owned by `bridge init`
/// proper.
pub fn materialize(config_dir: &Path) -> io::Result<Vec<Written>> {
    let mut out = Vec::with_capacity(STARTER_FILES.len());
    for (rel, contents) in STARTER_FILES {
        let path = config_dir.join(rel);
        if path.exists() {
            out.push(Written::Skipped(path));
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
        out.push(Written::Created(path));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn every_toml_asset_parses_as_toml() {
        for (rel, contents) in STARTER_FILES {
            if !rel.ends_with(".toml") {
                continue;
            }
            contents
                .parse::<toml::Value>()
                .unwrap_or_else(|e| panic!("starter file {rel} is not valid TOML: {e}"));
        }
    }

    #[test]
    fn materialize_creates_the_whole_tree() {
        let dir = tmp();
        let written = materialize(dir.path()).expect("materialize");
        assert_eq!(written.len(), STARTER_FILES.len());
        assert!(written.iter().all(Written::was_created));
        for (rel, contents) in STARTER_FILES {
            let path = dir.path().join(rel);
            assert!(path.is_file(), "{rel} was not written");
            assert_eq!(&std::fs::read_to_string(&path).unwrap(), contents);
        }
    }

    #[test]
    fn materialize_never_overwrites_an_existing_file() {
        let dir = tmp();
        std::fs::create_dir_all(dir.path().join("commands")).unwrap();
        std::fs::write(dir.path().join("commands/deploy.toml"), "MINE\n").unwrap();

        let written = materialize(dir.path()).expect("materialize");
        let deploy = dir.path().join("commands/deploy.toml");
        assert_eq!(
            std::fs::read_to_string(&deploy).unwrap(),
            "MINE\n",
            "an existing file must survive bridge init"
        );
        assert!(written
            .iter()
            .any(|w| matches!(w, Written::Skipped(p) if p == &deploy)));
        // ...and everything else still landed.
        assert!(dir.path().join("prompts/deploy.md").is_file());
    }

    #[test]
    fn materialize_is_idempotent() {
        let dir = tmp();
        materialize(dir.path()).expect("first");
        let second = materialize(dir.path()).expect("second");
        assert!(
            second.iter().all(|w| !w.was_created()),
            "the second run must create nothing"
        );
    }

    /// The load-bearing test of this module: the DOCUMENTED example must be
    /// loadable by the REAL loader with no errors. If a schema key is renamed
    /// by any pillar and the example is not updated, this goes red.
    #[test]
    fn the_starter_tree_loads_with_zero_errors() {
        let dir = tmp();
        materialize(dir.path()).expect("materialize");
        let reg = registry::load(dir.path());
        let rendered: Vec<String> = reg.errors.iter().map(ToString::to_string).collect();
        assert!(
            reg.errors.is_empty(),
            "the shipped example must be valid, got:\n  {}",
            rendered.join("\n  ")
        );
    }

    #[test]
    fn the_starter_tree_defines_the_entities_its_readme_promises() {
        let dir = tmp();
        materialize(dir.path()).expect("materialize");
        let reg = registry::load(dir.path());

        assert!(reg.engines.contains_key("ollama"), "user engine missing");
        // ...without displacing the built-ins.
        assert!(reg.engines.contains_key("claude"));
        assert!(reg.engines.contains_key("codex"));

        assert!(reg.agents.contains_key("reviewer"));
        assert_eq!(reg.agents["reviewer"].label, "Reviewer");
        assert!(reg.skills.contains_key("review"));
        assert!(reg.commands.contains_key("deploy"));
        assert!(reg.commands.contains_key("status"));

        // A NEW prompt template (not shadowing a built-in) must resolve, or
        // commands/deploy.toml's cross-reference could not have passed.
        assert!(reg.prompts.has("deploy"));
        assert!(reg.prompts.render("deploy", &[("env", "prod")]).contains("prod"));
        // ...and the built-in override took effect.
        assert!(reg.prompts.render("help", &[]).contains("Bridge"));
    }

    #[test]
    fn the_starter_agent_soul_and_skill_body_are_readable() {
        let dir = tmp();
        materialize(dir.path()).expect("materialize");
        let reg = registry::load(dir.path());
        let soul = reg.agents["reviewer"].soul.text().expect("soul");
        assert!(soul.contains("Lead with the verdict"));
        let body = reg.skills["review"].body().expect("body");
        assert!(body.contains("Correctness"));
    }

    #[test]
    fn every_documented_asset_is_reachable_from_starter_files() {
        // Guards against adding a file to defaults/ and forgetting the
        // include_str!, which would make it invisible at runtime.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/registry/defaults");
        let mut found: Vec<String> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read defaults dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path
                        .strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    found.push(rel);
                }
            }
        }
        found.sort();
        let mut declared: Vec<String> = STARTER_FILES.iter().map(|(r, _)| r.to_string()).collect();
        declared.sort();
        assert_eq!(found, declared, "defaults/ and STARTER_FILES disagree");
    }
}
