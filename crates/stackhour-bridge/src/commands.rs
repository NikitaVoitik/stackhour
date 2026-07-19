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
// Labels
// ---------------------------------------------------------------------------

/// `engineLabel(name)`.
///
/// The JS is `name === 'codex' ? 'Codex' : 'Claude'` — every unknown engine,
/// including garbage, renders as "Claude". Here the label comes from the
/// engine registry, so a user-declared engine gets its own label; anything
/// the registry does not know still falls back to the shipped default engine's
/// label, which reproduces the JS collapse for `claude` / `codex` / junk.
pub fn engine_label(reg: &Registry, engine: &str) -> String {
    if let Some(def) = reg.engines.get(engine) {
        return def.label.clone();
    }
    reg.engines
        .get(crate::state::DEFAULT_ENGINE)
        .map(|d| d.label.clone())
        .unwrap_or_else(|| "Claude".to_string())
}

/// `label(name)` — `targets[name]?.label || name`.
pub fn target_label(labels: &IndexMap<String, String>, target: &str) -> String {
    labels
        .get(target)
        .filter(|l| !l.is_empty())
        .cloned()
        .unwrap_or_else(|| target.to_string())
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// Where `/ship` parks the user. `blort` / `claude` in the live config; both
/// are config keys here rather than the JS's two hardcoded literals.
#[derive(Debug, Clone)]
pub struct ShipCfg {
    pub target: String,
    pub engine: String,
}

impl Default for ShipCfg {
    fn default() -> Self {
        ShipCfg {
            target: "blort".to_string(),
            engine: crate::state::DEFAULT_ENGINE.to_string(),
        }
    }
}

/// Everything the command surface needs to render a reply, and nothing it can
/// use to perform one. Planning is pure: [`plan_text`] and [`plan_callback`]
/// return [`Action`]s for the coordinator to execute, which is what makes the
/// whole command surface testable without a Bot API at all.
pub struct CommandEnv<'a> {
    pub reg: &'a Registry,
    /// `targets[<name>].label` from config.json. NOT limited to gcp/mac: the
    /// live config also declares `blort`, and `/where` must be able to name it.
    pub target_labels: &'a IndexMap<String, String>,
    /// `workerAlive()` at render time.
    pub worker_alive: bool,
    /// `busyLocal` at render time.
    pub busy: bool,
    pub ship: ShipCfg,
}

impl CommandEnv<'_> {
    fn tpl(&self, name: &str, vars: &[(&str, &str)]) -> String {
        self.reg.prompts.render(name, vars)
    }

    fn engine_label(&self, engine: &str) -> String {
        engine_label(self.reg, engine)
    }

    fn target_label(&self, target: &str) -> String {
        target_label(self.target_labels, target)
    }

    /// The `{{engine}}` / `{{target}}` pair every `where`-shaped template takes.
    fn where_vars(&self, state: &BridgeState) -> (String, String) {
        (
            self.engine_label(&state.engine),
            self.target_label(&state.active),
        )
    }
}

// ---------------------------------------------------------------------------
// Rendered texts
// ---------------------------------------------------------------------------

/// How many leading characters of a session id `/where` shows.
const SESSION_PREVIEW_CHARS: usize = 8;

/// The `/where` (and `/status`) reply, and the body of the `where` callback's
/// toast.
pub fn status_text(env: &CommandEnv, state: &BridgeState) -> String {
    let (engine, target) = env.where_vars(state);
    let session = match state.session() {
        Some(id) => {
            let head: String = id.chars().take(SESSION_PREVIEW_CHARS).collect();
            format!("{head}…")
        }
        None => env.tpl("session-none", &[]),
    };
    env.tpl(
        "status",
        &[
            ("engine", &engine),
            ("target", &target),
            ("session", &session),
            ("worker", if env.worker_alive { "online" } else { "offline" }),
            ("busy", if env.busy { "yes" } else { "no" }),
        ],
    )
}

/// The `/menu` reply.
pub fn menu_text(env: &CommandEnv, state: &BridgeState) -> String {
    let (engine, target) = env.where_vars(state);
    env.tpl("menu", &[("engine", &engine), ("target", &target)])
}

/// The startup banner.
pub fn online_text(env: &CommandEnv, state: &BridgeState) -> String {
    let (engine, target) = env.where_vars(state);
    env.tpl(
        "online",
        &[
            ("engine", &engine),
            ("target", &target),
            ("worker", if env.worker_alive { "online" } else { "offline" }),
        ],
    )
}

