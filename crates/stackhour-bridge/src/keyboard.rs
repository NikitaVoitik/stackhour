//! Inline keyboards, GENERATED from the command table.
//!
//! There is no second copy of the button list anywhere: every button comes
//! from a [`CommandDef`] with `keyboard = true`, sorted by `button_order`
//! (falling back to table position), laid out two per row. With no user
//! config this reproduces the JS coordinator's `controlKb()` byte-for-byte:
//!
//! ```text
//! [ 🧠 Claude ][ 🛠 Codex ]
//! [ 🖥️ Mac    ][ ☁️ GCP   ]
//! [ 🆕 New session ][ ℹ️ Status ]
//! ```
//!
//! The `'✅ '` active-marker prefix is applied to the entry matching the
//! current state: `kind = "engine"` matches `state.engine`, `kind = "target"`
//! matches `state.active`, `kind = "agent"` matches `state.agent`. Every other
//! kind is never marked.
//!
//! `callback_data` is generated too, and deliberately reproduces the legacy
//! opaque strings so an in-flight keyboard from the JS bridge keeps working:
//! `e:<engine>`, `t:<target>`, and the bare bridge verbs `new` / `where` /
//! `stop`. Everything else is namespaced `c:<command>` (and confirmations are
//! `y:<command> <args>` / `n:<command>`), which cannot collide with the legacy
//! set.

use indexmap::IndexMap;
use serde_json::{json, Value};
use stackhour_core::registry::command::{self, CommandDef, CommandKind};

use crate::state::BridgeState;

/// Buttons per keyboard row (the JS coordinator's layout).
const ROW_WIDTH: usize = 2;

/// The callback_data for one command's button.
pub fn callback_data(def: &CommandDef) -> String {
    match def.kind {
        CommandKind::Engine => match &def.engine {
            Some(e) => format!("e:{e}"),
            None => format!("c:{}", def.command),
        },
        CommandKind::Target => match &def.target {
            Some(t) => format!("t:{t}"),
            None => format!("c:{}", def.command),
        },
        CommandKind::Builtin => def
            .builtin
            .clone()
            .unwrap_or_else(|| format!("c:{}", def.command)),
        _ => format!("c:{}", def.command),
    }
}

/// Whether this command's button should carry the `'✅ '` active marker.
fn is_active(def: &CommandDef, state: &BridgeState) -> bool {
    match def.kind {
        CommandKind::Engine => def.engine.as_deref() == Some(state.engine.as_str()),
        CommandKind::Target => def.target.as_deref() == Some(state.active.as_str()),
        CommandKind::Agent => match (&def.agent, &state.agent) {
            (Some(a), Some(active)) => a == active,
            _ => false,
        },
        _ => false,
    }
}

/// The control inline keyboard for a command table.
///
/// `table` is the EFFECTIVE table (shipped commands with user overrides
/// substituted in place) — see `command::effective_table`.
pub fn control_keyboard_for(state: &BridgeState, table: &IndexMap<String, CommandDef>) -> Value {
    let mut entries: Vec<(i64, usize, &CommandDef)> = table
        .values()
        .enumerate()
        .filter(|(_, d)| d.keyboard && !d.hidden)
        .map(|(i, d)| (d.button_order.unwrap_or(i as i64), i, d))
        .collect();
    // Stable: explicit order first, table position as the tiebreak.
    entries.sort_by_key(|(order, index, _)| (*order, *index));

    let rows: Vec<Value> = entries
        .chunks(ROW_WIDTH)
        .map(|chunk| {
            chunk
                .iter()
                .map(|(_, _, def)| {
                    let prefix = if is_active(def, state) { "✅ " } else { "" };
                    json!({
                        "text": format!("{prefix}{}", def.button_text()),
                        "callback_data": callback_data(def),
                    })
                })
                .collect::<Vec<Value>>()
        })
        .map(Value::from)
        .collect();

    json!({ "inline_keyboard": rows })
}

/// The control inline keyboard (contract entry point).
pub fn control_keyboard(state: &BridgeState, reg: &stackhour_core::registry::Registry) -> Value {
    control_keyboard_for(state, &command::effective_table(&reg.commands))
}

