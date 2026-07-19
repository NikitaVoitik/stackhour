//! Two guarantees that must hold for the config layer to be safe to ship:
//!
//!   1. BACKWARD COMPATIBILITY. A user who upgrades and changes nothing — a
//!      config.json with only the legacy Node-era keys and NONE of the new
//!      sections, no `engines/`, `agents/`, `skills/`, `commands/`, `prompts/`
//!      directories — must get byte-identical behaviour to an empty config
//!      dir: the same command table, the same setMyCommands payload, the same
//!      generated /help, the same keyboard, the same dispatch, the same
//!      scalar defaults, and zero registry errors. The new layer must be
//!      invisible until someone opts in.
//!
//!   2. ERROR QUALITY. A malformed command / skill / agent / engine file must
//!      produce an ACTIONABLE error that names the offending FILE and the
//!      offending KEY. It must not panic, must not take the daemon down, must
//!      not be silently skipped, and must not poison its neighbours: every
//!      other valid entry in the same directory still loads.
//!
//! Both are config-only: nothing here needs a Rust change to pass, and if a
//! Rust change is ever needed to keep it passing, that change is the bug.

use indexmap::IndexMap;
use serde_json::json;
use std::fs;

use stackhour_bridge::commands::{self, Dispatch};
use stackhour_bridge::keyboard;
use stackhour_bridge::state::BridgeState;
use stackhour_core::registry::{self, Registry, RegistryEntityKind};

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

fn state() -> BridgeState {
    BridgeState {
        offset: 0,
        active: "gcp".into(),
        engine: "claude".into(),
        agent: None,
        sessions: IndexMap::new(),
        raw: json!({}),
    }
}

/// Every observable bridge surface derived from a registry, flattened into
/// one comparable value. If two registries agree here they are behaviourally
/// interchangeable as far as Telegram is concerned.
fn surfaces(reg: &Registry) -> serde_json::Value {
    let table = commands::table(reg);
    let dispatch = |text: &str| format!("{:?}", commands::resolve(&table, text));
    json!({
        "commands": table.keys().collect::<Vec<_>>(),
        "engines": reg.engines.keys().collect::<Vec<_>>(),
        "agents": reg.agents.keys().collect::<Vec<_>>(),
        "skills": reg.skills.keys().collect::<Vec<_>>(),
        "my_commands": commands::my_commands_payload(reg),
        "help": commands::help_text(reg),
        "keyboard": keyboard::control_keyboard(&state(), reg),
        "defaults": {
            "agent": reg.defaults.agent.clone(),
            "engine": reg.defaults.engine.clone(),
            "target": reg.defaults.target.clone(),
        },
        "dispatch": {
            "help": dispatch("/help"),
            "status": dispatch("/status"),
            "unknown": dispatch("/definitely-not-a-command"),
            "prompt": dispatch("just some prose"),
        },
    })
}

// ---------------------------------------------------------------------------
// 1. Backward compatibility
// ---------------------------------------------------------------------------

/// A real pre-rewrite settings file: tokens, chat ids, worker wiring. Not one
/// key the registry knows about, and specifically no `bridge` object.
const LEGACY_CONFIG_JSON: &str = r#"{
  "token": "123456:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
  "chatId": 4242,
  "allowedUsers": [4242],
  "workerUrl": "http://127.0.0.1:8787",
  "workerToken": "worker-secret",
  "repoPath": "/home/nikita/stackhour",
  "pollTimeoutSeconds": 30,
  "notify": { "onFinish": true, "onError": true }
}"#;

