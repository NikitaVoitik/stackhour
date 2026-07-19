//! Declarative Telegram commands.
//!
//! A command is a name, optional aliases, a description, an optional
//! positional [`ArgSpec`] list, and exactly one action:
//!
//! | `kind`     | action                                                    |
//! |------------|-----------------------------------------------------------|
//! | `prompt`   | render a named prompt template, route it like typed text  |
//! | `agent`    | switch the active named agent                             |
//! | `engine`   | switch the active engine                                  |
//! | `target`   | switch the active target (`gcp` / `mac`)                  |
//! | `shell`    | run a FIXED argv, never shell-interpolated                |
//! | `skill`    | invoke a named skill with the bound args                  |
//! | `sequence` | run other commands in order, aborting on first failure    |
//! | `builtin`  | EMBEDDED ONLY — the irreducible bridge verbs              |
//!
//! The nine verbs the JS coordinator registered with Telegram now live in
//! [`builtin_commands`], parsed from the embedded TOML text in
//! [`BUILTIN_COMMANDS_TOML`], so the shipped table and a user's table are
//! literally the same schema and `bridge init` can write that text out
//! verbatim.
//!
//! **Behavioural change vs. the JS bridge:** only `/start`, `/help`, `/menu`
//! and `/stop` are still [`RESERVED`]. Every other built-in verb (`claude`,
//! `codex`, `mac`, `gcp`, `where`, `new`, and the `local` / `remote` /
//! `status` / `reset` aliases) can now be shadowed by a user file of the same
//! name, which takes its slot in `/help`, the keyboard and `setMyCommands`.
//!
//! `commands/<name>.toml` schema (`kind` and `description` always required):
//!
//! ```toml
//! description  = "Deploy to production"  # required; shown by setMyCommands
//! aliases      = ["ship"]                # optional extra names
//! hidden       = false                   # registered, but not in /help
//! keyboard     = false                   # also render an inline button
//! button       = "🚀 Deploy"             # button caption (default: description)
//! button_order = 30                      # keyboard sort key (default: table pos)
//! confirm      = true                    # inline Yes/Cancel before running
//! kind         = "prompt"
//! template     = "deploy"                # kind=prompt
//! agent        = "reviewer"              # kind=agent, or an optional pre-switch
//! engine       = "codex"                 # kind=engine
//! target       = "gcp"                   # kind=target ("gcp" | "mac")
//! argv         = ["./deploy.sh"]         # kind=shell, non-empty
//! skill        = "review"                # kind=skill
//! steps        = ["build", "deploy"]     # kind=sequence, non-empty
//!
//! [[args]]
//! name     = "env"
//! required = true
//! choices  = ["staging", "prod"]
//! ```
//!
//! Unknown keys are ignored (forward compatibility). Every validation error is
//! a [`FieldError`] naming the offending key; the loader attaches the file. A
//! broken command file is skipped and recorded, never fatal. Cross-references
//! (`agent`, `engine`, `template`, `skill`, `steps`) and `steps` cycles are
//! checked by the loader.

use indexmap::IndexMap;

use super::args::{self, ArgSpec};
use super::cycle;
use super::error::FieldError;
use super::toml_util::{self, Table};

/// The built-in command names that can NEVER be shadowed by a user file.
///
/// Deliberately tiny: these are the verbs the bridge cannot recover without.
/// `/start` is an alias of `/help`, so it is reserved too.
pub const RESERVED: &[&str] = &["start", "help", "menu", "stop"];

/// Telegram bot-command name limit (`setMyCommands`: 1-32 chars of lowercase
/// English letters, digits and underscores).
const MAX_COMMAND_NAME_LEN: usize = 32;

/// The two runnable targets a command may switch to.
const TARGETS: &[&str] = &["gcp", "mac"];

/// What a declarative command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// Render `template` and route it exactly like typed text.
    Prompt,
    /// Switch the active named agent.
    Agent,
    /// Switch the active engine.
    Engine,
    /// Switch the active target.
    Target,
    /// Run a fixed argv; stdout is replied to the chat.
    Shell,
    /// Invoke a named skill with the bound args.
    Skill,
    /// Run other commands in order, aborting on the first failure.
    Sequence,
    /// EMBEDDED ONLY: one of the irreducible bridge verbs. Rejected in user
    /// files — a user can shadow a built-in, not invent bridge internals.
    Builtin,
}

impl CommandKind {
    /// The TOML spelling of this kind (`kind = "…"`).
    pub fn as_toml_str(self) -> &'static str {
        match self {
            CommandKind::Prompt => "prompt",
            CommandKind::Agent => "agent",
            CommandKind::Engine => "engine",
            CommandKind::Target => "target",
            CommandKind::Shell => "shell",
            CommandKind::Skill => "skill",
            CommandKind::Sequence => "sequence",
            CommandKind::Builtin => "builtin",
        }
    }

    fn from_toml_str(s: &str) -> Option<Self> {
        match s {
            "prompt" => Some(CommandKind::Prompt),
            "agent" => Some(CommandKind::Agent),
            "engine" => Some(CommandKind::Engine),
            "target" => Some(CommandKind::Target),
            "shell" => Some(CommandKind::Shell),
            "skill" => Some(CommandKind::Skill),
            "sequence" => Some(CommandKind::Sequence),
            "builtin" => Some(CommandKind::Builtin),
            _ => None,
        }
    }
}

/// The `kind` values a USER may write (`builtin` is embedded-only).
pub const USER_KINDS: &[&str] = &[
    "prompt", "agent", "engine", "target", "shell", "skill", "sequence",
];

/// The irreducible bridge verbs `kind = "builtin"` may name.
pub const BUILTIN_VERBS: &[&str] = &["help", "menu", "where", "new", "stop"];