/// `(resuming session)` / `(new session)`, read AFTER the switch — which is
/// why both switch functions mutate first and render second.
fn session_note(env: &CommandEnv, state: &BridgeState) -> String {
    let name = if state.session().is_some() {
        "session-resuming"
    } else {
        "session-new"
    };
    env.tpl(name, &[])
}

/// `switchEngine(name)`: mutate, then describe. The caller persists.
pub fn switch_engine(env: &CommandEnv, state: &mut BridgeState, engine: &str) -> String {
    state.engine = engine.to_string();
    let (engine, target) = env.where_vars(state);
    let session = session_note(env, state);
    env.tpl(
        "switch-engine",
        &[("engine", &engine), ("target", &target), ("session", &session)],
    )
}

/// `switchTarget(name)`: mutate, then describe. The caller persists.
pub fn switch_target(env: &CommandEnv, state: &mut BridgeState, target: &str) -> String {
    state.active = target.to_string();
    let (engine, target) = env.where_vars(state);
    let session = session_note(env, state);
    env.tpl(
        "switch-target",
        &[("engine", &engine), ("target", &target), ("session", &session)],
    )
}

/// The `/new` reply.
pub fn session_reset_text(env: &CommandEnv, state: &BridgeState) -> String {
    let (engine, target) = env.where_vars(state);
    env.tpl("session-reset", &[("engine", &engine), ("target", &target)])
}

/// What `/stop` actually managed to stop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StopOutcome {
    /// A local child was signalled.
    pub stopped_local: bool,
    /// Queued mac jobs whose job file was removed before the worker claimed it.
    pub cancelled: usize,
    /// Mac jobs already claimed — unreachable from here.
    pub running: usize,
}

/// The `/stop` reply, assembled from what was stopped.
///
/// Line order is the JS's: local kill, then cancellations, then the
/// already-running warning; an empty set becomes "Nothing running.".
pub fn stop_text(env: &CommandEnv, outcome: StopOutcome) -> String {
    let mut lines: Vec<String> = Vec::new();
    if outcome.stopped_local {
        lines.push(env.tpl("stop-local", &[]));
    }
    if outcome.cancelled > 0 {
        lines.push(env.tpl("stop-cancelled", &[("count", &outcome.cancelled.to_string())]));
    }
    if outcome.running > 0 {
        lines.push(env.tpl("stop-claimed", &[("count", &outcome.running.to_string())]));
    }
    if lines.is_empty() {
        return env.tpl("stop-idle", &[]);
    }
    lines.join("\n")
}

/// The `Unknown command.` reply.
pub fn unknown_text(env: &CommandEnv) -> String {
    env.tpl("unknown-command", &[("help", &help_text(env.reg))])
}

/// The toast for a tapped button.
///
/// Table-driven: a command's `toast` key wins, because the JS strings are
/// deliberately NOT the labels ("Using Claude Code", not "Using Claude"), so
/// they cannot be derived from `engineLabel()`/`label()`. A command with no
/// `toast` falls back to the `toast-*` templates, which ARE label-derived.
pub fn toast_for(env: &CommandEnv, state: &BridgeState, def: &CommandDef) -> Option<String> {
    if let Some(toast) = &def.toast {
        return Some(toast.clone());
    }
    match def.kind {
        CommandKind::Engine => {
            let engine = def.engine.as_deref().unwrap_or(&state.engine);
            Some(env.tpl("toast-engine", &[("engine", &env.engine_label(engine))]))
        }
        CommandKind::Target => {
            let target = def.target.as_deref().unwrap_or(&state.active);
            Some(env.tpl("toast-target", &[("target", &env.target_label(target))]))
        }
        CommandKind::Builtin => match def.builtin.as_deref() {
            Some("new") => Some(env.tpl("toast-new", &[])),
            Some("stop") => Some(env.tpl("toast-stop", &[])),
            _ => None,
        },
        _ => None,
    }
}

