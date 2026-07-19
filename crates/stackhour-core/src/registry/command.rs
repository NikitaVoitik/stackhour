//! Declarative Telegram commands (NEW).
//!
//! kind=prompt renders a named template with `{{args}}` and routes it exactly
//! like typed text (optionally switching agent/target first); kind=agent /
//! engine / target switches state; kind=shell runs a FIXED argv with user
//! args appended as ONE final argv element — never shell-interpolated.
//! Built-in commands always win over user files of the same name:
//! [`CommandDef::from_toml`] rejects every [`RESERVED`] name, so a user file
//! can never shadow a built-in verb.
//!
//! `commands/<name>.toml` schema (`kind` and `description` always required):
//!
//! ```toml
//! description = "Deploy to production"  # required; shown by setMyCommands
//! kind = "prompt"                       # "prompt" | "agent" | "engine"
//!                                       #   | "target" | "shell"
//! template = "deploy"                   # kind=prompt: required prompt name
//! agent = "reviewer"                    # kind=agent: required; other kinds:
//!                                       #   optional pre-switch
//! engine = "codex"                      # kind=engine: required
//! target = "gcp"                        # kind=target: required; must be
//!                                       #   "gcp" or "mac" wherever present
//! argv = ["./deploy.sh", "--prod"]      # kind=shell: required, non-empty
//! confirm = true                        # optional; inline Yes/Cancel first
//! ```
//!
//! Unknown keys are ignored (forward compatibility); validation errors are
//! plain strings collected into `Registry::errors` by the loader — a broken
//! command file is skipped, never fatal. Cross-references (`agent` exists,
//! prompt `template` exists) are checked by the loader, not here.

/// The built-in command names that can NEVER be shadowed by user files.
pub const RESERVED: &[&str] = &[
    "start", "help", "menu", "claude", "codex", "mac", "local", "gcp", "remote", "where", "status", "new",
    "reset", "stop",
];

/// The built-in setMyCommands entries, byte-identical to the payload in
/// coordinator.mjs (same order, same descriptions). The full setMyCommands
/// payload is these entries followed by the loaded user commands.
///
/// Deliberately a subset of [`RESERVED`]: `/start`, `/local`, `/remote`,
/// `/status` and `/reset` are aliases the JS coordinator handles but never
/// registered with Telegram — parity requires keeping them out of the
/// payload while still refusing to let user files shadow them.
pub const BUILTIN_MY_COMMANDS: &[(&str, &str)] = &[
    ("claude", "Use Claude Code 🧠"),
    ("codex", "Use Codex 🛠"),
    ("mac", "Run on the Mac 🖥️"),
    ("gcp", "Run on the GCP box ☁️"),
    ("where", "Show active target & session"),
    ("new", "Fresh session on active target"),
    ("stop", "Kill/cancel the running job"),
    ("menu", "Show tap-button controls"),
    ("help", "Show command list"),
];

/// Telegram bot-command name limit (`setMyCommands`: 1-32 chars of lowercase
/// English letters, digits and underscores).
const MAX_COMMAND_NAME_LEN: usize = 32;

/// The two runnable targets a command may switch to.
const TARGETS: &[&str] = &["gcp", "mac"];

/// What a declarative command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// Render `template` with `{{args}}` and route like typed text.
    Prompt,
    /// Switch the active named agent.
    Agent,
    /// Switch the active engine.
    Engine,
    /// Switch the active target.
    Target,
    /// Run a fixed argv; stdout is replied to the chat.
    Shell,
}

impl CommandKind {
    /// The TOML spelling of this kind (`kind = "…"` in commands/<name>.toml).
    pub fn as_toml_str(self) -> &'static str {
        match self {
            CommandKind::Prompt => "prompt",
            CommandKind::Agent => "agent",
            CommandKind::Engine => "engine",
            CommandKind::Target => "target",
            CommandKind::Shell => "shell",
        }
    }

    fn from_toml_str(s: &str) -> Option<Self> {
        match s {
            "prompt" => Some(CommandKind::Prompt),
            "agent" => Some(CommandKind::Agent),
            "engine" => Some(CommandKind::Engine),
            "target" => Some(CommandKind::Target),
            "shell" => Some(CommandKind::Shell),
            _ => None,
        }
    }
}