#[test]
fn a_legacy_config_json_with_none_of_the_new_sections_behaves_exactly_as_before() {
    // Baseline: a completely empty config dir — the shipped built-ins alone.
    let (_empty_dir, baseline) = write_config(&[]);
    // Upgraded user: same dir plus their untouched legacy config.json.
    let (_legacy_dir, legacy) = write_config(&[("config.json", LEGACY_CONFIG_JSON)]);

    assert!(
        legacy.errors.is_empty(),
        "a legacy config.json must not produce registry errors, got: {:?}",
        legacy.errors.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
    assert!(
        baseline.errors.is_empty(),
        "baseline errors: {:?}",
        baseline.errors
    );

    // Guard against a vacuous comparison: the baseline must actually have
    // content, or "identical" would be meaningless.
    assert!(
        !baseline.commands.is_empty() && !baseline.engines.is_empty(),
        "baseline registry is empty — the surface comparison below proves nothing"
    );
    // The shipped built-ins are exactly the Node bridge's command set, and
    // /help still renders the Node help body verbatim.
    assert_eq!(
        baseline.commands.keys().collect::<Vec<_>>(),
        ["claude", "codex", "mac", "gcp", "ship", "where", "new", "stop", "menu", "help"]
            .iter()
            .collect::<Vec<_>>()
    );
    assert_eq!(
        baseline.engines.keys().collect::<Vec<_>>(),
        ["claude", "codex"].iter().collect::<Vec<_>>()
    );
    let help = commands::help_text(&baseline);
    assert!(
        help.starts_with("<b>Claude + Codex bridge</b>") && help.contains("/menu"),
        "built-in help drifted from the Node body:\n{help}"
    );

    assert_eq!(
        surfaces(&legacy),
        surfaces(&baseline),
        "a config.json with none of the new sections changed an observable surface"
    );

    // And the baseline itself is the documented legacy behaviour, not just
    // "whatever the code does today": no agent, claude, gcp.
    assert_eq!(legacy.defaults.agent, None);
    assert_eq!(legacy.defaults.engine, "claude");
    assert_eq!(legacy.defaults.target, "gcp");
}

#[test]
fn empty_new_section_directories_are_also_a_no_op() {
    // Someone runs `mkdir -p ~/.stackhour/{engines,agents,skills,commands,prompts}`
    // and stops there. Still nothing changes.
    let (_baseline_dir, baseline) = write_config(&[]);
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("config.json"), LEGACY_CONFIG_JSON).expect("write");
    for sub in ["engines", "agents", "skills", "commands", "prompts"] {
        fs::create_dir_all(dir.path().join(sub)).expect("mkdir");
    }
    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));

    assert!(reg.errors.is_empty(), "errors: {:?}", reg.errors);
    assert_eq!(surfaces(&reg), surfaces(&baseline));
}

// ---------------------------------------------------------------------------
// 2. Error quality
// ---------------------------------------------------------------------------

/// One good entry beside each bad one, so we can prove the bad file is
/// reported AND that it does not take its neighbour down with it.
fn good_neighbours() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "engines/ollama.toml",
            "label = \"Ollama\"\nbin = \"ollama\"\nkind = \"plain-lines\"\n",
        ),
        (
            "agents/good/agent.toml",
            "label = \"Good\"\nengine = \"ollama\"\n",
        ),
        (
            "skills/good/skill.toml",
            "description = \"Good skill\"\nprompt = \"Do the thing.\"\n",
        ),
        (
            "commands/good.toml",
            "description = \"Good command\"\nkind = \"shell\"\nargv = [\"true\"]\n",
        ),
    ]
}

/// Assert exactly one error mentions `file`, that its rendered one-liner
/// names the file path, and that the message names `key` and explains itself.
#[track_caller]
fn assert_actionable(reg: &Registry, file: &str, key: &str, kind: RegistryEntityKind) {
    let rendered: Vec<String> = reg.errors.iter().map(ToString::to_string).collect();
    let hits: Vec<&registry::RegistryError> = reg
        .errors
        .iter()
        .filter(|e| e.file.as_ref().is_some_and(|f| f.ends_with(file)))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one error naming {file}, got: {rendered:#?}"
    );
    let err = hits[0];
    let line = err.to_string();
    assert!(
        line.contains(file),
        "error must name the offending file {file}: {line}"
    );
    assert!(
        line.contains(&format!("`{key}`")),
        "error must name the offending key `{key}`: {line}"
    );
    assert!(
        err.message.len() > format!("key `{key}`: ").len() + 4,
        "error must say what was expected, not just name the key: {line}"
    );
    assert_eq!(err.kind, kind, "wrong entity kind for {file}: {line}");
}