/// The single-button stop keyboard shown under a running job's status
/// message. Not table-driven: it is the job lifecycle's own affordance, not a
/// command entry, and the JS bridge treats it the same way.
pub fn stop_keyboard() -> Value {
    json!({ "inline_keyboard": [[{ "text": "⏹ Stop", "callback_data": "stop" }]] })
}

/// The Yes/Cancel keyboard for a `confirm = true` command. `raw` is the
/// user's argument string, carried through the callback so the confirmed run
/// gets the same arguments the user typed.
///
/// Telegram caps `callback_data` at 64 bytes; the argument tail is truncated
/// on a char boundary to fit, so a very long argument string degrades to a
/// shorter one rather than failing the whole send.
pub fn confirm_keyboard(command: &str, raw: &str) -> Value {
    let mut data = format!("y:{command}");
    let raw = raw.trim();
    if !raw.is_empty() {
        let budget = 64usize.saturating_sub(data.len() + 1);
        let mut end = raw.len().min(budget);
        while end > 0 && !raw.is_char_boundary(end) {
            end -= 1;
        }
        if end > 0 {
            data.push(' ');
            data.push_str(&raw[..end]);
        }
    }
    json!({ "inline_keyboard": [[
        { "text": "✅ Yes", "callback_data": data },
        { "text": "✖️ Cancel", "callback_data": format!("n:{command}") },
    ]] })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn state(engine: &str, active: &str) -> BridgeState {
        BridgeState {
            offset: 0,
            active: active.to_string(),
            engine: engine.to_string(),
            agent: None,
            sessions: IndexMap::new(),
            raw: Value::Null,
        }
    }

    fn parse(name: &str, doc: &str) -> CommandDef {
        let v: toml::Value = doc.parse().expect("test TOML");
        CommandDef::from_toml(name, &v).expect("valid command")
    }

    /// The JS coordinator's controlKb() for the given state, verbatim.
    fn legacy_control_kb(engine: &str, active: &str) -> Value {
        json!({ "inline_keyboard": [
            [
                { "text": format!("{}🧠 Claude", if engine == "claude" { "✅ " } else { "" }), "callback_data": "e:claude" },
                { "text": format!("{}🛠 Codex", if engine == "codex" { "✅ " } else { "" }), "callback_data": "e:codex" },
            ],
            [
                { "text": format!("{}🖥️ Mac", if active == "mac" { "✅ " } else { "" }), "callback_data": "t:mac" },
                { "text": format!("{}☁️ GCP", if active == "gcp" { "✅ " } else { "" }), "callback_data": "t:gcp" },
            ],
            [
                { "text": "🆕 New session", "callback_data": "new" },
                { "text": "ℹ️ Status", "callback_data": "where" },
            ],
        ] })
    }

    // ---- backward-compatibility gate ----

    #[test]
    fn defaults_only_keyboard_is_byte_identical_to_the_js_bridge() {
        let table = command::builtin_commands();
        for (engine, active) in [
            ("claude", "gcp"),
            ("claude", "mac"),
            ("codex", "gcp"),
            ("codex", "mac"),
        ] {
            assert_eq!(
                control_keyboard_for(&state(engine, active), &table),
                legacy_control_kb(engine, active),
                "{engine} on {active}"
            );
        }
    }

    #[test]
    fn an_unknown_engine_marks_nothing() {
        let table = command::builtin_commands();
        let kb = control_keyboard_for(&state("ollama", "gcp"), &table);
        let row = &kb["inline_keyboard"][0];
        assert_eq!(row[0]["text"], "🧠 Claude");
        assert_eq!(row[1]["text"], "🛠 Codex");
    }

    // ---- user commands ----

    #[test]
    fn a_user_command_adds_a_button_at_its_declared_position() {
        let mut user: IndexMap<String, CommandDef> = IndexMap::new();
        let def = parse(
            "deploy",
            r#"description = "Deploy"
kind = "shell"
argv = ["./deploy.sh"]
keyboard = true
button = "🚀 Deploy"
button_order = 40
"#,
        );
        user.insert("deploy".into(), def);
        let kb = control_keyboard_for(&state("claude", "gcp"), &command::effective_table(&user));
        // Fourth row, first (and only) button.
        assert_eq!(kb["inline_keyboard"].as_array().unwrap().len(), 4);
        assert_eq!(kb["inline_keyboard"][3][0]["text"], "🚀 Deploy");
        assert_eq!(kb["inline_keyboard"][3][0]["callback_data"], "c:deploy");
    }

    #[test]
    fn hidden_commands_never_get_a_button() {
        let mut user: IndexMap<String, CommandDef> = IndexMap::new();
        user.insert(
            "secret".into(),
            parse(
                "secret",
                "description = \"s\"\nkind = \"shell\"\nargv = [\"true\"]\nkeyboard = true\nhidden = true\n",
            ),
        );
        assert_eq!(
            control_keyboard_for(&state("claude", "gcp"), &command::effective_table(&user)),
            legacy_control_kb("claude", "gcp")
        );
    }

    #[test]
    fn an_agent_command_is_marked_from_the_active_agent() {
        let mut user: IndexMap<String, CommandDef> = IndexMap::new();
        user.insert(
            "rev".into(),
            parse(
                "rev",
                "description = \"Reviewer\"\nkind = \"agent\"\nagent = \"reviewer\"\nkeyboard = true\nbutton = \"Reviewer\"\nbutton_order = 99\n",
            ),
        );
        let table = command::effective_table(&user);
        let mut st = state("claude", "gcp");
        assert_eq!(
            control_keyboard_for(&st, &table)["inline_keyboard"][3][0]["text"],
            "Reviewer"
        );
        st.agent = Some("reviewer".into());
        assert_eq!(
            control_keyboard_for(&st, &table)["inline_keyboard"][3][0]["text"],
            "✅ Reviewer"
        );
    }

    #[test]
    fn a_user_override_takes_the_builtin_button_slot() {
        let mut user: IndexMap<String, CommandDef> = IndexMap::new();
        user.insert(
            "codex".into(),
            parse(
                "codex",
                "description = \"My Codex\"\nkind = \"engine\"\nengine = \"codex\"\nkeyboard = true\nbutton = \"⚡ Codex\"\nbutton_order = 11\n",
            ),
        );
        let kb = control_keyboard_for(&state("codex", "gcp"), &command::effective_table(&user));
        assert_eq!(kb["inline_keyboard"][0][1]["text"], "✅ ⚡ Codex");
        assert_eq!(kb["inline_keyboard"][0][1]["callback_data"], "e:codex");
    }

    // ---- other keyboards ----

    #[test]
    fn stop_keyboard_matches_the_js_stop_kb() {
        assert_eq!(
            stop_keyboard(),
            json!({ "inline_keyboard": [[{ "text": "⏹ Stop", "callback_data": "stop" }]] })
        );
    }

    #[test]
    fn confirm_keyboard_carries_the_arguments() {
        assert_eq!(
            confirm_keyboard("deploy", "  prod now  "),
            json!({ "inline_keyboard": [[
                { "text": "✅ Yes", "callback_data": "y:deploy prod now" },
                { "text": "✖️ Cancel", "callback_data": "n:deploy" },
            ]] })
        );
        assert_eq!(
            confirm_keyboard("deploy", "")["inline_keyboard"][0][0]["callback_data"],
            "y:deploy"
        );
    }

    #[test]
    fn confirm_keyboard_truncates_to_telegrams_64_byte_limit() {
        let kb = confirm_keyboard("deploy", &"é".repeat(100));
        let data = kb["inline_keyboard"][0][0]["callback_data"].as_str().unwrap();
        assert!(data.len() <= 64, "len {}", data.len());
        assert!(data.starts_with("y:deploy é"));
        // Never split a char: the payload must still be valid UTF-8 text.
        assert!(data.chars().all(|c| c == 'é' || c.is_ascii()));
    }
}