// ---------------------------------------------------------------------------
// CommandDef
// ---------------------------------------------------------------------------

/// A declarative command (`commands/<name>.toml`, or an embedded default).
#[derive(Debug, Clone)]
pub struct CommandDef {
    /// Without the leading slash (`deploy` -> `/deploy`).
    pub command: String,
    pub description: String,
    /// Extra names that dispatch to this command. Never registered with
    /// Telegram separately.
    pub aliases: Vec<String>,
    /// Registered, but omitted from `/help` and the keyboard.
    pub hidden: bool,
    /// Also render as an inline-keyboard button.
    pub keyboard: bool,
    /// Button caption when `keyboard = true` (default: the description).
    pub button: Option<String>,
    /// Keyboard sort key; buttons are laid out two per row in this order.
    pub button_order: Option<i64>,
    pub kind: CommandKind,
    /// kind=prompt: prompt template name.
    pub template: Option<String>,
    /// kind=agent target, or an optional pre-switch for any other kind.
    pub agent: Option<String>,
    /// kind=engine target engine name.
    pub engine: Option<String>,
    /// Optional target override (`gcp` | `mac`).
    pub target: Option<String>,
    /// kind=shell: the FIXED argv (program + args).
    pub argv: Option<Vec<String>>,
    /// kind=skill: the skill name.
    pub skill: Option<String>,
    /// kind=sequence: names of other commands, run in order.
    pub steps: Vec<String>,
    /// kind=builtin: the bridge verb.
    pub builtin: Option<String>,
    /// Positional argument spec.
    pub args: Vec<ArgSpec>,
    /// true -> inline Yes/Cancel keyboard before running.
    pub confirm: bool,
}

impl Default for CommandDef {
    /// An inert command: no action, no presentation. A base for struct-update
    /// syntax in tests and in callers building a def by hand.
    fn default() -> Self {
        CommandDef {
            command: String::new(),
            description: String::new(),
            aliases: Vec::new(),
            hidden: false,
            keyboard: false,
            button: None,
            button_order: None,
            kind: CommandKind::Prompt,
            template: None,
            agent: None,
            engine: None,
            target: None,
            argv: None,
            skill: None,
            steps: Vec::new(),
            builtin: None,
            args: Vec::new(),
            confirm: false,
        }
    }
}

impl CommandDef {
    /// Parse a `commands/<name>.toml` document written by a USER: `kind =
    /// "builtin"` is rejected and [`RESERVED`] names are refused.
    pub fn parse(name: &str, v: &toml::Value) -> Result<Self, FieldError> {
        let def = Self::parse_inner(name, v)?;
        if def.kind == CommandKind::Builtin {
            return Err(FieldError::key(
                "kind",
                "\"builtin\" is reserved for the commands shipped with the bridge",
            ));
        }
        Ok(def)
    }

    /// The loader entry point: the same validation, with the error rendered
    /// as `key: message` (the file lives in `RegistryError::file`).
    pub fn from_toml(name: &str, v: &toml::Value) -> Result<Self, String> {
        Self::parse(name, v).map_err(|e| e.message())
    }

    fn parse_inner(name: &str, v: &toml::Value) -> Result<Self, FieldError> {
        let table = toml_util::root_table(v, "command file")?;

        if !valid_command_name(name) {
            return Err(FieldError::file_level(format!(
                "command name '{name}' must be 1-{MAX_COMMAND_NAME_LEN} characters of lowercase \
                 letters, digits, or underscores"
            )));
        }
        if RESERVED.contains(&name) {
            return Err(FieldError::file_level(format!(
                "'{name}' is a reserved built-in command and cannot be redefined (reserved: {})",
                RESERVED.join(", ")
            )));
        }

        let description = toml_util::req_string(table, "description")?;

        let kind_str = toml_util::req_string(table, "kind")?;
        let kind = CommandKind::from_toml_str(&kind_str).ok_or_else(|| {
            FieldError::key("kind", format!("unknown kind '{kind_str}'"))
                .with_known(USER_KINDS)
        })?;

        let nonempty = |key: &str| -> Result<Option<String>, FieldError> {
            toml_util::opt_nonempty_string(table, key)
        };
        let template = nonempty("template")?;
        let agent = nonempty("agent")?;
        let engine = nonempty("engine")?;
        let target = nonempty("target")?;
        let skill = nonempty("skill")?;
        let builtin = nonempty("builtin")?;
        let button = nonempty("button")?;

        let argv = toml_util::opt_string_list(table, "argv")?;
        let steps = toml_util::string_list(table, "steps")?;
        let aliases = toml_util::string_list(table, "aliases")?;

        let confirm = toml_util::opt_bool(table, "confirm", false)?;
        let hidden = toml_util::opt_bool(table, "hidden", false)?;
        let keyboard = toml_util::opt_bool(table, "keyboard", false)?;

        let button_order = match table.get("button_order") {
            None => None,
            Some(toml::Value::Integer(i)) => Some(*i),
            Some(other) => {
                return Err(FieldError::key(
                    "button_order",
                    format!("must be an integer (got {})", toml_util::type_name(other)),
                ))
            }
        };

        validate_aliases(name, &aliases)?;
        let args = args::parse_arg_specs(table)?;

        // Wherever a target appears (kind=target or a pre-switch), it must
        // name one of the two runnable targets.
        if let Some(t) = &target {
            if !TARGETS.contains(&t.as_str()) {
                return Err(
                    FieldError::key("target", format!("unknown target '{t}'")).with_known(TARGETS)
                );
            }
        }

        let require = |value: &Option<String>, key: &'static str| -> Result<(), FieldError> {
            match value {
                Some(_) => Ok(()),
                None => Err(FieldError::key(
                    key,
                    format!(
                        "is required for kind = \"{}\" and must be a non-empty string",
                        kind.as_toml_str()
                    ),
                )),
            }
        };