/// A registry loaded from the good neighbours plus one bad file.
fn with_bad(bad: (&str, &str)) -> (tempfile::TempDir, Registry) {
    let mut files = good_neighbours();
    files.push(bad);
    write_config(&files)
}

/// The good entries all survived, whatever went wrong elsewhere.
#[track_caller]
fn assert_neighbours_survived(reg: &Registry) {
    assert!(reg.engines.contains_key("ollama"), "engine neighbour lost");
    assert!(reg.agents.contains_key("good"), "agent neighbour lost");
    assert!(reg.skills.contains_key("good"), "skill neighbour lost");
    assert!(reg.commands.contains_key("good"), "command neighbour lost");
}

#[test]
fn a_malformed_command_names_its_file_and_key() {
    // `argv` is required for kind = "shell" and must be a list of strings.
    let (_d, reg) = with_bad((
        "commands/broken.toml",
        "description = \"Broken\"\nkind = \"shell\"\nargv = \"rm -rf /\"\n",
    ));
    assert_actionable(&reg, "commands/broken.toml", "argv", RegistryEntityKind::Command);
    assert!(
        !reg.commands.contains_key("broken"),
        "bad command was loaded anyway"
    );
    assert_neighbours_survived(&reg);
}

#[test]
fn a_command_pointing_at_an_unknown_agent_names_the_key_and_the_options() {
    let (_d, reg) = with_bad((
        "commands/ghosty.toml",
        "description = \"Ghosty\"\nkind = \"prompt\"\ntemplate = \"help\"\nagent = \"nobody\"\n",
    ));
    assert_actionable(&reg, "commands/ghosty.toml", "agent", RegistryEntityKind::Command);
    let line = reg.errors[0].to_string();
    assert!(line.contains("nobody"), "error must quote the bad value: {line}");
    assert_neighbours_survived(&reg);
}

#[test]
fn a_malformed_skill_names_its_file_and_key() {
    // `description` must be a non-empty string.
    let (_d, reg) = with_bad((
        "skills/broken/skill.toml",
        "description = 7\nprompt = \"Do the thing.\"\n",
    ));
    assert_actionable(
        &reg,
        "skills/broken/skill.toml",
        "description",
        RegistryEntityKind::Skill,
    );
    assert!(!reg.skills.contains_key("broken"), "bad skill was loaded anyway");
    assert_neighbours_survived(&reg);
}

#[test]
fn a_malformed_agent_names_its_file_and_key() {
    // `engine` must reference an engine that exists.
    let (_d, reg) = with_bad((
        "agents/broken/agent.toml",
        "label = \"Broken\"\nengine = \"gpt5\"\n",
    ));
    assert_actionable(
        &reg,
        "agents/broken/agent.toml",
        "engine",
        RegistryEntityKind::Agent,
    );
    let line = reg.errors[0].to_string();
    assert!(line.contains("gpt5"), "error must quote the bad value: {line}");
    assert!(
        line.contains("known:") && line.contains("ollama"),
        "an unknown-reference error should list the known options: {line}"
    );
    assert_neighbours_survived(&reg);
}

#[test]
fn unparseable_toml_is_reported_against_the_file_not_panicked_on() {
    for (file, body) in [
        (
            "commands/syntax.toml",
            "description = \"x\"\nkind = \"shell\"\nargv = [\n",
        ),
        ("skills/syntax/skill.toml", "description = = \"x\"\n"),
        ("agents/syntax/agent.toml", "label = \"x\"\nengine\n"),
    ] {
        let (_d, reg) = with_bad((file, body));
        let hits: Vec<String> = reg
            .errors
            .iter()
            .filter(|e| e.file.as_ref().is_some_and(|f| f.ends_with(file)))
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "expected one error for {file}, got: {:?}",
            reg.errors
        );
        assert!(
            hits[0].contains(file),
            "a TOML syntax error must name the file: {}",
            hits[0]
        );
        assert_neighbours_survived(&reg);
    }
}

