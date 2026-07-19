//! Telegram command handling — registration, help and dispatch, all
//! GENERATED from the command table.
//!
//! There is exactly one command table: the commands shipped with the bridge
//! (`registry::command::builtin_commands`, parsed from embedded TOML) with any
//! user `commands/<name>.toml` of the same name substituted IN PLACE, followed
//! by the remaining user commands in load order. Everything else is derived
//! from it:
//!
//! * [`my_commands_payload`] — the `setMyCommands` body. Aliases are never
//!   registered separately, matching the JS bridge where `/local`, `/remote`,
//!   `/status`, `/reset` and `/start` worked but were unlisted.
//! * [`help_text`] — the `/help` body, rendered through the `help` prompt
//!   template's `{{commands}}` placeholder. There is no second copy of the
//!   help text anywhere in this crate.
//! * [`control_keyboard`] — see [`crate::keyboard`].
//!
//! Dispatch order in [`resolve`]: exact command name, then aliases, then
//! unknown-slash -> help, then plain text -> the prompt lane. `confirm = true`
//! commands stop at a Yes/Cancel keyboard whose callback carries the typed
//! arguments.
//!
//! With no config directory at all, every one of those outputs is
//! byte-identical to the JS coordinator's — see the tests at the bottom.

use indexmap::IndexMap;
use serde_json::{json, Value};
use stackhour_core::registry::command::{self, CommandDef, CommandKind};
use stackhour_core::registry::Registry;

use crate::coordinator::Coordinator;
use crate::state::BridgeState;

pub use crate::keyboard::{
    callback_data, confirm_keyboard, control_keyboard, control_keyboard_for, stop_keyboard,
};

/// Telegram's `setMyCommands` description limit, in characters.
const MAX_DESCRIPTION_LEN: usize = 256;

/// The effective command table for a loaded registry.
pub fn table(reg: &Registry) -> IndexMap<String, CommandDef> {
    command::effective_table(&reg.commands)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// The `setMyCommands` request body:
/// `{ "commands": [ { "command": …, "description": … } ] }`.
///
/// Shipped commands first, in their embedded order (a user override keeps the
/// slot it replaced), then the remaining user commands. `hidden = true`
/// commands are omitted; aliases are never registered as separate entries.
pub fn my_commands_payload(reg: &Registry) -> Value {
    my_commands_payload_for(&table(reg))
}

/// [`my_commands_payload`] over an already-built table.
pub fn my_commands_payload_for(table: &IndexMap<String, CommandDef>) -> Value {
    let commands: Vec<Value> = table
        .values()
        .filter(|d| !d.hidden)
        .map(|d| {
            json!({
                "command": d.command,
                "description": truncate_chars(&d.description, MAX_DESCRIPTION_LEN),
            })
        })
        .collect();
    json!({ "commands": commands })
}

/// Truncate on a char boundary (Telegram counts characters, not bytes).
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((i, _)) => s[..i].to_string(),
    }
}

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