/// A declarative command (commands/<name>.toml). Also feeds the
/// setMyCommands payload (built-ins + user commands).
#[derive(Debug, Clone)]
pub struct CommandDef {
    /// Without the leading slash (`deploy` -> `/deploy`).
    pub command: String,
    pub description: String,
    pub kind: CommandKind,
    /// kind=prompt: prompt template name.
    pub template: Option<String>,
    /// Optional: switch to this agent first (or the target of kind=agent).
    pub agent: Option<String>,
    /// kind=engine target engine name.
    pub engine: Option<String>,
    /// Optional target override (`gcp` | `mac`).
    pub target: Option<String>,
    /// kind=shell: the FIXED argv (program + args).
    pub argv: Option<Vec<String>>,
    /// true -> inline Yes/Cancel keyboard before running.
    pub confirm: bool,
}

impl CommandDef {
    /// Parse a `commands/<name>.toml` document; reserved names are rejected
    /// with an exact-string validation error.
    pub fn from_toml(name: &str, v: &toml::Value) -> Result<Self, String> {
        let table = v
            .as_table()
            .ok_or_else(|| "command file must be a TOML table".to_string())?;

        if !valid_command_name(name) {
            return Err(
                "command name must be 1-32 characters of lowercase letters, digits, or underscores"
                    .to_string(),
            );
        }
        if RESERVED.contains(&name) {
            return Err(format!("'{name}' is a reserved built-in command"));
        }

        let description = match opt_string(table, "description")? {
            Some(s) if !s.is_empty() => s,
            Some(_) => return Err("description cannot be empty".to_string()),
            None => return Err("description is required".to_string()),
        };

        let kind = match opt_string(table, "kind")? {
            None => return Err("kind is required".to_string()),
            Some(s) => CommandKind::from_toml_str(&s).ok_or_else(|| {
                "kind must be \"prompt\", \"agent\", \"engine\", \"target\", or \"shell\"".to_string()
            })?,
        };

        let template = opt_string(table, "template")?.filter(|s| !s.is_empty());
        let agent = opt_string(table, "agent")?.filter(|s| !s.is_empty());
        let engine = opt_string(table, "engine")?.filter(|s| !s.is_empty());
        let target = opt_string(table, "target")?.filter(|s| !s.is_empty());
        let argv = opt_string_list(table, "argv")?;

        let confirm = match table.get("confirm") {
            None => false,
            Some(toml::Value::Boolean(b)) => *b,
            Some(_) => return Err("confirm must be a boolean".to_string()),
        };

        // Wherever a target appears (kind=target or a pre-switch), it must
        // name one of the two runnable targets.
        if let Some(t) = &target {
            if !TARGETS.contains(&t.as_str()) {
                return Err("target must be \"gcp\" or \"mac\"".to_string());
            }
        }

        match kind {
            CommandKind::Prompt => {
                if template.is_none() {
                    return Err("template is required for kind = \"prompt\"".to_string());
                }
            }
            CommandKind::Agent => {
                if agent.is_none() {
                    return Err("agent is required for kind = \"agent\"".to_string());
                }
            }
            CommandKind::Engine => {
                if engine.is_none() {
                    return Err("engine is required for kind = \"engine\"".to_string());
                }
            }
            CommandKind::Target => {
                if target.is_none() {
                    return Err("target is required for kind = \"target\"".to_string());
                }
            }
            CommandKind::Shell => match &argv {
                None => return Err("argv is required for kind = \"shell\"".to_string()),
                Some(a) if a.is_empty() => return Err("argv cannot be empty".to_string()),
                Some(_) => {}
            },
        }

        Ok(CommandDef {
            command: name.to_string(),
            description,
            kind,
            template,
            agent,
            engine,
            target,
            argv,
            confirm,
        })
    }

