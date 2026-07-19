//! Extensibility gate: a brand-new Telegram command, added by CONFIG ONLY.
//!
//! Nothing in this file touches Rust source. It writes a config directory that
//! a user could write by hand — one `commands/status.toml` plus a
//! `prompts/help.md` — loads it through the same entry point the daemon uses,
//! and asserts the command shows up in every generated surface:
//!
//!   1. the `setMyCommands` payload sent to Telegram,
//!   2. the GENERATED `/help` body,
//!   3. the `/menu` inline keyboard (text + callback_data),
//!   4. dispatch: `/status`, `/status <args>`, its alias, and its button's
//!      callback_data all resolve to the same command,
//!   5. the resolved definition carries the fixed argv that the shell lane
//!      exec's, so the action itself is reachable from the table.
//!
//! If this file ever needs a source change to pass, the config layer has
//! stopped being extensible and that is the bug.

use std::fs;

use stackhour_bridge::commands::{self, Dispatch};
use stackhour_bridge::keyboard;
use stackhour_bridge::state::BridgeState;
use stackhour_core::registry::command::CommandKind;
use stackhour_core::registry::{self, Registry};

/// The action the new command runs. A fixed argv — never shell-interpolated.
const STATUS_ARGV: [&str; 3] = ["sh", "-c", "uptime; df -h /"];

fn write_config(files: &[(&str, &str)]) -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (rel, body) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        fs::write(&path, body).expect("write");
    }
    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));
    (dir, reg)
}

/// The whole of the user's contribution: one command file and a help template
/// that opts into the generated command list.
fn status_config() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "commands/status.toml",
            r#"
description = "Show host load and disk"
aliases = ["health"]
kind = "shell"
argv = ["sh", "-c", "uptime; df -h /"]
keyboard = true
button = "📊 Status"

[[args]]
name = "note"
rest = true
description = "anything extra to log"
"#,
        ),
        ("prompts/help.md", "<b>Commands</b>\n\n{{commands}}\n"),
    ]
}

fn loaded() -> (tempfile::TempDir, Registry) {
    let (dir, reg) = write_config(&status_config());
    assert!(
        reg.commands.contains_key("status"),
        "commands/status.toml did not load; registry errors: {:?}",
        reg.errors
    );
    (dir, reg)
}

#[test]
fn config_only_command_registers_with_telegram() {
    let (_dir, reg) = loaded();
    let payload = commands::my_commands_payload(&reg);
    let entries = payload["commands"].as_array().expect("commands array");

    let found = entries
        .iter()
        .find(|e| e["command"] == "status")
        .unwrap_or_else(|| panic!("/status missing from setMyCommands: {payload}"));
    assert_eq!(found["description"], "Show host load and disk");

    // Aliases are never registered as separate Telegram entries.
    assert!(
        !entries.iter().any(|e| e["command"] == "health"),
        "alias leaked into setMyCommands: {payload}"
    );
}

#[test]
fn config_only_command_appears_in_generated_help() {
    let (_dir, reg) = loaded();
    let help = commands::help_text(&reg);
    assert!(
        help.contains("/status [note...] — Show host load and disk"),
        "generated help is missing the new command:\n{help}"
    );
    // The generated list is the whole body, not an appendix: the shipped
    // commands render through the same template.
    assert!(help.starts_with("<b>Commands</b>"), "help:\n{help}");
    assert!(help.contains("/help"), "help:\n{help}");
}

#[test]
fn config_only_command_appears_in_the_keyboard() {
    let (_dir, reg) = loaded();
    let state = BridgeState {
        offset: 0,
        active: "gcp".into(),
        engine: "claude".into(),
        agent: None,
        sessions: indexmap::IndexMap::new(),
        raw: serde_json::Value::Null,
    };
    let kb = keyboard::control_keyboard(&state, &reg);

    let button = kb["inline_keyboard"]
        .as_array()
        .expect("rows")
        .iter()
        .flat_map(|row| row.as_array().expect("row").iter())
        .find(|b| b["callback_data"] == "c:status")
        .unwrap_or_else(|| panic!("/status button missing from keyboard: {kb}"));
    assert_eq!(button["text"], "📊 Status");
}

#[test]
fn config_only_command_dispatches() {
    let (_dir, reg) = loaded();
    let table = commands::table(&reg);

    // Bare, with arguments, case-insensitively, with a @botname suffix, and
    // via the declared alias — all the same command.
    for text in ["/status", "/STATUS", "/status@mybot", "/health"] {
        assert_eq!(
            commands::resolve(&table, text),
            Dispatch::Command {
                command: "status".into(),
                raw: String::new(),
            },
            "dispatch failed for {text:?}"
        );
    }
    assert_eq!(
        commands::resolve(&table, "/status after the deploy"),
        Dispatch::Command {
            command: "status".into(),
            raw: "after the deploy".into(),
        }
    );

    // The keyboard button routes to the same place.
    assert_eq!(
        commands::resolve_callback(&table, "c:status"),
        Some(Dispatch::Command {
            command: "status".into(),
            raw: String::new(),
        })
    );

    // And the resolved definition carries the action the shell lane exec's.
    let def = table.get("status").expect("in table");
    assert_eq!(def.kind, CommandKind::Shell);
    assert_eq!(def.argv.as_deref(), Some(&STATUS_ARGV.map(String::from)[..]));
}