/// The four emoji whose presence in a message's text means it is carrying a
/// control keyboard worth repainting (`/🖥️|☁️|🧠|🛠/`).
///
/// Coupled to the button captions by design in the JS: changing a caption
/// silently breaks the refresh. Here the set is derived from the table's own
/// buttons, so it cannot drift.
pub fn should_refresh_keyboard(table: &IndexMap<String, CommandDef>, text: &str) -> bool {
    table
        .values()
        .filter(|d| {
            // Only the SELECTION buttons — the ones carrying a '✅ ' marker
            // that a repaint would move. The JS regex is exactly the engine
            // and target emoji for the same reason: '🆕 New session' is never
            // marked, so a message merely containing 🆕 is not a control
            // surface and must be left alone.
            d.keyboard
                && !d.hidden
                && matches!(d.kind, CommandKind::Engine | CommandKind::Target)
        })
        .flat_map(|d| d.button_text().chars())
        .filter(|c| !c.is_ascii() && !is_marker(*c))
        .any(|c| text.contains(c))
}

/// Characters that appear in a caption but must not, on their own, mark a
/// message as keyboard-bearing: the active marker and the variation selector.
fn is_marker(c: char) -> bool {
    matches!(c, '✅' | '\u{fe0f}' | ' ')
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// One effect the coordinator should perform. Deliberately transport-free:
/// the planner never touches Telegram, the queue, or the disk.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Send a chat message. `html` selects `parse_mode: 'HTML'`; `keyboard`
    /// is the `reply_markup`, absent where the JS sends none (`/new`, `/stop`).
    Send {
        text: String,
        html: bool,
        keyboard: Option<Value>,
    },
    /// Persist state.json. Emitted after every mutation, matching the JS
    /// where `setSession`/`switchTarget`/`switchEngine` all save inline.
    SaveState,
    /// Route text to the active lane (mac dispatch or the local queue).
    RoutePrompt {
        text: String,
        message_id: Option<i64>,
    },
    /// Kill the local child and cancel queued mac jobs, then reply with
    /// [`stop_text`] of the outcome.
    Stop,
    /// A declarative command (shell/prompt/skill/sequence) for the runner.
    RunCommand { command: String, raw: String },
    /// Show a `confirm = true` command's Yes/Cancel keyboard.
    Confirm { command: String, raw: String },
    /// Answer a callback query (a `None` text is the bare spinner dismissal).
    AnswerCallback { id: String, text: Option<String> },
    /// Repaint a tapped message's control keyboard, resending its text
    /// unchanged with `parse_mode` omitted.
    RefreshKeyboard { message_id: i64, text: String },
}

/// Plan one inbound text update. Mutates `state`; emits [`Action::SaveState`]
/// whenever it did.
pub fn plan_text(
    env: &CommandEnv,
    state: &mut BridgeState,
    text: &str,
    message_id: Option<i64>,
) -> Vec<Action> {
    let table = table(env.reg);
    match resolve(&table, text) {
        Dispatch::Prompt(text) => vec![Action::RoutePrompt { text, message_id }],
        Dispatch::Unknown => vec![Action::Send {
            text: unknown_text(env),
            html: true,
            keyboard: Some(control_keyboard_for(state, &table)),
        }],
        Dispatch::Confirm { command, raw } => vec![Action::Confirm { command, raw }],
        Dispatch::Command { command, raw } => {
            run_command(env, state, &table, &command, &raw, message_id)
        }
    }
}

/// Plan one callback query.
pub fn plan_callback(env: &CommandEnv, state: &mut BridgeState, cb: &Value) -> Vec<Action> {
    let table = table(env.reg);
    let id = cb["id"].as_str().unwrap_or_default().to_string();
    let data = cb["data"].as_str().unwrap_or_default();

    // An unknown or stale payload still dismisses the client-side spinner,
    // but changes nothing and repaints nothing.
    let Some(dispatch) = resolve_callback(&table, data) else {
        return vec![Action::AnswerCallback { id, text: None }];
    };
    let (command, raw) = match dispatch {
        Dispatch::Command { command, raw } | Dispatch::Confirm { command, raw } => (command, raw),
        _ => return vec![Action::AnswerCallback { id, text: None }],
    };

    // `where` is the one button that answers with a body instead of acting.
    if builtin_verb(&table, &command) == Some("where") {
        return vec![Action::AnswerCallback {
            id,
            text: Some(status_text(env, state)),
        }];
    }

    // No message_id -> no reaction, exactly like `handleText('/stop')` from
    // the stop button.
    let mut out = run_command(env, state, &table, &command, &raw, None);
    let toast = table.get(&command).and_then(|d| toast_for(env, state, d));
    out.push(Action::AnswerCallback { id, text: toast });

    // Repaint the tapped message's markers — but only if it is a message that
    // carries a control keyboard.
    if let (Some(mid), Some(text)) = (
        cb["message"]["message_id"].as_i64(),
        cb["message"]["text"].as_str(),
    ) {
        if should_refresh_keyboard(&table, text) {
            out.push(Action::RefreshKeyboard {
                message_id: mid,
                text: text.to_string(),
            });
        }
    }
    out
}