        match kind {
            CommandKind::Prompt => require(&template, "template")?,
            CommandKind::Agent => require(&agent, "agent")?,
            CommandKind::Engine => require(&engine, "engine")?,
            CommandKind::Target => require(&target, "target")?,
            CommandKind::Skill => require(&skill, "skill")?,
            CommandKind::Shell => match &argv {
                None => {
                    return Err(FieldError::key(
                        "argv",
                        "is required for kind = \"shell\" and must be a non-empty array of strings",
                    ))
                }
                Some(a) if a.is_empty() => {
                    return Err(FieldError::key("argv", "must not be empty"))
                }
                Some(a) if a.iter().any(|e| e.is_empty()) => {
                    return Err(FieldError::key("argv", "entries must not be empty"))
                }
                Some(_) => {}
            },
            CommandKind::Sequence => {
                if steps.is_empty() {
                    return Err(FieldError::key(
                        "steps",
                        "is required for kind = \"sequence\" and must be a non-empty array of \
                         command names",
                    ));
                }
                if steps.len() > cycle::MAX_DEPTH {
                    return Err(FieldError::key(
                        "steps",
                        format!("must not list more than {} steps", cycle::MAX_DEPTH),
                    ));
                }
                if steps.iter().any(|s| s == name) {
                    return Err(FieldError::key(
                        "steps",
                        format!("'{name}' must not list itself as a step"),
                    ));
                }
            }
            CommandKind::Builtin => match &builtin {
                None => {
                    return Err(FieldError::key(
                        "builtin",
                        "is required for kind = \"builtin\"",
                    ))
                }
                Some(b) if !BUILTIN_VERBS.contains(&b.as_str()) => {
                    return Err(
                        FieldError::key("builtin", format!("unknown bridge verb '{b}'"))
                            .with_known(BUILTIN_VERBS),
                    )
                }
                Some(_) => {}
            },
        }

        Ok(CommandDef {
            command: name.to_string(),
            description,
            aliases,
            hidden,
            keyboard,
            button,
            button_order,
            kind,
            template,
            agent,
            engine,
            target,
            argv,
            skill,
            steps,
            builtin,
            args,
            confirm,
        })
    }

    /// Every name this command answers to, in dispatch order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.command.as_str()).chain(self.aliases.iter().map(String::as_str))
    }

    /// The button caption: explicit `button`, else the description.
    pub fn button_text(&self) -> &str {
        self.button.as_deref().unwrap_or(&self.description)
    }

    /// The generated usage line, e.g. `/deploy <env> [note...]`. A command
    /// with no `[[args]]` renders as just `/name`.
    pub fn usage(&self) -> String {
        if self.args.is_empty() {
            format!("/{}", self.command)
        } else {
            format!("/{} {}", self.command, args::usage(&self.args))
        }
    }

    /// Bind a raw Telegram argument string to this command's spec. Always
    /// includes `args`, the raw untouched string, for back compatibility.
    pub fn bind(&self, raw: &str) -> Result<IndexMap<String, String>, FieldError> {
        args::bind_args(&self.args, raw)
    }

    /// The argv for one `kind = "shell"` run.
    ///
    /// Two documented modes, neither of which ever reaches a shell:
    ///
    /// * **No `[[args]]`** — legacy behaviour, byte-compatible with the shape
    ///   the JS bridge would have used: the trimmed raw argument string is
    ///   appended as ONE final element, verbatim.
    /// * **With `[[args]]`** — each fixed argv element gets `{{name}}`
    ///   substituted per element; any argument NOT referenced by the template
    ///   is then appended as its OWN separate element, in declaration order,
    ///   skipping empty values. This is a deliberate change from the single
    ///   trailing blob, and is why an args spec is opt-in.
    pub fn shell_argv(&self, raw: &str) -> Result<Vec<String>, FieldError> {
        let fixed = self.argv.clone().unwrap_or_default();
        if self.args.is_empty() {
            let mut out = fixed;
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            return Ok(out);
        }

        let bound = self.bind(raw)?;
        let mut out: Vec<String> = Vec::with_capacity(fixed.len() + self.args.len());
        let mut used: Vec<&str> = Vec::new();
        for element in &fixed {
            let mut e = element.clone();
            for spec in &self.args {
                let needle = format!("{{{{{}}}}}", spec.name);
                if e.contains(&needle) {
                    e = e.replace(&needle, bound.get(&spec.name).map(String::as_str).unwrap_or(""));
                    used.push(spec.name.as_str());
                }
            }
            out.push(e);
        }
        for spec in &self.args {
            if used.contains(&spec.name.as_str()) {
                continue;
            }
            match bound.get(&spec.name) {
                Some(v) if !v.is_empty() => out.push(v.clone()),
                _ => {}
            }
        }
        Ok(out)
    }
}