    /// The argv for one kind=shell run: the FIXED argv with the user's args
    /// (trimmed; empty -> nothing) appended as ONE final element. The user
    /// text is never split, quoted, or passed through a shell — whatever was
    /// typed after the command arrives as a single argv element.
    pub fn shell_argv(&self, user_args: &str) -> Vec<String> {
        let mut out = self.argv.clone().unwrap_or_default();
        let trimmed = user_args.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
        out
    }
}

/// Telegram's bot-command name rule: 1-32 chars, lowercase ASCII letters,
/// digits and underscores only (the name comes from the file stem).
fn valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_COMMAND_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(name: &str, doc: &str) -> Result<CommandDef, String> {
        let v: toml::Value = doc.parse().expect("test TOML must parse");
        CommandDef::from_toml(name, &v)
    }

    // ---- happy paths per kind ----

    #[test]
    fn prompt_command_roundtrip() {
        let def = parse(
            "deploy",
            r#"
description = "Deploy to production"
kind = "prompt"
template = "deploy"
confirm = true
"#,
        )
        .expect("valid prompt command");
        assert_eq!(def.command, "deploy");
        assert_eq!(def.description, "Deploy to production");
        assert_eq!(def.kind, CommandKind::Prompt);
        assert_eq!(def.template.as_deref(), Some("deploy"));
        assert_eq!(def.agent, None);
        assert_eq!(def.engine, None);
        assert_eq!(def.target, None);
        assert_eq!(def.argv, None);
        assert!(def.confirm);
    }

    #[test]
    fn prompt_command_with_agent_and_target_pre_switch() {
        let def = parse(
            "review",
            r#"
description = "Review the latest PR"
kind = "prompt"
template = "review"
agent = "reviewer"
target = "mac"
"#,
        )
        .expect("prompt with pre-switch");
        assert_eq!(def.agent.as_deref(), Some("reviewer"));
        assert_eq!(def.target.as_deref(), Some("mac"));
        assert!(!def.confirm); // default
    }

    #[test]
    fn agent_command_roundtrip() {
        let def = parse(
            "rev",
            "description = \"Switch to the reviewer\"\nkind = \"agent\"\nagent = \"reviewer\"\n",
        )
        .expect("valid agent command");
        assert_eq!(def.kind, CommandKind::Agent);
        assert_eq!(def.agent.as_deref(), Some("reviewer"));
    }

    #[test]
    fn engine_command_roundtrip() {
        let def = parse(
            "aider",
            "description = \"Use Aider\"\nkind = \"engine\"\nengine = \"aider\"\n",
        )
        .expect("valid engine command");
        assert_eq!(def.kind, CommandKind::Engine);
        assert_eq!(def.engine.as_deref(), Some("aider"));
    }

    #[test]
    fn target_command_roundtrip() {
        let def = parse(
            "laptop",
            "description = \"Run on the Mac\"\nkind = \"target\"\ntarget = \"mac\"\n",
        )
        .expect("valid target command");
        assert_eq!(def.kind, CommandKind::Target);
        assert_eq!(def.target.as_deref(), Some("mac"));
    }

    #[test]
    fn shell_command_roundtrip() {
        let def = parse(
            "disk",
            "description = \"Disk usage\"\nkind = \"shell\"\nargv = [\"df\", \"-h\"]\n",
        )
        .expect("valid shell command");
        assert_eq!(def.kind, CommandKind::Shell);
        assert_eq!(def.argv.as_deref(), Some(&["df".to_string(), "-h".to_string()][..]));
    }

    // ---- reserved names ----

    #[test]
    fn every_reserved_name_is_rejected() {
        for name in RESERVED {
            let err = parse(
                name,
                "description = \"shadow attempt\"\nkind = \"prompt\"\ntemplate = \"x\"\n",
            )
            .unwrap_err();
            assert_eq!(err, format!("'{name}' is a reserved built-in command"));
        }
    }

    #[test]
    fn near_reserved_names_pass() {
        // Only exact matches are reserved.
        assert!(parse(
            "stop2",
            "description = \"d\"\nkind = \"prompt\"\ntemplate = \"t\"\n"
        )
        .is_ok());
        assert!(parse(
            "helpme",
            "description = \"d\"\nkind = \"prompt\"\ntemplate = \"t\"\n"
        )
        .is_ok());
    }

    // ---- command name validation ----

    #[test]
    fn command_name_charset_and_length() {
        let doc = "description = \"d\"\nkind = \"prompt\"\ntemplate = \"t\"\n";
        let msg =
            "command name must be 1-32 characters of lowercase letters, digits, or underscores";
        assert_eq!(parse("", doc).unwrap_err(), msg);
        assert_eq!(parse("Deploy", doc).unwrap_err(), msg);
        assert_eq!(parse("de-ploy", doc).unwrap_err(), msg);
        assert_eq!(parse("dep loy", doc).unwrap_err(), msg);
        assert_eq!(parse("dépl", doc).unwrap_err(), msg);
        assert_eq!(parse(&"a".repeat(33), doc).unwrap_err(), msg);
        assert!(parse(&"a".repeat(32), doc).is_ok());
        assert!(parse("deploy_2", doc).is_ok());
    }

    // ---- required keys and kind dispatch ----

    #[test]
    fn description_required_and_non_empty() {
        assert_eq!(
            parse("x", "kind = \"prompt\"\ntemplate = \"t\"\n").unwrap_err(),
            "description is required"
        );
        assert_eq!(
            parse("x", "description = \"\"\nkind = \"prompt\"\ntemplate = \"t\"\n").unwrap_err(),
            "description cannot be empty"
        );
        assert_eq!(
            parse("x", "description = 3\nkind = \"prompt\"\n").unwrap_err(),
            "description must be a string"
        );
    }

    #[test]
    fn kind_required_and_validated() {
        assert_eq!(
            parse("x", "description = \"d\"\n").unwrap_err(),
            "kind is required"
        );
        assert_eq!(
            parse("x", "description = \"d\"\nkind = \"magic\"\n").unwrap_err(),
            "kind must be \"prompt\", \"agent\", \"engine\", \"target\", or \"shell\""
        );
    }

    #[test]
    fn per_kind_requirements() {
        assert_eq!(
            parse("x", "description = \"d\"\nkind = \"prompt\"\n").unwrap_err(),
            "template is required for kind = \"prompt\""
        );
        // An empty template string is treated as absent.
        assert_eq!(
            parse("x", "description = \"d\"\nkind = \"prompt\"\ntemplate = \"\"\n").unwrap_err(),
            "template is required for kind = \"prompt\""
        );
        assert_eq!(
            parse("x", "description = \"d\"\nkind = \"agent\"\n").unwrap_err(),
            "agent is required for kind = \"agent\""
        );
        assert_eq!(
            parse("x", "description = \"d\"\nkind = \"engine\"\n").unwrap_err(),
            "engine is required for kind = \"engine\""
        );
        assert_eq!(
            parse("x", "description = \"d\"\nkind = \"target\"\n").unwrap_err(),
            "target is required for kind = \"target\""
        );
        assert_eq!(
            parse("x", "description = \"d\"\nkind = \"shell\"\n").unwrap_err(),
            "argv is required for kind = \"shell\""
        );
        assert_eq!(
            parse("x", "description = \"d\"\nkind = \"shell\"\nargv = []\n").unwrap_err(),
            "argv cannot be empty"
        );
    }

    #[test]
    fn target_values_validated_everywhere() {
        assert_eq!(
            parse(
                "x",
                "description = \"d\"\nkind = \"target\"\ntarget = \"moon\"\n"
            )
            .unwrap_err(),
            "target must be \"gcp\" or \"mac\""
        );
        // Also on a prompt pre-switch.
        assert_eq!(
            parse(
                "x",
                "description = \"d\"\nkind = \"prompt\"\ntemplate = \"t\"\ntarget = \"moon\"\n"
            )
            .unwrap_err(),
            "target must be \"gcp\" or \"mac\""
        );
    }

    #[test]
    fn argv_and_confirm_type_errors() {
        assert_eq!(
            parse("x", "description = \"d\"\nkind = \"shell\"\nargv = [1]\n").unwrap_err(),
            "argv must be an array of strings"
        );
        assert_eq!(
            parse(
                "x",
                "description = \"d\"\nkind = \"shell\"\nargv = [\"a\", \"\"]\n"
            )
            .unwrap_err(),
            "argv entries cannot be empty"
        );
        assert_eq!(
            parse(
                "x",
                "description = \"d\"\nkind = \"shell\"\nargv = [\"a\"]\nconfirm = \"yes\"\n"
            )
            .unwrap_err(),
            "confirm must be a boolean"
        );
    }

    #[test]
    fn non_table_document_rejected() {
        let v = toml::Value::String("no".to_string());
        assert_eq!(
            CommandDef::from_toml("x", &v).unwrap_err(),
            "command file must be a TOML table"
        );
    }

    // ---- shell_argv: args as ONE element, never shell-interpolated ----

    #[test]
    fn shell_argv_appends_user_args_as_one_element() {
        let def = parse(
            "deploy",
            "description = \"d\"\nkind = \"shell\"\nargv = [\"./deploy.sh\", \"--prod\"]\n",
        )
        .expect("shell command");
        assert_eq!(
            def.shell_argv("staging eu-west"),
            vec!["./deploy.sh", "--prod", "staging eu-west"]
        );
        // Shell metacharacters survive verbatim inside the single element.
        assert_eq!(
            def.shell_argv("x; rm -rf / && echo $(pwd) | tee"),
            vec!["./deploy.sh", "--prod", "x; rm -rf / && echo $(pwd) | tee"]
        );
    }

    #[test]
    fn shell_argv_empty_or_blank_args_append_nothing() {
        let def = parse(
            "disk",
            "description = \"d\"\nkind = \"shell\"\nargv = [\"df\", \"-h\"]\n",
        )
        .expect("shell command");
        assert_eq!(def.shell_argv(""), vec!["df", "-h"]);
        assert_eq!(def.shell_argv("   \t "), vec!["df", "-h"]);
        // Args are trimmed before appending.
        assert_eq!(def.shell_argv("  /home  "), vec!["df", "-h", "/home"]);
    }

    // ---- setMyCommands built-ins ----

    #[test]
    fn builtin_my_commands_match_coordinator_payload() {
        // Exact order + descriptions from coordinator.mjs setMyCommands.
        assert_eq!(
            BUILTIN_MY_COMMANDS,
            &[
                ("claude", "Use Claude Code 🧠"),
                ("codex", "Use Codex 🛠"),
                ("mac", "Run on the Mac 🖥️"),
                ("gcp", "Run on the GCP box ☁️"),
                ("where", "Show active target & session"),
                ("new", "Fresh session on active target"),
                ("stop", "Kill/cancel the running job"),
                ("menu", "Show tap-button controls"),
                ("help", "Show command list"),
            ]
        );
    }

    #[test]
    fn every_builtin_payload_name_is_reserved() {
        for (name, _) in BUILTIN_MY_COMMANDS {
            assert!(RESERVED.contains(name), "{name} must be reserved");
        }
    }

    #[test]
    fn kind_toml_spellings_roundtrip() {
        for kind in [
            CommandKind::Prompt,
            CommandKind::Agent,
            CommandKind::Engine,
            CommandKind::Target,
            CommandKind::Shell,
        ] {
            assert_eq!(CommandKind::from_toml_str(kind.as_toml_str()), Some(kind));
        }
        assert_eq!(CommandKind::from_toml_str("Prompt"), None);
        assert_eq!(CommandKind::from_toml_str(""), None);
    }
}