#[test]
fn a_broken_file_never_takes_the_bridge_down() {
    // All four broken at once: the daemon still boots, still has a usable
    // command table, still answers /help, and reports every problem.
    let mut files = good_neighbours();
    files.extend([
        ("commands/b1.toml", "kind = \"shell\"\nargv = 3\n"),
        ("skills/b2/skill.toml", "description = []\n"),
        ("agents/b3/agent.toml", "engine = 12\n"),
        ("engines/b4.toml", "label = \"B4\"\n"),
    ]);
    let (_d, reg) = write_config(&files);

    assert_neighbours_survived(&reg);
    assert!(
        reg.errors.len() >= 4,
        "every broken file must be reported, got {}: {:?}",
        reg.errors.len(),
        reg.errors.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
    for e in &reg.errors {
        assert!(e.file.is_some(), "every registry error must name a file: {e}");
        assert!(!e.message.trim().is_empty(), "empty error message");
    }
    // The bridge surfaces still work.
    let table = commands::table(&reg);
    assert!(table.contains_key("good"));
    assert!(matches!(
        commands::resolve(&table, "/good"),
        Dispatch::Command { .. } | Dispatch::Confirm { .. }
    ));
    assert!(!commands::help_text(&reg).is_empty());
}

/// A broken user override must REVERT to the shipped command, not delete it.
///
/// `Registry.commands` is documented as "the shipped table with user files
/// substituted in place", and the loader's cross-reference pass used to
/// `shift_remove` the rejected entry from the merged table — deleting the
/// built-in the user was overriding. It looked fine only because every
/// current consumer redundantly re-applied `effective_table`.
#[test]
fn a_broken_user_override_reverts_to_the_shipped_command_rather_than_deleting_it() {
    let shipped = stackhour_core::registry::command::builtin_commands();
    let victim = shipped
        .keys()
        .next()
        .expect("there is at least one shipped command")
        .clone();

    // A user file overriding a shipped command, pointing at a template that
    // does not exist — rejected by cross-reference.
    let rel = format!("commands/{victim}.toml");
    let (_d, reg) = write_config(&[(
        rel.as_str(),
        "description = \"Mine\"\nkind = \"prompt\"\ntemplate = \"nope-missing\"\n",
    )]);

    assert!(
        !reg.errors.is_empty(),
        "the broken override should have been reported"
    );
    assert!(
        reg.commands.contains_key(&victim),
        "/{victim} vanished from Registry.commands instead of reverting; keys = {:?}",
        reg.commands.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        reg.commands[&victim].description, shipped[&victim].description,
        "/{victim} must be the SHIPPED definition, not the rejected user one"
    );
    assert_ne!(
        reg.commands[&victim].description, "Mine",
        "the rejected user definition survived"
    );
    // Every shipped command still present, in the shipped order.
    assert_eq!(
        reg.commands.keys().take(shipped.len()).collect::<Vec<_>>(),
        shipped.keys().collect::<Vec<_>>(),
        "the restored command lost its slot in the shipped order"
    );
}

/// Agents get the same NAMED cycle path that skills and commands get.
///
/// `agent_extends` was a stub returning `Vec::new()`, so the shared cycle
/// checker was a permanent no-op for agents and a 3-agent loop produced three
/// identical "unresolvable inheritance chain" messages with no path.
#[test]
fn an_agent_extends_cycle_reports_the_named_path() {
    let (_d, reg) = write_config(&[
        (
            "engines/e.toml",
            "label = \"E\"\nbin = \"true\"\nprompt_arg = \"-p\"\n",
        ),
        (
            "agents/a/agent.toml",
            "label = \"A\"\nengine = \"e\"\nextends = \"b\"\n",
        ),
        (
            "agents/b/agent.toml",
            "label = \"B\"\nengine = \"e\"\nextends = \"a\"\n",
        ),
    ]);

    let lines: Vec<String> = reg
        .errors
        .iter()
        .filter(|e| e.kind == RegistryEntityKind::Agent)
        .map(ToString::to_string)
        .collect();
    assert!(!lines.is_empty(), "the cycle was not reported at all");
    assert!(
        lines.iter().any(|l| l.contains("a -> b -> a") || l.contains("b -> a -> b")),
        "expected a named cycle path like `a -> b -> a`, got: {lines:#?}"
    );
    assert!(
        !reg.agents.contains_key("a") && !reg.agents.contains_key("b"),
        "agents on a cycle must be dropped"
    );
}
