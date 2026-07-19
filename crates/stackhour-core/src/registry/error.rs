//! `FieldError` — the one way registry validation messages are constructed.
//!
//! Every schema validation failure in the registry must name the offending
//! FILE and the offending KEY and say what was expected. Free-form `String`
//! errors kept forgetting one or both, so all five registry pillars construct
//! their errors through this type instead.
//!
//! The file is usually NOT known at the point the error is raised: a
//! `from_toml` receives the parsed document, not its path. So a `FieldError`
//! starts file-less and the loader attaches the path on the way out
//! (`FieldError::in_file`). `Display` renders whatever is known:
//!
//! ```text
//! agents/reviewer/agent.toml: key `engine`: references unknown engine 'gpt5' (known: claude, codex)
//! key `engine`: references unknown engine 'gpt5'          // before attachment
//! agents/reviewer/agent.toml: must be a TOML table        // file-level, no key
//! ```
//!
//! `From<FieldError> for String` exists so the pre-existing
//! `Result<_, String>` `from_toml` signatures can keep using `?` on the
//! `toml_util` helpers while they migrate.

use std::fmt;
use std::path::{Path, PathBuf};

/// A validation error that knows which file and which key it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// Offending file. `None` until the loader attaches it.
    pub file: Option<PathBuf>,
    /// Offending key, dotted for nested tables (`resume.subcommand`,
    /// `args[2].name`). Empty for whole-file errors.
    pub key: String,
    /// What was wrong / what was expected. No trailing period.
    pub msg: String,
}

impl FieldError {
    /// An error about a specific key. File attached later by the loader.
    pub fn key(key: impl Into<String>, msg: impl Into<String>) -> Self {
        FieldError {
            file: None,
            key: key.into(),
            msg: msg.into(),
        }
    }

    /// An error about the file as a whole (bad TOML, wrong top-level shape).
    pub fn file_level(msg: impl Into<String>) -> Self {
        FieldError {
            file: None,
            key: String::new(),
            msg: msg.into(),
        }
    }

    /// An error about a key nested under `parent` (`parent.key`).
    pub fn nested(parent: &str, key: &str, msg: impl Into<String>) -> Self {
        FieldError::key(format!("{parent}.{key}"), msg)
    }

    /// Attach (or replace) the file this error came from. Idempotent, and
    /// safe to call on errors bubbled up from nested helpers.
    #[must_use]
    pub fn in_file(mut self, file: impl AsRef<Path>) -> Self {
        self.file = Some(file.as_ref().to_path_buf());
        self
    }

    /// Attach the file only if none is set yet (keeps the innermost, most
    /// specific path when an error crosses a directory boundary).
    #[must_use]
    pub fn or_file(mut self, file: impl AsRef<Path>) -> Self {
        if self.file.is_none() {
            self.file = Some(file.as_ref().to_path_buf());
        }
        self
    }

    /// Prefix the key with `parent`, e.g. when a sub-table parser that only
    /// knows `name` is called from a parser that knows it lives under `args`.
    #[must_use]
    pub fn under(mut self, parent: &str) -> Self {
        self.key = if self.key.is_empty() {
            parent.to_string()
        } else {
            format!("{parent}.{}", self.key)
        };
        self
    }

    /// Render "expected one of" tails consistently:
    /// `unknown engine 'gpt5' (known: claude, codex)`.
    pub fn with_known(mut self, known: &[&str]) -> Self {
        if !known.is_empty() {
            self.msg = format!("{} (known: {})", self.msg, known.join(", "));
        }
        self
    }

    /// The message WITHOUT the file prefix (`key \`k\`: msg`), which is what
    /// `RegistryError::message` carries — the file lives in its own field.
    pub fn message(&self) -> String {
        if self.key.is_empty() {
            self.msg.clone()
        } else {
            format!("key `{}`: {}", self.key, self.msg)
        }
    }
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(file) => write!(f, "{}: {}", file.display(), self.message()),
            None => f.write_str(&self.message()),
        }
    }
}

impl std::error::Error for FieldError {}

/// Bridge to the legacy `Result<_, String>` `from_toml` signatures so the
/// shared `toml_util` helpers can be used with `?` before a pillar migrates.
impl From<FieldError> for String {
    fn from(e: FieldError) -> String {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_without_file_is_key_and_message() {
        let e = FieldError::key("engine", "must be a non-empty string");
        assert_eq!(e.to_string(), "key `engine`: must be a non-empty string");
    }

    #[test]
    fn display_with_file_matches_the_agreed_shape() {
        let e = FieldError::key("engine", "references unknown engine 'gpt5'")
            .with_known(&["claude", "codex"])
            .in_file("agents/reviewer/agent.toml");
        assert_eq!(
            e.to_string(),
            "agents/reviewer/agent.toml: key `engine`: references unknown engine 'gpt5' (known: claude, codex)"
        );
    }

    #[test]
    fn file_level_errors_omit_the_key_segment() {
        let e = FieldError::file_level("must be a TOML table").in_file("engines/ollama.toml");
        assert_eq!(e.to_string(), "engines/ollama.toml: must be a TOML table");
        assert_eq!(e.message(), "must be a TOML table");
    }

    #[test]
    fn nested_and_under_compose_dotted_keys() {
        assert_eq!(
            FieldError::nested("resume", "subcommand", "x").key,
            "resume.subcommand"
        );
        assert_eq!(FieldError::key("name", "x").under("args[0]").key, "args[0].name");
        assert_eq!(FieldError::file_level("x").under("hooks").key, "hooks");
    }

    #[test]
    fn or_file_keeps_the_innermost_path() {
        let e = FieldError::key("k", "m")
            .in_file("skills/review/skill.toml")
            .or_file("skills/review");
        assert_eq!(e.file.as_deref(), Some(Path::new("skills/review/skill.toml")));
    }

    #[test]
    fn in_file_replaces_an_existing_path() {
        let e = FieldError::key("k", "m").in_file("a").in_file("b");
        assert_eq!(e.file.as_deref(), Some(Path::new("b")));
    }

    #[test]
    fn with_known_on_an_empty_list_is_a_no_op() {
        let e = FieldError::key("k", "m").with_known(&[]);
        assert_eq!(e.msg, "m");
    }

    #[test]
    fn converts_into_string_for_legacy_signatures() {
        fn legacy() -> Result<(), String> {
            Err(FieldError::key("bin", "is required"))?
        }
        assert_eq!(legacy().unwrap_err(), "key `bin`: is required");
    }
}