/// The body shared by typed commands and tapped buttons.
fn run_command(
    env: &CommandEnv,
    state: &mut BridgeState,
    table: &IndexMap<String, CommandDef>,
    command: &str,
    raw: &str,
    message_id: Option<i64>,
) -> Vec<Action> {
    let Some(def) = table.get(command) else {
        return vec![Action::Send {
            text: unknown_text(env),
            html: true,
            keyboard: Some(control_keyboard_for(state, table)),
        }];
    };

    // A switch reply's keyboard must show the NEW selection, so it is built
    // after the mutation.
    let with_kb = |state: &BridgeState, text: String| Action::Send {
        text,
        html: false,
        keyboard: Some(control_keyboard_for(state, table)),
    };

    match def.kind {
        CommandKind::Engine => {
            let engine = def.engine.clone().unwrap_or_else(|| state.engine.clone());
            let text = switch_engine(env, state, &engine);
            vec![Action::SaveState, with_kb(state, text)]
        }
        CommandKind::Target => {
            let target = def.target.clone().unwrap_or_else(|| state.active.clone());
            let text = switch_target(env, state, &target);
            vec![Action::SaveState, with_kb(state, text)]
        }
        CommandKind::Builtin => match def.builtin.as_deref().unwrap_or("") {
            "help" => vec![Action::Send {
                text: help_text(env.reg),
                html: true,
                keyboard: Some(control_keyboard_for(state, table)),
            }],
            "menu" => {
                let text = menu_text(env, state);
                vec![with_kb(state, text)]
            }
            "where" => {
                let text = status_text(env, state);
                vec![with_kb(state, text)]
            }
            "new" => {
                state.set_session(None);
                // The only state command that sends NO keyboard.
                vec![
                    Action::SaveState,
                    Action::Send {
                        text: session_reset_text(env, state),
                        html: false,
                        keyboard: None,
                    },
                ]
            }
            "stop" => vec![Action::Stop],
            "ship" => plan_ship(env, state, table, raw, message_id),
            _ => vec![Action::Send {
                text: unknown_text(env),
                html: true,
                keyboard: Some(control_keyboard_for(state, table)),
            }],
        },
        // Agent switches and every declarative kind belong to their own
        // runners; the command surface only decides that they were asked for.
        _ => vec![Action::RunCommand {
            command: def.command.clone(),
            raw: raw.to_string(),
        }],
    }
}