/// Minimal HTML escaping, matching the JS coordinator's `esc()`.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// One `/help` line per visible command: `/usage — description`.
fn help_lines<'a>(defs: impl Iterator<Item = &'a CommandDef>) -> String {
    defs.filter(|d| !d.hidden)
        .map(|d| format!("{} — {}", esc(&d.usage()), esc(&d.description)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `/help` body.
///
/// Rendered through the `help` prompt template with a `{{commands}}`
/// placeholder holding the generated command list. If the active template has
/// no `{{commands}}` placeholder — which is the case for the built-in
/// template, whose body is still the JS bridge's frozen HTML blob — the
/// rendered text is returned unchanged, and only the commands that template
/// cannot possibly describe (the user's own, beyond the shipped table) are
/// appended.
///
/// That is what makes the backward-compatibility gate hold: with no config at
/// all, `help_text` returns the legacy string byte-for-byte, while a user who
/// drops in `prompts/help.md` with `{{commands}}` gets a fully generated body.
pub fn help_text(reg: &Registry) -> String {
    let table = table(reg);
    let generated = help_lines(table.values());
    let rendered = reg.prompts.render("help", &[("commands", &generated)]);

    if generated.is_empty() || rendered.contains(&generated) {
        return rendered;
    }
    let shipped = command::builtin_commands();
    let extra = help_lines(table.values().filter(|d| !shipped.contains_key(&d.command)));
    if extra.is_empty() {
        rendered
    } else {
        format!("{rendered}\n\n{extra}")
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// What one inbound text update resolves to. Pure: no IO and no state
/// mutation, so the dispatch order is directly testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// A command matched (by name or alias). `raw` is everything typed after
    /// the command word, trimmed.
    Command { command: String, raw: String },
    /// The command matched but declares `confirm = true`: show the Yes/Cancel
    /// keyboard instead of running it.
    Confirm { command: String, raw: String },
    /// Text starting with `/` that matched nothing -> "Unknown command." + help.
    Unknown,
    /// Anything else -> the prompt lane, verbatim.
    Prompt(String),
}

/// Resolve one inbound text update against a command table.
///
/// Order: exact command name, then aliases, then unknown-slash, then plain
/// text. The command word is lowercased and a `@botname` suffix stripped,
/// matching how Telegram delivers commands in groups.
pub fn resolve(table: &IndexMap<String, CommandDef>, text: &str) -> Dispatch {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('/') {
        return Dispatch::Prompt(text.to_string());
    }

    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let (word, rest) = trimmed.split_at(end);
    let name = word[1..].split('@').next().unwrap_or("").to_lowercase();
    let raw = rest.trim().to_string();

    match command::lookup(table, &name) {
        None => Dispatch::Unknown,
        Some(def) if def.confirm => Dispatch::Confirm {
            command: def.command.clone(),
            raw,
        },
        Some(def) => Dispatch::Command {
            command: def.command.clone(),
            raw,
        },
    }
}

/// The bridge verb a resolved command runs, when it is a shipped built-in
/// (`kind = "builtin"`): `help`, `menu`, `where`, `new`, `stop`.
pub fn builtin_verb<'a>(table: &'a IndexMap<String, CommandDef>, command: &str) -> Option<&'a str> {
    let def = table.get(command)?;
    if def.kind == CommandKind::Builtin {
        def.builtin.as_deref()
    } else {
        None
    }
}

/// Resolve a `callback_data` string back to a dispatch.
///
/// Understands the legacy opaque payloads (`e:<engine>`, `t:<target>`, and the
/// bare verbs `new` / `where` / `stop`) as well as the generated `c:<command>`
/// and confirmation `y:<command> <args>` / `n:<command>` forms, so a keyboard
/// sent by the JS bridge before an upgrade keeps working. `None` = nothing to
/// do (an unknown payload, or an explicit cancel).
pub fn resolve_callback(table: &IndexMap<String, CommandDef>, data: &str) -> Option<Dispatch> {
    if let Some(rest) = data.strip_prefix("y:") {
        let end = rest.find(' ').unwrap_or(rest.len());
        let (name, raw) = rest.split_at(end);
        let def = command::lookup(table, name)?;
        return Some(Dispatch::Command {
            command: def.command.clone(),
            raw: raw.trim().to_string(),
        });
    }
    if data.starts_with("n:") {
        return None; // cancelled
    }

    let name: String = if let Some(c) = data.strip_prefix("c:") {
        c.to_string()
    } else if let Some(e) = data.strip_prefix("e:") {
        find_by(table, |d| {
            d.kind == CommandKind::Engine && d.engine.as_deref() == Some(e)
        })?
    } else if let Some(t) = data.strip_prefix("t:") {
        find_by(table, |d| {
            d.kind == CommandKind::Target && d.target.as_deref() == Some(t)
        })?
    } else {
        find_by(table, |d| {
            d.kind == CommandKind::Builtin && d.builtin.as_deref() == Some(data)
        })?
    };

    let def = command::lookup(table, &name)?;
    Some(Dispatch::Command {
        command: def.command.clone(),
        raw: String::new(),
    })
}

fn find_by(
    table: &IndexMap<String, CommandDef>,
    pred: impl Fn(&CommandDef) -> bool,
) -> Option<String> {
    table.values().find(|d| pred(d)).map(|d| d.command.clone())
}

// ---------------------------------------------------------------------------
// Runtime (awaiting the coordinator/telegram/state pillars)
// ---------------------------------------------------------------------------

/// Handle one text update.
///
/// The decision is [`resolve`]; the effects (state writes, `Tg::send`,
/// spawning the shell and prompt lanes) need `telegram.rs`, `state.rs` and
/// `coordinator.rs`, which are still `todo!()` scaffolds owned by other
/// pillars. Wiring lands with them; the dispatch logic above is complete and
/// tested.
pub fn handle_text(ctx: &mut Coordinator, text: &str, msg_id: Option<i64>) {
    let _ = (ctx, text, msg_id);
    todo!("wire resolve() into the coordinator lanes once state.rs/telegram.rs land")
}

/// Handle one callback query (inline keyboards). The decision is
/// [`resolve_callback`]; see [`handle_text`] for why the effects are pending.
pub fn handle_callback(ctx: &mut Coordinator, cb: &Value) {
    let _ = (ctx, cb);
    todo!("wire resolve_callback() into the coordinator lanes")
}

/// The `/status` reply text (owned by the coordinator pillar).
pub fn status_text(state: &BridgeState, worker_alive: bool, busy: bool) -> String {
    let _ = (state, worker_alive, busy);
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stackhour_core::registry;

    fn empty_registry() -> Registry {
        // No config dir at all: shipped defaults only.
        registry::load(std::path::Path::new(
            "/nonexistent/stackhour-commands-pillar-test",
        ))
    }

    fn parse(name: &str, doc: &str) -> CommandDef {
        let v: toml::Value = doc.parse().expect("test TOML");
        CommandDef::from_toml(name, &v).expect("valid command")
    }

    fn table_with(defs: Vec<CommandDef>) -> IndexMap<String, CommandDef> {
        let user: IndexMap<String, CommandDef> =
            defs.into_iter().map(|d| (d.command.clone(), d)).collect();
        command::effective_table(&user)
    }

    // ---- backward-compatibility gate: defaults only ----

    #[test]
    fn defaults_only_my_commands_payload_matches_the_js_bridge() {
        // registerCommands() in coordinator.mjs, verbatim.
        let expected = json!({ "commands": [
            { "command": "claude", "description": "Use Claude Code 🧠" },
            { "command": "codex",  "description": "Use Codex 🛠" },
            { "command": "mac",    "description": "Run on the Mac 🖥️" },
            { "command": "gcp",    "description": "Run on the GCP box ☁️" },
            { "command": "where",  "description": "Show active target & session" },
            { "command": "new",    "description": "Fresh session on active target" },
            { "command": "stop",   "description": "Kill/cancel the running job" },
            { "command": "menu",   "description": "Show tap-button controls" },
            { "command": "help",   "description": "Show command list" },
        ] });
        assert_eq!(my_commands_payload(&empty_registry()), expected);
    }

    #[test]
    fn defaults_only_help_text_matches_the_js_help_constant() {
        // The HELP constant from coordinator.mjs, line for line.
        let expected = [
            "<b>Claude + Codex bridge</b> (distributed)",
            "",
            "🧠 /claude — use Claude Code",
            "🛠 /codex — use Codex",
            "🖥️ /mac — run on the Mac",
            "☁️ /gcp — run on the GCP box",
            "ℹ️ /where — active engine, target &amp; session",
            "🆕 /new — fresh session for this engine + target",
            "⏹ /stop — kill/cancel the running job",
            "🎛 /menu — tap-button controls",
            "",
            "<i>Anything else → selected engine on the active target.</i>",
        ]
        .join("\n");
        assert_eq!(help_text(&empty_registry()), expected);
    }

    // ---- a valid custom command takes effect ----

    #[test]
    fn a_user_command_is_registered_and_helped() {
        let mut reg = empty_registry();
        reg.commands.insert(
            "deploy".into(),
            parse(
                "deploy",
                r#"description = "Deploy to production"
kind = "shell"
argv = ["./deploy.sh"]

[[args]]
name = "env"
required = true
"#,
            ),
        );

        let payload = my_commands_payload(&reg);
        let list = payload["commands"].as_array().unwrap();
        assert_eq!(list.len(), 10);
        assert_eq!(
            list[9],
            json!({ "command": "deploy", "description": "Deploy to production" })
        );

        // Appended to the frozen help blob, with the generated usage line.
        let help = help_text(&reg);
        assert!(help.starts_with("<b>Claude + Codex bridge</b> (distributed)"));
        assert!(
            help.ends_with("\n\n/deploy &lt;env&gt; — Deploy to production"),
            "got: {help}"
        );
    }

    #[test]
    fn a_hidden_command_is_registered_but_not_helped() {
        let mut reg = empty_registry();
        reg.commands.insert(
            "secret".into(),
            parse(
                "secret",
                "description=\"Secret\"\nkind=\"shell\"\nargv=[\"true\"]\nhidden=true\n",
            ),
        );
        assert_eq!(help_text(&reg), help_text(&empty_registry()));
        assert_eq!(
            my_commands_payload(&reg)["commands"].as_array().unwrap().len(),
            9
        );
    }

    #[test]
    fn a_user_override_keeps_the_builtin_slot_everywhere() {
        let mut reg = empty_registry();
        reg.commands.insert(
            "where".into(),
            parse(
                "where",
                "description=\"Where am I\"\nkind=\"shell\"\nargv=[\"hostname\"]\n",
            ),
        );
        let payload = my_commands_payload(&reg);
        let list = payload["commands"].as_array().unwrap();
        assert_eq!(list.len(), 9);
        assert_eq!(
            list[4],
            json!({ "command": "where", "description": "Where am I" })
        );
    }

    #[test]
    fn a_help_template_with_the_placeholder_generates_the_whole_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("prompts")).unwrap();
        std::fs::write(
            dir.path().join("prompts/help.md"),
            "<b>Commands</b>\n{{commands}}\n",
        )
        .unwrap();
        let reg = registry::load(dir.path());
        let help = help_text(&reg);
        assert!(
            help.starts_with("<b>Commands</b>\n/claude — Use Claude Code 🧠\n"),
            "got: {help}"
        );
        assert!(help.contains("/where — Show active target &amp; session"));
        assert!(help.ends_with("/help — Show command list\n"));
    }

    // ---- an invalid command produces the right error ----

    #[test]
    fn an_invalid_command_file_is_skipped_with_a_file_and_key_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("commands")).unwrap();
        std::fs::write(
            dir.path().join("commands/deploy.toml"),
            "description = \"Deploy\"\nkind = \"target\"\ntarget = \"moon\"\n",
        )
        .unwrap();
        let reg = registry::load(dir.path());

        assert!(
            !reg.commands.contains_key("deploy"),
            "the broken command must be skipped"
        );
        let err = reg
            .errors
            .iter()
            .find(|e| e.name == "deploy")
            .expect("one error for the broken command");
        assert_eq!(
            err.file.as_deref(),
            Some(dir.path().join("commands/deploy.toml").as_path())
        );
        assert_eq!(
            err.message,
            "key `target`: unknown target 'moon' (known: gcp, mac)"
        );
        // ...and the bridge still shows exactly the shipped table.
        assert_eq!(
            my_commands_payload(&reg),
            my_commands_payload(&empty_registry())
        );
    }

    #[test]
    fn the_shipped_starter_tree_generates_a_working_table() {
        // `stackhour bridge init --config-dir` must produce a config a user
        // can actually run: every example command survives cross-referencing
        // and shows up in registration, help and dispatch.
        let dir = tempfile::tempdir().unwrap();
        registry::defaults::materialize(dir.path()).expect("materialize starter tree");
        let reg = registry::load(dir.path());

        let t = table(&reg);
        for name in ["deploy", "status", "review", "ship"] {
            assert!(t.contains_key(name), "/{name} missing: {:?}", reg.errors);
        }
        // Aliases from the examples dispatch to their owners.
        assert_eq!(
            resolve(&t, "/ship"),
            Dispatch::Confirm {
                command: "ship".into(),
                raw: String::new()
            }
        );
        assert_eq!(
            resolve(&t, "/rv src/api"),
            Dispatch::Command {
                command: "review".into(),
                raw: "src/api".into()
            }
        );
        // /status is a user command name, so it wins over /where's alias.
        assert_eq!(
            resolve(&t, "/status"),
            Dispatch::Command {
                command: "status".into(),
                raw: String::new()
            }
        );

        let payload = my_commands_payload(&reg);
        let registered: Vec<&str> = payload["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["command"].as_str().unwrap())
            .collect();
        assert_eq!(
            registered,
            vec![
                "claude", "codex", "mac", "gcp", "where", "new", "stop", "menu", "help", "deploy",
                "review", "ship", "status",
            ]
        );

        let help = help_text(&reg);
        assert!(help.contains("/deploy &lt;env&gt; [note...] — Run the deploy checklist"));
        assert!(help.contains("/ship — Review, then deploy"));
    }

    // ---- dispatch order ----

    #[test]
    fn resolve_matches_names_then_aliases_then_falls_through() {
        let t = command::builtin_commands();
        let cmd = |c: &str| Dispatch::Command {
            command: c.into(),
            raw: String::new(),
        };
        assert_eq!(resolve(&t, "/gcp"), cmd("gcp"));
        assert_eq!(resolve(&t, "/remote"), cmd("gcp"));
        assert_eq!(resolve(&t, "/local"), cmd("mac"));
        assert_eq!(resolve(&t, "/status"), cmd("where"));
        assert_eq!(resolve(&t, "/reset"), cmd("new"));
        assert_eq!(resolve(&t, "/start"), cmd("help"));
        assert_eq!(resolve(&t, "/nope"), Dispatch::Unknown);
        assert_eq!(
            resolve(&t, "hello there"),
            Dispatch::Prompt("hello there".into())
        );
        // A slash mid-text is still just a prompt.
        assert_eq!(
            resolve(&t, "what about /gcp?"),
            Dispatch::Prompt("what about /gcp?".into())
        );
    }

    #[test]
    fn resolve_lowercases_strips_the_bot_suffix_and_keeps_args_verbatim() {
        let t = table_with(vec![parse(
            "deploy",
            "description=\"d\"\nkind=\"shell\"\nargv=[\"true\"]\n",
        )]);
        assert_eq!(
            resolve(&t, "/DEPLOY@stackhour_bot   prod  eu-west  "),
            Dispatch::Command {
                command: "deploy".into(),
                raw: "prod  eu-west".into()
            }
        );
    }

    #[test]
    fn resolve_routes_confirm_commands_to_the_confirmation() {
        let t = table_with(vec![parse(
            "deploy",
            "description=\"d\"\nkind=\"shell\"\nargv=[\"true\"]\nconfirm=true\n",
        )]);
        assert_eq!(
            resolve(&t, "/deploy prod"),
            Dispatch::Confirm {
                command: "deploy".into(),
                raw: "prod".into()
            }
        );
        // ...and the Yes button round-trips back to the real run.
        let data = confirm_keyboard("deploy", "prod")["inline_keyboard"][0][0]["callback_data"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            resolve_callback(&t, &data),
            Some(Dispatch::Command {
                command: "deploy".into(),
                raw: "prod".into()
            })
        );
        assert_eq!(resolve_callback(&t, "n:deploy"), None);
    }

    #[test]
    fn resolve_callback_understands_the_legacy_payloads() {
        let t = command::builtin_commands();
        for (data, expected) in [
            ("e:claude", "claude"),
            ("e:codex", "codex"),
            ("t:mac", "mac"),
            ("t:gcp", "gcp"),
            ("new", "new"),
            ("where", "where"),
            ("stop", "stop"),
        ] {
            assert_eq!(
                resolve_callback(&t, data),
                Some(Dispatch::Command {
                    command: expected.into(),
                    raw: String::new()
                }),
                "callback {data}"
            );
        }
        assert_eq!(resolve_callback(&t, "bogus"), None);
        assert_eq!(resolve_callback(&t, "c:nope"), None);
    }

    #[test]
    fn generated_callback_data_round_trips_through_resolve_callback() {
        let t = table_with(vec![parse(
            "deploy",
            "description=\"d\"\nkind=\"shell\"\nargv=[\"true\"]\nkeyboard=true\n",
        )]);
        for def in t.values() {
            let data = callback_data(def);
            assert_eq!(
                resolve_callback(&t, &data),
                Some(Dispatch::Command {
                    command: def.command.clone(),
                    raw: String::new()
                }),
                "callback_data {data} for /{}",
                def.command
            );
        }
    }

    #[test]
    fn builtin_verb_only_answers_for_shipped_verbs() {
        let t = command::builtin_commands();
        assert_eq!(builtin_verb(&t, "where"), Some("where"));
        assert_eq!(builtin_verb(&t, "stop"), Some("stop"));
        assert_eq!(builtin_verb(&t, "claude"), None); // kind = engine
        assert_eq!(builtin_verb(&t, "nope"), None);
        // A user shadowing a verb takes over its behaviour, so it stops
        // being a builtin.
        let shadowed = table_with(vec![parse(
            "where",
            "description=\"d\"\nkind=\"shell\"\nargv=[\"hostname\"]\n",
        )]);
        assert_eq!(builtin_verb(&shadowed, "where"), None);
    }

    #[test]
    fn description_is_truncated_to_telegrams_limit() {
        let long = "é".repeat(300);
        let def = parse(
            "x",
            &format!("description=\"{long}\"\nkind=\"shell\"\nargv=[\"true\"]\n"),
        );
        let payload = my_commands_payload_for(&table_with(vec![def]));
        let d = payload["commands"][9]["description"].as_str().unwrap();
        assert_eq!(d.chars().count(), MAX_DESCRIPTION_LEN);
    }
}