/// Alias rules checkable from ONE file: valid name, not the command's own
/// name, not reserved, not listed twice. Cross-file collisions are
/// [`alias_conflicts`].
fn validate_aliases(name: &str, aliases: &[String]) -> Result<(), FieldError> {
    for (i, alias) in aliases.iter().enumerate() {
        if !valid_command_name(alias) {
            return Err(FieldError::key(
                "aliases",
                format!(
                    "'{alias}' must be 1-{MAX_COMMAND_NAME_LEN} characters of lowercase letters, \
                     digits, or underscores"
                ),
            ));
        }
        if alias == name {
            return Err(FieldError::key(
                "aliases",
                format!("'{alias}' duplicates the command's own name"),
            ));
        }
        if RESERVED.contains(&alias.as_str()) {
            return Err(FieldError::key(
                "aliases",
                format!("'{alias}' is a reserved built-in command"),
            ));
        }
        if aliases[..i].contains(alias) {
            return Err(FieldError::key(
                "aliases",
                format!("'{alias}' is listed twice"),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Embedded defaults
// ---------------------------------------------------------------------------

/// The command table shipped with the bridge, as the exact TOML text
/// `bridge init --config-dir` writes into `commands/`. Parsed at startup, so
/// the embedded layer and the on-disk layer cannot drift.
///
/// Order is the `setMyCommands` order the JS coordinator used; `button_order`
/// reproduces its control keyboard row for row.
pub const BUILTIN_COMMANDS_TOML: &[(&str, &str)] = &[
    (
        "claude",
        r#"description = "Use Claude Code 🧠"
kind = "engine"
engine = "claude"
keyboard = true
button = "🧠 Claude"
button_order = 10
"#,
    ),
    (
        "codex",
        r#"description = "Use Codex 🛠"
kind = "engine"
engine = "codex"
keyboard = true
button = "🛠 Codex"
button_order = 11
"#,
    ),
    (
        "mac",
        r#"description = "Run on the Mac 🖥️"
kind = "target"
target = "mac"
aliases = ["local"]
keyboard = true
button = "🖥️ Mac"
button_order = 20
"#,
    ),
    (
        "gcp",
        r#"description = "Run on the GCP box ☁️"
kind = "target"
target = "gcp"
aliases = ["remote"]
keyboard = true
button = "☁️ GCP"
button_order = 21
"#,
    ),
    (
        "where",
        r#"description = "Show active target & session"
kind = "builtin"
builtin = "where"
aliases = ["status"]
keyboard = true
button = "ℹ️ Status"
button_order = 31
"#,
    ),
    (
        "new",
        r#"description = "Fresh session on active target"
kind = "builtin"
builtin = "new"
aliases = ["reset"]
keyboard = true
button = "🆕 New session"
button_order = 30
"#,
    ),
    (
        "stop",
        r#"description = "Kill/cancel the running job"
kind = "builtin"
builtin = "stop"
"#,
    ),
    (
        "menu",
        r#"description = "Show tap-button controls"
kind = "builtin"
builtin = "menu"
"#,
    ),
    (
        "help",
        r#"description = "Show command list"
kind = "builtin"
builtin = "help"
aliases = ["start"]
"#,
    ),
];

/// The shipped command table, parsed from [`BUILTIN_COMMANDS_TOML`].
///
/// Panics only if the embedded text is malformed, which
/// `embedded_table_parses_and_matches_the_js_payload` makes impossible to
/// ship.
pub fn builtin_commands() -> IndexMap<String, CommandDef> {
    let mut out = IndexMap::with_capacity(BUILTIN_COMMANDS_TOML.len());
    for (name, text) in BUILTIN_COMMANDS_TOML {
        let value: toml::Value = text
            .parse()
            .unwrap_or_else(|e| panic!("embedded command '{name}' is not valid TOML: {e}"));
        let def = CommandDef::parse_inner(name, &value)
            .unwrap_or_else(|e| panic!("embedded command '{name}' is invalid: {e}"));
        out.insert(def.command.clone(), def);
    }
    out
}

/// The effective command table: the shipped commands with any user command of
/// the same name substituted IN PLACE (keeping its `/help`, keyboard and
/// `setMyCommands` slot), followed by the remaining user commands in load
/// order.
pub fn effective_table(user: &IndexMap<String, CommandDef>) -> IndexMap<String, CommandDef> {
    let mut out = builtin_commands();
    for (name, def) in user {
        // IndexMap::insert on an existing key replaces the value and keeps
        // the entry's position — that is what preserves the slot.
        out.insert(name.clone(), def.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Table-level validation (called by the loader)
// ---------------------------------------------------------------------------

/// Detect alias collisions across the WHOLE table: an alias must not equal
/// another command's name, another command's alias, or a reserved verb.
///
/// Returns `(command name, error)` pairs, one per collision, in table order.
/// The loader attaches the file and drops the offending command.
pub fn alias_conflicts(table: &IndexMap<String, CommandDef>) -> Vec<(String, FieldError)> {
    let mut owner: IndexMap<&str, &str> = IndexMap::new();
    let mut out: Vec<(String, FieldError)> = Vec::new();

    for def in table.values() {
        owner.insert(def.command.as_str(), def.command.as_str());
    }
    for def in table.values() {
        for alias in &def.aliases {
            if RESERVED.contains(&alias.as_str()) {
                out.push((
                    def.command.clone(),
                    FieldError::key(
                        "aliases",
                        format!("'{alias}' is a reserved built-in command"),
                    ),
                ));
                continue;
            }
            match owner.get(alias.as_str()) {
                Some(other) if *other != def.command.as_str() => out.push((
                    def.command.clone(),
                    FieldError::key(
                        "aliases",
                        format!("'{alias}' is already taken by /{other}"),
                    ),
                )),
                Some(_) => {}
                None => {
                    owner.insert(alias.as_str(), def.command.as_str());
                }
            }
        }
    }
    out
}

/// Cycles in the `kind = "sequence"` graph, via the loader's shared detector.
///
/// Each entry is a path of the form `[a, b, a]`. Steps naming an unknown
/// command are ignored here — that is a cross-reference error with a much
/// better message.
pub fn sequence_cycles(table: &IndexMap<String, CommandDef>) -> Vec<Vec<String>> {
    cycle::detect_cycles(table.keys().map(String::as_str), |name| {
        table
            .get(name)
            .filter(|d| d.kind == CommandKind::Sequence)
            .map(|d| d.steps.clone())
            .unwrap_or_default()
    })
}

/// Resolve one typed name to its command, in dispatch order: exact command
/// name first, then aliases in table order.
pub fn lookup<'a>(table: &'a IndexMap<String, CommandDef>, name: &str) -> Option<&'a CommandDef> {
    if let Some(def) = table.get(name) {
        return Some(def);
    }
    table.values().find(|d| d.names().any(|n| n == name))
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

/// A parsed `[[args]]`-free view used by callers that only have a table.
pub fn table_of(v: &toml::Value) -> Result<&Table, FieldError> {
    toml_util::root_table(v, "command file")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(name: &str, doc: &str) -> Result<CommandDef, String> {
        let v: toml::Value = doc.parse().expect("test TOML must parse");
        CommandDef::from_toml(name, &v)
    }

    fn ok(name: &str, doc: &str) -> CommandDef {
        parse(name, doc).expect("valid command")
    }

    fn table_from(defs: Vec<CommandDef>) -> IndexMap<String, CommandDef> {
        defs.into_iter().map(|d| (d.command.clone(), d)).collect()
    }

    fn seq(name: &str, steps: &[&str]) -> CommandDef {
        CommandDef {
            command: name.to_string(),
            description: "d".into(),
            kind: CommandKind::Sequence,
            steps: steps.iter().map(|s| s.to_string()).collect(),
            ..CommandDef::default()
        }
    }

    // ---- happy paths per kind ----

    #[test]
    fn prompt_command_roundtrip() {
        let def = ok(
            "deploy",
            r#"
description = "Deploy to production"
kind = "prompt"
template = "deploy"
confirm = true
"#,
        );
        assert_eq!(def.command, "deploy");
        assert_eq!(def.description, "Deploy to production");
        assert_eq!(def.kind, CommandKind::Prompt);
        assert_eq!(def.template.as_deref(), Some("deploy"));
        assert!(def.aliases.is_empty());
        assert!(!def.hidden && !def.keyboard);
        assert!(def.args.is_empty());
        assert!(def.confirm);
    }

    #[test]
    fn every_user_kind_parses() {
        assert_eq!(
            ok("a", "description=\"d\"\nkind=\"agent\"\nagent=\"reviewer\"\n").kind,
            CommandKind::Agent
        );
        assert_eq!(
            ok("e", "description=\"d\"\nkind=\"engine\"\nengine=\"codex\"\n").kind,
            CommandKind::Engine
        );
        assert_eq!(
            ok("t", "description=\"d\"\nkind=\"target\"\ntarget=\"mac\"\n").kind,
            CommandKind::Target
        );
        assert_eq!(
            ok("s", "description=\"d\"\nkind=\"shell\"\nargv=[\"df\",\"-h\"]\n").argv,
            Some(vec!["df".to_string(), "-h".to_string()])
        );
        assert_eq!(
            ok("k", "description=\"d\"\nkind=\"skill\"\nskill=\"review\"\n").skill,
            Some("review".to_string())
        );
        assert_eq!(
            ok("q", "description=\"d\"\nkind=\"sequence\"\nsteps=[\"a\",\"b\"]\n").steps,
            vec!["a", "b"]
        );
    }

    #[test]
    fn presentation_keys_roundtrip() {
        let def = ok(
            "deploy",
            r#"
description = "Deploy"
kind = "shell"
argv = ["./deploy.sh"]
aliases = ["ship", "push"]
hidden = true
keyboard = true
button = "🚀 Go"
button_order = 5
"#,
        );
        assert_eq!(def.aliases, vec!["ship", "push"]);
        assert!(def.hidden && def.keyboard);
        assert_eq!(def.button_text(), "🚀 Go");
        assert_eq!(def.button_order, Some(5));
        assert_eq!(def.names().collect::<Vec<_>>(), vec!["deploy", "ship", "push"]);
    }

    #[test]
    fn button_defaults_to_description() {
        let def = ok(
            "x",
            "description=\"Disk usage\"\nkind=\"shell\"\nargv=[\"df\"]\nkeyboard=true\n",
        );
        assert_eq!(def.button_text(), "Disk usage");
    }

    // ---- reserved + name validation ----

    #[test]
    fn reserved_is_only_the_four_unrecoverable_verbs() {
        assert_eq!(RESERVED, &["start", "help", "menu", "stop"]);
        for name in RESERVED {
            let err = parse(
                name,
                "description = \"shadow\"\nkind = \"prompt\"\ntemplate = \"x\"\n",
            )
            .unwrap_err();
            assert_eq!(
                err,
                format!(
                    "'{name}' is a reserved built-in command and cannot be redefined \
                     (reserved: start, help, menu, stop)"
                )
            );
        }
    }

    #[test]
    fn former_builtins_can_now_be_shadowed() {
        // The single largest behavioural change: these were unshadowable.
        for name in [
            "claude", "codex", "mac", "gcp", "where", "new", "local", "remote", "status", "reset",
        ] {
            assert!(
                parse(name, "description=\"mine\"\nkind=\"shell\"\nargv=[\"true\"]\n").is_ok(),
                "/{name} should be shadowable now"
            );
        }
    }

    #[test]
    fn command_name_charset_and_length() {
        let doc = "description = \"d\"\nkind = \"prompt\"\ntemplate = \"t\"\n";
        for bad in ["", "Deploy", "de-ploy", "dépl", &"a".repeat(33)] {
            let err = parse(bad, doc).unwrap_err();
            assert!(
                err.contains("must be 1-32 characters of lowercase letters, digits, or underscores"),
                "name '{bad}' -> {err}"
            );
        }
        assert!(parse(&"a".repeat(32), doc).is_ok());
        assert!(parse("deploy_2", doc).is_ok());
    }

    // ---- errors name the key and say what was expected ----

    #[test]
    fn errors_carry_the_key_and_render_with_the_file() {
        let v: toml::Value = "description=\"d\"\nkind=\"target\"\ntarget=\"moon\"\n"
            .parse()
            .unwrap();
        let err = CommandDef::parse("x", &v).unwrap_err();
        assert_eq!(err.key, "target");
        assert_eq!(err.msg, "unknown target 'moon' (known: gcp, mac)");
        assert_eq!(
            err.in_file("commands/x.toml").to_string(),
            "commands/x.toml: key `target`: unknown target 'moon' (known: gcp, mac)"
        );
    }

    #[test]
    fn unknown_kind_lists_the_user_kinds() {
        assert_eq!(
            parse("x", "description=\"d\"\nkind=\"magic\"\n").unwrap_err(),
            "key `kind`: unknown kind 'magic' (known: prompt, agent, engine, target, shell, \
             skill, sequence)"
        );
    }

    #[test]
    fn per_kind_requirements() {
        for (doc, expected) in [
            (
                "description=\"d\"\nkind=\"prompt\"\n",
                "key `template`: is required for kind = \"prompt\" and must be a non-empty string",
            ),
            (
                "description=\"d\"\nkind=\"agent\"\n",
                "key `agent`: is required for kind = \"agent\" and must be a non-empty string",
            ),
            (
                "description=\"d\"\nkind=\"engine\"\n",
                "key `engine`: is required for kind = \"engine\" and must be a non-empty string",
            ),
            (
                "description=\"d\"\nkind=\"target\"\n",
                "key `target`: is required for kind = \"target\" and must be a non-empty string",
            ),
            (
                "description=\"d\"\nkind=\"skill\"\n",
                "key `skill`: is required for kind = \"skill\" and must be a non-empty string",
            ),
            (
                "description=\"d\"\nkind=\"shell\"\n",
                "key `argv`: is required for kind = \"shell\" and must be a non-empty array of \
                 strings",
            ),
            (
                "description=\"d\"\nkind=\"shell\"\nargv=[]\n",
                "key `argv`: must not be empty",
            ),
            (
                "description=\"d\"\nkind=\"shell\"\nargv=[\"a\",\"\"]\n",
                "key `argv`: entries must not be empty",
            ),
            (
                "description=\"d\"\nkind=\"sequence\"\n",
                "key `steps`: is required for kind = \"sequence\" and must be a non-empty array \
                 of command names",
            ),
        ] {
            assert_eq!(parse("x", doc).unwrap_err(), expected, "doc: {doc}");
        }
    }

    #[test]
    fn description_required_and_non_empty() {
        assert_eq!(
            parse("x", "kind=\"prompt\"\ntemplate=\"t\"\n").unwrap_err(),
            "key `description`: is required and must be a non-empty string"
        );
        assert_eq!(
            parse("x", "description=\"\"\nkind=\"prompt\"\ntemplate=\"t\"\n").unwrap_err(),
            "key `description`: must be a non-empty string"
        );
        assert!(parse("x", "description=3\nkind=\"prompt\"\n")
            .unwrap_err()
            .starts_with("key `description`:"));
    }

    #[test]
    fn builtin_kind_is_rejected_in_user_files() {
        assert_eq!(
            parse("mine", "description=\"d\"\nkind=\"builtin\"\nbuiltin=\"help\"\n").unwrap_err(),
            "key `kind`: \"builtin\" is reserved for the commands shipped with the bridge"
        );
    }

    #[test]
    fn sequence_self_reference_and_depth_cap() {
        assert_eq!(
            parse("loop", "description=\"d\"\nkind=\"sequence\"\nsteps=[\"a\",\"loop\"]\n")
                .unwrap_err(),
            "key `steps`: 'loop' must not list itself as a step"
        );
        let many: Vec<String> = (0..17).map(|i| format!("\"s{i}\"")).collect();
        assert_eq!(
            parse(
                "big",
                &format!("description=\"d\"\nkind=\"sequence\"\nsteps=[{}]\n", many.join(","))
            )
            .unwrap_err(),
            "key `steps`: must not list more than 16 steps"
        );
    }

    #[test]
    fn non_table_document_rejected() {
        assert_eq!(
            CommandDef::from_toml("x", &toml::Value::String("no".into())).unwrap_err(),
            "command file must be a TOML table"
        );
    }

    #[test]
    fn button_order_type_error_names_the_key() {
        assert_eq!(
            parse(
                "x",
                "description=\"d\"\nkind=\"shell\"\nargv=[\"a\"]\nbutton_order=\"5\"\n"
            )
            .unwrap_err(),
            "key `button_order`: must be an integer (got a string)"
        );
    }

    // ---- aliases ----

    #[test]
    fn alias_validation_within_one_file() {
        let base = "description=\"d\"\nkind=\"shell\"\nargv=[\"true\"]\n";
        assert!(parse("x", &format!("{base}aliases=[\"Ship\"]\n"))
            .unwrap_err()
            .starts_with("key `aliases`: 'Ship' must be 1-32 characters"));
        assert_eq!(
            parse("x", &format!("{base}aliases=[\"x\"]\n")).unwrap_err(),
            "key `aliases`: 'x' duplicates the command's own name"
        );
        assert_eq!(
            parse("x", &format!("{base}aliases=[\"help\"]\n")).unwrap_err(),
            "key `aliases`: 'help' is a reserved built-in command"
        );
        assert_eq!(
            parse("x", &format!("{base}aliases=[\"a\",\"a\"]\n")).unwrap_err(),
            "key `aliases`: 'a' is listed twice"
        );
    }

    #[test]
    fn alias_conflicts_across_the_table() {
        let sh = |n: &str, a: &str| {
            ok(
                n,
                &format!("description=\"d\"\nkind=\"shell\"\nargv=[\"true\"]\naliases=[\"{a}\"]\n"),
            )
        };
        let conflicts = alias_conflicts(&table_from(vec![
            sh("deploy", "ship"),
            sh("release", "ship"),
            sh("other", "deploy"),
        ]));
        assert_eq!(
            conflicts
                .iter()
                .map(|(n, e)| (n.as_str(), e.msg.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("release", "'ship' is already taken by /deploy"),
                ("other", "'deploy' is already taken by /deploy"),
            ]
        );
    }

    #[test]
    fn no_alias_conflicts_in_the_shipped_table() {
        assert!(alias_conflicts(&builtin_commands()).is_empty());
    }

    #[test]
    fn lookup_resolves_names_then_aliases() {
        let t = builtin_commands();
        for (typed, expected) in [
            ("gcp", "gcp"),
            ("remote", "gcp"),
            ("local", "mac"),
            ("status", "where"),
            ("reset", "new"),
            ("start", "help"),
        ] {
            assert_eq!(lookup(&t, typed).unwrap().command, expected, "typed {typed}");
        }
        assert!(lookup(&t, "nope").is_none());
    }

    // ---- args ----

    #[test]
    fn arg_spec_parses_and_renders_usage() {
        let def = ok(
            "deploy",
            r#"
description = "Deploy"
kind = "prompt"
template = "deploy"

[[args]]
name = "env"
required = true
choices = ["staging", "prod"]
description = "target environment"

[[args]]
name = "note"
rest = true
"#,
        );
        assert_eq!(def.args.len(), 2);
        assert_eq!(def.args[0].name, "env");
        assert!(def.args[0].required);
        assert!(def.args[1].rest);
        assert_eq!(def.usage(), "/deploy <env> [note...]");
        assert_eq!(ok("x", "description=\"d\"\nkind=\"shell\"\nargv=[\"a\"]\n").usage(), "/x");
    }

    #[test]
    fn arg_spec_errors_are_scoped_to_the_offending_entry() {
        let base = "description=\"d\"\nkind=\"shell\"\nargv=[\"true\"]\n";
        assert!(parse("x", &format!("{base}[[args]]\nrequired=true\n"))
            .unwrap_err()
            .starts_with("key `args[0].name`:"));
        assert!(parse("x", &format!("{base}[[args]]\nname=\"A\"\n"))
            .unwrap_err()
            .starts_with("key `args[0].name`:"));
        assert!(parse(
            "x",
            &format!("{base}[[args]]\nname=\"a\"\nrest=true\n[[args]]\nname=\"b\"\n")
        )
        .unwrap_err()
        .starts_with("key `args[0].rest`:"));
        assert!(parse("x", &format!("{base}[[args]]\nname=\"a\"\n[[args]]\nname=\"a\"\n"))
            .unwrap_err()
            .starts_with("key `args[1].name`:"));
    }

    #[test]
    fn bind_returns_named_values_plus_the_raw_string() {
        let def = ok(
            "deploy",
            "description=\"d\"\nkind=\"prompt\"\ntemplate=\"t\"\n\n[[args]]\nname=\"env\"\nrequired=true\n\n[[args]]\nname=\"note\"\nrest=true\n",
        );
        let bound = def.bind("prod ship it now").unwrap();
        assert_eq!(bound["env"], "prod");
        assert_eq!(bound["note"], "ship it now");
        assert_eq!(bound["args"], "prod ship it now");
    }

    #[test]
    fn bind_without_a_spec_only_exposes_the_raw_args() {
        let def = ok("x", "description=\"d\"\nkind=\"prompt\"\ntemplate=\"t\"\n");
        let bound = def.bind("  anything at all  ").unwrap();
        assert_eq!(bound.len(), 1);
        assert_eq!(bound["args"], "anything at all");
    }

    #[test]
    fn bind_reports_a_missing_required_argument() {
        let def = ok(
            "deploy",
            "description=\"d\"\nkind=\"prompt\"\ntemplate=\"t\"\n\n[[args]]\nname=\"env\"\nrequired=true\n",
        );
        let err = def.bind("").unwrap_err();
        assert_eq!(err.key, "env");
        assert!(err.msg.contains("missing required argument 'env'"), "{}", err.msg);
    }

    // ---- shell argv ----

    #[test]
    fn shell_argv_legacy_single_trailing_element_without_args_spec() {
        let def = ok(
            "deploy",
            "description=\"d\"\nkind=\"shell\"\nargv=[\"./deploy.sh\",\"--prod\"]\n",
        );
        assert_eq!(
            def.shell_argv("staging eu-west").unwrap(),
            vec!["./deploy.sh", "--prod", "staging eu-west"]
        );
        // Shell metacharacters survive verbatim inside the single element.
        assert_eq!(
            def.shell_argv("x; rm -rf / && echo $(pwd) | tee").unwrap(),
            vec!["./deploy.sh", "--prod", "x; rm -rf / && echo $(pwd) | tee"]
        );
        assert_eq!(def.shell_argv("").unwrap(), vec!["./deploy.sh", "--prod"]);
        assert_eq!(def.shell_argv("  \t ").unwrap(), vec!["./deploy.sh", "--prod"]);
    }

    #[test]
    fn shell_argv_with_args_spec_substitutes_then_appends_separately() {
        let def = ok(
            "deploy",
            r#"
description = "d"
kind = "shell"
argv = ["./deploy.sh", "--env", "{{env}}"]

[[args]]
name = "env"
required = true

[[args]]
name = "note"
rest = true
"#,
        );
        assert_eq!(
            def.shell_argv("prod hurry up").unwrap(),
            vec!["./deploy.sh", "--env", "prod", "hurry up"]
        );
        // An empty optional arg contributes nothing.
        assert_eq!(
            def.shell_argv("prod").unwrap(),
            vec!["./deploy.sh", "--env", "prod"]
        );
        // Substituted values are still never shell-interpolated.
        assert_eq!(
            def.shell_argv("prod ; rm -rf /").unwrap(),
            vec!["./deploy.sh", "--env", "prod", "; rm -rf /"]
        );
        assert!(def.shell_argv("").unwrap_err().msg.contains("missing required argument"));
    }

    // ---- embedded defaults ----

    #[test]
    fn embedded_table_parses_and_matches_the_js_payload() {
        let got: Vec<(String, String)> = builtin_commands()
            .values()
            .map(|d| (d.command.clone(), d.description.clone()))
            .collect();
        let expected = [
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
        assert_eq!(got.len(), expected.len());
        for (i, (n, d)) in expected.iter().enumerate() {
            assert_eq!((got[i].0.as_str(), got[i].1.as_str()), (*n, *d));
        }
    }

    #[test]
    fn embedded_aliases_match_the_js_coordinator() {
        let t = builtin_commands();
        assert_eq!(t["mac"].aliases, vec!["local"]);
        assert_eq!(t["gcp"].aliases, vec!["remote"]);
        assert_eq!(t["where"].aliases, vec!["status"]);
        assert_eq!(t["new"].aliases, vec!["reset"]);
        assert_eq!(t["help"].aliases, vec!["start"]);
        for name in ["claude", "codex", "stop", "menu"] {
            assert!(t[name].aliases.is_empty(), "{name} should have no alias");
        }
    }

    #[test]
    fn embedded_keyboard_entries_reproduce_the_js_rows() {
        let t = builtin_commands();
        let mut kb: Vec<(&str, i64)> = t
            .values()
            .filter(|d| d.keyboard)
            .map(|d| (d.command.as_str(), d.button_order.unwrap_or(0)))
            .collect();
        kb.sort_by_key(|(_, o)| *o);
        assert_eq!(
            kb.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec!["claude", "codex", "mac", "gcp", "new", "where"]
        );
    }

    #[test]
    fn effective_table_substitutes_user_commands_in_place() {
        let table = effective_table(&table_from(vec![
            ok("gcp", "description=\"My GCP\"\nkind=\"shell\"\nargv=[\"true\"]\n"),
            ok("deploy", "description=\"Deploy\"\nkind=\"shell\"\nargv=[\"true\"]\n"),
        ]));
        assert_eq!(
            table.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["claude", "codex", "mac", "gcp", "where", "new", "stop", "menu", "help", "deploy"]
        );
        assert_eq!(table["gcp"].description, "My GCP");
        assert_eq!(table["gcp"].kind, CommandKind::Shell);
    }

    #[test]
    fn effective_table_defaults_only_equals_the_shipped_table() {
        let empty: IndexMap<String, CommandDef> = IndexMap::new();
        assert_eq!(
            effective_table(&empty).keys().collect::<Vec<_>>(),
            builtin_commands().keys().collect::<Vec<_>>()
        );
    }

    // ---- sequence cycles ----

    #[test]
    fn sequence_cycles_detects_and_names_the_path() {
        let cycles = sequence_cycles(&table_from(vec![seq("a", &["b"]), seq("b", &["a"])]));
        assert_eq!(cycles, vec![vec!["a".to_string(), "b".into(), "a".into()]]);
    }

    #[test]
    fn sequence_cycles_clean_graph_is_empty() {
        assert!(sequence_cycles(&table_from(vec![
            seq("a", &["b", "c"]),
            seq("b", &["c"]),
            seq("c", &["d"]),
        ]))
        .is_empty());
        assert!(sequence_cycles(&builtin_commands()).is_empty());
    }

    #[test]
    fn sequence_cycles_reports_each_cycle_once() {
        let cycles = sequence_cycles(&table_from(vec![
            seq("a", &["b"]),
            seq("b", &["c"]),
            seq("c", &["a"]),
            seq("z", &["a"]),
        ]));
        assert_eq!(cycles.len(), 1, "got {cycles:?}");
        assert_eq!(cycles[0], vec!["a", "b", "c", "a"]);
    }

    #[test]
    fn sequence_cycles_ignores_non_sequence_kinds() {
        // A kind=shell command named in someone's steps is a leaf, not an edge.
        let table = table_from(vec![
            seq("a", &["b"]),
            ok("b", "description=\"d\"\nkind=\"shell\"\nargv=[\"true\"]\n"),
        ]);
        assert!(sequence_cycles(&table).is_empty());
    }

    #[test]
    fn kind_toml_spellings_roundtrip() {
        for kind in [
            CommandKind::Prompt,
            CommandKind::Agent,
            CommandKind::Engine,
            CommandKind::Target,
            CommandKind::Shell,
            CommandKind::Skill,
            CommandKind::Sequence,
            CommandKind::Builtin,
        ] {
            assert_eq!(CommandKind::from_toml_str(kind.as_toml_str()), Some(kind));
        }
        assert_eq!(CommandKind::from_toml_str("Prompt"), None);
        assert_eq!(CommandKind::from_toml_str(""), None);
        // USER_KINDS is exactly the set minus `builtin`.
        assert_eq!(USER_KINDS.len(), 7);
        assert!(!USER_KINDS.contains(&"builtin"));
    }
}