/// `/ship`.
///
/// The JS mutates state BEFORE it checks for a task, so a bare `/ship`
/// permanently parks the user on the ship target — there is no `/unship`, only
/// `/gcp` or `/mac`. Reproduced deliberately: the owner's muscle memory is
/// "`/ship`, then send the ticket as a second message", which only works
/// because the switch already happened.
fn plan_ship(
    env: &CommandEnv,
    state: &mut BridgeState,
    table: &IndexMap<String, CommandDef>,
    raw: &str,
    message_id: Option<i64>,
) -> Vec<Action> {
    state.active = env.ship.target.clone();
    state.engine = env.ship.engine.clone();

    let task = raw.trim();
    if task.is_empty() {
        let (engine, target) = env.where_vars(state);
        return vec![
            Action::SaveState,
            Action::Send {
                text: env.tpl("ship-empty", &[("engine", &engine), ("target", &target)]),
                html: false,
                keyboard: Some(control_keyboard_for(state, table)),
            },
        ];
    }
    vec![
        Action::SaveState,
        Action::RoutePrompt {
            // The command word is re-prepended: the engine sees `/ship <task>`.
            text: env.tpl("ship-prompt", &[("task", task)]),
            message_id,
        },
    ]
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
            { "command": "ship",   "description": "Ship a Blort task 🚀" },
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
            "🚀 /ship — ship a Blort task (Notion→PR)",
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
        assert_eq!(list.len(), 11);
        assert_eq!(
            list[10],
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
            10
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
        assert_eq!(list.len(), 10);
        assert_eq!(
            list[5],
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
                "claude", "codex", "mac", "gcp", "ship", "where", "new", "stop", "menu", "help",
                "deploy", "review", "status",
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

    // ---- labels, texts and state transitions ----

    fn labels() -> IndexMap<String, String> {
        // The live config's three targets, labels and all.
        [("gcp", "☁️ GCP"), ("mac", "🖥️ Mac"), ("blort", "🚀 Blort")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn env<'a>(reg: &'a Registry, labels: &'a IndexMap<String, String>) -> CommandEnv<'a> {
        CommandEnv {
            reg,
            target_labels: labels,
            worker_alive: true,
            busy: false,
            ship: ShipCfg::default(),
        }
    }

    fn fresh_state() -> BridgeState {
        BridgeState::default()
    }

    #[test]
    fn engine_and_target_labels_match_the_js_collapse() {
        let reg = empty_registry();
        let l = labels();
        assert_eq!(engine_label(&reg, "codex"), "Codex");
        assert_eq!(engine_label(&reg, "claude"), "Claude");
        // engineLabel(): anything that is not 'codex' renders as 'Claude'.
        assert_eq!(engine_label(&reg, "ollama"), "Claude");
        assert_eq!(engine_label(&reg, ""), "Claude");
        // label(): the config label, falling back to the bare target name.
        assert_eq!(target_label(&l, "mac"), "🖥️ Mac");
        assert_eq!(target_label(&l, "blort"), "🚀 Blort");
        assert_eq!(target_label(&l, "unconfigured"), "unconfigured");
    }

    #[test]
    fn status_text_matches_the_js_status_text() {
        let reg = empty_registry();
        let l = labels();
        let mut e = env(&reg, &l);
        let mut state = fresh_state();

        assert_eq!(
            status_text(&e, &state),
            "Engine: Claude\nTarget: ☁️ GCP\nSession: none (fresh)\nMac worker: online\nGCP busy: no"
        );

        // A session shows its first EIGHT characters plus U+2026.
        state.set_session(Some("0123456789abcdef".into()));
        e.worker_alive = false;
        e.busy = true;
        assert_eq!(
            status_text(&e, &state),
            "Engine: Claude\nTarget: ☁️ GCP\nSession: 01234567…\nMac worker: offline\nGCP busy: yes"
        );
    }

    /// A short session id must not be padded or panic on the 8-char cut.
    #[test]
    fn a_short_session_id_is_shown_whole() {
        let reg = empty_registry();
        let l = labels();
        let mut state = fresh_state();
        state.set_session(Some("abc".into()));
        assert!(status_text(&env(&reg, &l), &state).contains("Session: abc…"));
    }

    /// The parenthetical is read AFTER the switch, so switching to a target
    /// that already has a session says "resuming" even though the session
    /// belongs to the target we just arrived at.
    #[test]
    fn switching_reports_the_session_of_the_destination() {
        let reg = empty_registry();
        let l = labels();
        let e = env(&reg, &l);
        let mut state = fresh_state();
        state.set_session_for("mac", "claude", None, Some("mac-session".into()));

        assert_eq!(
            switch_target(&e, &mut state, "mac"),
            "Switched to 🖥️ Mac with Claude. (resuming session)"
        );
        assert_eq!(state.active, "mac");
        // Same target, other engine: no session there yet.
        assert_eq!(
            switch_engine(&e, &mut state, "codex"),
            "Switched to Codex on 🖥️ Mac. (new session)"
        );
        assert_eq!(state.engine, "codex");
    }

    #[test]
    fn menu_online_and_reset_texts_match_the_js() {
        let reg = empty_registry();
        let l = labels();
        let mut e = env(&reg, &l);
        let state = fresh_state();
        assert_eq!(menu_text(&e, &state), "🎛 Controls — Claude on ☁️ GCP");
        assert_eq!(
            online_text(&e, &state),
            "🤖 Claude + Codex bridge online. Active: Claude on ☁️ GCP. Mac worker: online."
        );
        e.worker_alive = false;
        assert!(online_text(&e, &state).ends_with("Mac worker: offline."));
        assert_eq!(
            session_reset_text(&e, &state),
            "🆕 Fresh Claude session on ☁️ GCP."
        );
    }

    #[test]
    fn stop_text_assembles_the_js_line_order() {
        let reg = empty_registry();
        let l = labels();
        let e = env(&reg, &l);
        assert_eq!(stop_text(&e, StopOutcome::default()), "Nothing running.");
        assert_eq!(
            stop_text(
                &e,
                StopOutcome {
                    stopped_local: true,
                    cancelled: 2,
                    running: 1,
                }
            ),
            "🛑 Stopped GCP job.\n🛑 Cancelled 2 queued Mac job(s).\n\
             ⚠️ 1 Mac job(s) already running — can't interrupt remotely yet."
        );
        // Zero counts contribute no line at all.
        assert_eq!(
            stop_text(
                &e,
                StopOutcome {
                    stopped_local: false,
                    cancelled: 1,
                    running: 0,
                }
            ),
            "🛑 Cancelled 1 queued Mac job(s)."
        );
    }

    // ---- planning ----

    fn sends(actions: &[Action]) -> Vec<&str> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_text_goes_straight_to_the_prompt_lane() {
        let reg = empty_registry();
        let l = labels();
        let mut state = fresh_state();
        assert_eq!(
            plan_text(&env(&reg, &l), &mut state, "refactor the parser", Some(7)),
            vec![Action::RoutePrompt {
                text: "refactor the parser".into(),
                message_id: Some(7)
            }]
        );
    }

    /// Every state-mutating command must persist, and it must persist BEFORE
    /// the reply — a crash between the two costs a message, not the session.
    #[test]
    fn switch_commands_save_and_reply_with_the_repainted_keyboard() {
        let reg = empty_registry();
        let l = labels();
        let mut state = fresh_state();
        let actions = plan_text(&env(&reg, &l), &mut state, "/codex", None);

        assert_eq!(actions[0], Action::SaveState);
        assert_eq!(state.engine, "codex");
        let Action::Send { text, html, keyboard } = &actions[1] else {
            panic!("expected a send, got {actions:?}");
        };
        assert_eq!(text, "Switched to Codex on ☁️ GCP. (new session)");
        assert!(!html, "switch replies carry no parse_mode");
        // The keyboard shows the NEW engine as active.
        let kb = keyboard.as_ref().expect("keyboard");
        assert_eq!(kb["inline_keyboard"][0][1]["text"], "✅ 🛠 Codex");
        assert_eq!(kb["inline_keyboard"][0][0]["text"], "🧠 Claude");
    }

    #[test]
    fn new_clears_the_session_saves_and_sends_no_keyboard() {
        let reg = empty_registry();
        let l = labels();
        let mut state = fresh_state();
        state.set_session(Some("live".into()));

        let actions = plan_text(&env(&reg, &l), &mut state, "/reset", None);
        assert_eq!(actions[0], Action::SaveState);
        assert_eq!(state.session(), None);
        // An explicit null, not a removed key.
        assert_eq!(state.sessions.get("gcp:claude"), Some(&None));
        assert_eq!(
            actions[1],
            Action::Send {
                text: "🆕 Fresh Claude session on ☁️ GCP.".into(),
                html: false,
                keyboard: None,
            },
            "/new is the one state command with no keyboard"
        );
    }

    #[test]
    fn help_and_unknown_are_html_with_a_keyboard() {
        let reg = empty_registry();
        let l = labels();
        let e = env(&reg, &l);
        let mut state = fresh_state();

        let help = plan_text(&e, &mut state, "/start", None);
        assert!(matches!(help[0], Action::Send { html: true, keyboard: Some(_), .. }));
        assert_eq!(sends(&help), vec![help_text(&reg)]);

        let unknown = plan_text(&e, &mut state, "/nope", None);
        assert!(matches!(unknown[0], Action::Send { html: true, keyboard: Some(_), .. }));
        assert_eq!(sends(&unknown)[0], format!("Unknown command.\n\n{}", help_text(&reg)));
        // Neither touched state.
        assert!(!unknown.contains(&Action::SaveState));
    }

    #[test]
    fn stop_defers_to_the_runtime_outcome() {
        let reg = empty_registry();
        let l = labels();
        let mut state = fresh_state();
        assert_eq!(plan_text(&env(&reg, &l), &mut state, "/stop", None), vec![Action::Stop]);
    }

    // ---- /ship ----

    #[test]
    fn bare_ship_parks_on_the_ship_target_and_prompts_for_a_task() {
        let reg = empty_registry();
        let l = labels();
        let mut state = fresh_state();
        let actions = plan_text(&env(&reg, &l), &mut state, "/ship", None);

        assert_eq!(state.active, "blort");
        assert_eq!(state.engine, "claude");
        assert_eq!(actions[0], Action::SaveState);
        assert_eq!(
            sends(&actions),
            vec!["🚀 Ship mode: Claude on the Blort repo. Send the task (text, ECM-xxxx, or a Slack link)."]
        );
        // Parked on blort, NEITHER Mac nor GCP is checked.
        let Action::Send { keyboard: Some(kb), .. } = &actions[1] else {
            panic!("expected a keyboard");
        };
        assert_eq!(kb["inline_keyboard"][1][0]["text"], "🖥️ Mac");
        assert_eq!(kb["inline_keyboard"][1][1]["text"], "☁️ GCP");
    }

    /// The command word is matched case-insensitively but the TASK keeps its
    /// case — lowercasing it would corrupt ticket ids and Slack links.
    #[test]
    fn ship_with_a_task_reroutes_it_with_its_case_intact() {
        let reg = empty_registry();
        let l = labels();
        let mut state = fresh_state();
        let actions = plan_text(&env(&reg, &l), &mut state, "/SHIP ECM-1234 fix the CSV export", Some(42));

        assert_eq!(state.active, "blort");
        assert_eq!(actions[0], Action::SaveState);
        assert_eq!(
            actions[1],
            Action::RoutePrompt {
                text: "/ship ECM-1234 fix the CSV export".into(),
                message_id: Some(42),
            }
        );
    }

    /// '/ship   ' trims to nothing and takes the empty branch, exactly like a
    /// bare '/ship'.
    #[test]
    fn ship_with_only_whitespace_is_a_bare_ship() {
        let reg = empty_registry();
        let l = labels();
        let mut state = fresh_state();
        let actions = plan_text(&env(&reg, &l), &mut state, "/ship   ", None);
        assert!(matches!(actions[1], Action::Send { .. }));
    }

    /// The ship destination is config, not two literals buried in a branch.
    #[test]
    fn the_ship_target_and_engine_come_from_config() {
        let reg = empty_registry();
        let l = labels();
        let e = CommandEnv {
            ship: ShipCfg {
                target: "mac".into(),
                engine: "codex".into(),
            },
            ..env(&reg, &l)
        };
        let mut state = fresh_state();
        plan_text(&e, &mut state, "/ship", None);
        assert_eq!((state.active.as_str(), state.engine.as_str()), ("mac", "codex"));
    }

    // ---- callbacks ----

    fn callback(data: &str, text: Option<&str>) -> Value {
        let mut cb = json!({ "id": "cb-1", "data": data });
        if let Some(text) = text {
            cb["message"] = json!({ "message_id": 99, "text": text });
        }
        cb
    }

    #[test]
    fn callback_toasts_match_the_js_hardcoded_strings() {
        let reg = empty_registry();
        let l = labels();
        let e = env(&reg, &l);
        for (data, expected) in [
            ("e:claude", "Using Claude Code"),
            ("e:codex", "Using Codex"),
            ("t:mac", "On the Mac 🖥️"),
            ("t:gcp", "On the GCP box ☁️"),
            ("new", "Fresh session"),
            ("stop", "Stopping…"),
        ] {
            let mut state = fresh_state();
            let actions = plan_callback(&e, &mut state, &callback(data, None));
            let toast = actions
                .iter()
                .find_map(|a| match a {
                    Action::AnswerCallback { text, .. } => text.clone(),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no toast for {data}"));
            assert_eq!(toast, expected, "callback {data}");
        }
    }

    /// The `where` button answers with the whole status text as the toast body
    /// and changes nothing.
    #[test]
    fn the_where_callback_only_answers() {
        let reg = empty_registry();
        let l = labels();
        let e = env(&reg, &l);
        let mut state = fresh_state();
        let actions = plan_callback(&e, &mut state, &callback("where", Some("🧠 anything")));
        assert_eq!(
            actions,
            vec![Action::AnswerCallback {
                id: "cb-1".into(),
                text: Some(status_text(&e, &state)),
            }]
        );
    }

    /// An unknown payload still dismisses the spinner, but repaints nothing.
    #[test]
    fn an_unknown_callback_dismisses_the_spinner_and_returns() {
        let reg = empty_registry();
        let l = labels();
        let mut state = fresh_state();
        assert_eq!(
            plan_callback(&env(&reg, &l), &mut state, &callback("stale:payload", Some("🧠 x"))),
            vec![Action::AnswerCallback {
                id: "cb-1".into(),
                text: None
            }]
        );
    }

    /// The repaint is gated on the tapped message's TEXT carrying one of the
    /// button emoji — a plain reply is left alone.
    #[test]
    fn the_keyboard_is_repainted_only_for_keyboard_bearing_messages() {
        let reg = empty_registry();
        let l = labels();
        let e = env(&reg, &l);
        let table = table(&reg);

        assert!(should_refresh_keyboard(&table, "🎛 Controls — Claude on ☁️ GCP"));
        assert!(should_refresh_keyboard(&table, "Switched to 🖥️ Mac with Claude."));
        assert!(should_refresh_keyboard(&table, "🧠 Claude"));
        assert!(!should_refresh_keyboard(&table, "🆕 Fresh Claude session on gcp."));
        assert!(!should_refresh_keyboard(&table, "Nothing running."));

        let mut state = fresh_state();
        let actions = plan_callback(&e, &mut state, &callback("e:codex", Some("🧠 Claude on ☁️ GCP")));
        assert_eq!(
            actions.last(),
            Some(&Action::RefreshKeyboard {
                message_id: 99,
                // The text is resent UNCHANGED; only reply_markup differs.
                text: "🧠 Claude on ☁️ GCP".into(),
            })
        );

        let mut state = fresh_state();
        let plain = plan_callback(&e, &mut state, &callback("new", Some("Nothing running.")));
        assert!(
            !plain.iter().any(|a| matches!(a, Action::RefreshKeyboard { .. })),
            "a plain message must not be repainted: {plain:?}"
        );
    }

    /// A tapped button acts exactly like the typed command, including the save.
    #[test]
    fn a_tapped_button_runs_the_same_command_body() {
        let reg = empty_registry();
        let l = labels();
        let e = env(&reg, &l);
        let mut state = fresh_state();
        let actions = plan_callback(&e, &mut state, &callback("t:mac", None));

        assert_eq!(state.active, "mac");
        assert!(actions.contains(&Action::SaveState));
        assert_eq!(sends(&actions), vec!["Switched to 🖥️ Mac with Claude. (new session)"]);
    }

    /// The stop button carries no message id, so nothing reacts to it.
    #[test]
    fn the_stop_button_requests_a_stop_and_toasts() {
        let reg = empty_registry();
        let l = labels();
        let mut state = fresh_state();
        let actions = plan_callback(&env(&reg, &l), &mut state, &callback("stop", None));
        assert_eq!(
            actions,
            vec![
                Action::Stop,
                Action::AnswerCallback {
                    id: "cb-1".into(),
                    text: Some("Stopping…".into())
                }
            ]
        );
    }

    /// A user command with its own toast overrides the derived one, and a
    /// user engine command with none falls back to the label template.
    #[test]
    fn toasts_are_table_driven_with_a_template_fallback() {
        let mut reg = empty_registry();
        reg.commands.insert(
            "aider".into(),
            parse(
                "aider",
                "description=\"Aider\"\nkind=\"engine\"\nengine=\"claude\"\nkeyboard=true\n",
            ),
        );
        let l = labels();
        let e = env(&reg, &l);
        let t = table(&reg);
        let state = fresh_state();

        assert_eq!(
            toast_for(&e, &state, t.get("claude").unwrap()).as_deref(),
            Some("Using Claude Code"),
            "the table's toast must win"
        );
        assert_eq!(
            toast_for(&e, &state, t.get("aider").unwrap()).as_deref(),
            Some("Using Claude"),
            "no toast key -> the label-derived template"
        );
    }

    #[test]
    fn description_is_truncated_to_telegrams_limit() {
        let long = "é".repeat(300);
        let def = parse(
            "x",
            &format!("description=\"{long}\"\nkind=\"shell\"\nargv=[\"true\"]\n"),
        );
        let payload = my_commands_payload_for(&table_with(vec![def]));
        let d = payload["commands"][10]["description"].as_str().unwrap();
        assert_eq!(d.chars().count(), MAX_DESCRIPTION_LEN);
    }
}
