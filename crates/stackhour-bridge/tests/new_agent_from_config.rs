//! Extensibility gate: a brand-new AGENT with its own SOUL, added by CONFIG ONLY.
//!
//! Nothing in this file requires a Rust source change. It writes a config
//! directory that a user could write by hand — one `agents/oracle/agent.toml`,
//! one `agents/oracle/soul.md`, one overlay, a command that binds to it, and a
//! `config.json` naming it as the default — then walks the same chain the
//! daemon walks and asserts:
//!
//!   1. the agent loads clean and is selectable by name;
//!   2. its SOUL TEXT reaches the engine inside the system prompt
//!      (`souls::apply_agent` -> `RunRequest::system_prompt` -> argv), and is
//!      observed arriving in a real spawned child process;
//!   3. it is selectable PER CONVERSATION (`/agent <name>`), and that beats
//!      the configured default;
//!   4. it is selectable PER COMMAND, and that beats the conversation;
//!   5. the CONFIGURED DEFAULT (`config.json` `bridge.defaultAgent`, and
//!      `STACKHOUR_AGENT`) is used when no agent is named anywhere;
//!   6. hand-editing soul.md is live on the next composition, no reload.
//!
//! If this file ever needs a source change to pass, the config layer has
//! stopped being extensible and that is the bug.

use std::fs;
use std::path::Path;

use stackhour_bridge::engines::RunRequest;
use stackhour_bridge::souls;
use stackhour_bridge::state::BridgeState;
use stackhour_core::registry::engine::ArgvVars;
use stackhour_core::registry::{self, Registry};

/// A phrase that appears nowhere in the Rust sources. Finding it inside a
/// system prompt (or a child process's argv) proves the user's markdown, and
/// not some built-in text, is what reached the engine.
const SOUL_SENTINEL: &str = "ORACLE-SOUL-SENTINEL-4417";
const OVERLAY_SENTINEL: &str = "ORACLE-OVERLAY-SENTINEL-9902";
/// A second agent, so "the right one was picked" is distinguishable from
/// "there was only one to pick".
const RIVAL_SENTINEL: &str = "SCRIBE-SOUL-SENTINEL-1188";

/// The engine the agents bind to. Declared entirely in config, and it declares
/// a `system_prompt_args` flag — so the soul must be delivered through THAT,
/// not prepended to the user's prompt.
const FAKE_ENGINE_TOML: &str = r#"
label = "Fake Echo"
emoji = "🧪"
bin = "fake-echo"
kind = "plain-lines"
args = ["--run", "-"]
model_args = ["--model", "{{model}}"]
system_prompt_args = ["--system", "{{system_prompt}}"]
"#;

/// A stand-in engine binary: echoes its argv and its stdin so the test can see
/// exactly what the child was handed.
const FAKE_ENGINE_SH: &str = r#"#!/bin/sh
prompt=$(cat)
echo "argv:$*"
echo "prompt:$prompt"
"#;

/// The whole of the user's contribution. Every file here is hand-writable.
fn agent_config() -> Vec<(&'static str, String)> {
    vec![
        ("engines/fake-echo.toml", FAKE_ENGINE_TOML.to_string()),
        (
            "agents/oracle/agent.toml",
            r#"
label = "The Oracle"
engine = "fake-echo"
model = "tiny-1"
soul = "soul.md"
overlays = ["laconic.md"]
permission_mode = "default"
"#
            .to_string(),
        ),
        (
            "agents/oracle/soul.md",
            format!("You are the Oracle.\n\n{SOUL_SENTINEL}\n"),
        ),
        ("agents/oracle/laconic.md", format!("{OVERLAY_SENTINEL}\n")),
        (
            "agents/scribe/agent.toml",
            "label = \"The Scribe\"\nengine = \"fake-echo\"\n".to_string(),
        ),
        (
            "agents/scribe/soul.md",
            format!("You are the Scribe.\n\n{RIVAL_SENTINEL}\n"),
        ),
        // A command that binds itself to the Oracle.
        (
            "commands/consult.toml",
            r#"
description = "Ask the Oracle"
kind = "prompt"
template = "consult"
agent = "oracle"
"#
            .to_string(),
        ),
        ("prompts/consult.md", "Consult: {{prompt}}\n".to_string()),
        // The configured default, used when nothing else names an agent.
        (
            "config.json",
            r#"{ "bridge": { "defaultAgent": "oracle" } }"#.to_string(),
        ),
    ]
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
    fs::write(&path, body).expect("write");
}

/// Materialise the config tree and load it through the daemon's own entry
/// point. The tempdir must outlive the registry.
fn fixture() -> (tempfile::TempDir, Registry) {
    fixture_with_env(&[])
}

fn fixture_with_env(env: &[(&str, &str)]) -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (rel, body) in agent_config() {
        write(dir.path(), rel, &body);
    }
    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(env));
    assert!(
        reg.errors.is_empty(),
        "a hand-written agent config must load clean; errors: {:?}",
        reg.errors
    );
    (dir, reg)
}

/// A conversation with nothing selected — the state of a fresh chat.
fn fresh_state() -> BridgeState {
    BridgeState {
        offset: 0,
        active: "gcp".into(),
        engine: "fake-echo".into(),
        agent: None,
        sessions: Default::default(),
        raw: serde_json::json!({}),
    }
}

// ---------------------------------------------------------------------------
// 1. The agent exists, purely because a directory does.
// ---------------------------------------------------------------------------

#[test]
fn a_config_only_agent_loads_and_is_selectable_by_name() {
    let (_d, reg) = fixture();

    let agent = reg
        .agents
        .get("oracle")
        .expect("an agent defined purely in config must be selectable by name");
    assert_eq!(agent.name, "oracle");
    assert_eq!(agent.label, "The Oracle");
    assert_eq!(agent.engine, "fake-echo");
    assert_eq!(agent.model.as_deref(), Some("tiny-1"));

    // It shows up in the user-facing listing, so it is discoverable.
    let listing = souls::agent_list_text(&reg);
    assert!(
        listing.contains("oracle") && listing.contains("The Oracle"),
        "the new agent is missing from /agents:\n{listing}"
    );
}

// ---------------------------------------------------------------------------
// 2. The soul text actually reaches the engine.
// ---------------------------------------------------------------------------

#[test]
fn the_soul_document_reaches_the_engine_inside_the_system_prompt() {
    let (_d, reg) = fixture();
    let agent = reg.agents.get("oracle").expect("agent");
    let engine = reg.engines.get("fake-echo").expect("engine").clone();

    let composed = souls::compose_system_prompt(agent, &reg).expect("compose");
    assert!(
        composed.contains(SOUL_SENTINEL),
        "the soul.md text is missing from the composed system prompt:\n{composed}"
    );
    assert!(
        composed.contains(OVERLAY_SENTINEL),
        "the declared overlay is missing from the composed system prompt:\n{composed}"
    );

    let mut req = RunRequest {
        prompt: "who am I speaking to?".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(&engine, Some(agent), &reg, &mut req);

    // The engine DECLARES a system-prompt flag, so the soul must ride that
    // flag rather than being smuggled into the user's message.
    let system = req
        .system_prompt
        .as_deref()
        .expect("an engine with system_prompt_args must receive the soul there");
    assert!(
        system.contains(SOUL_SENTINEL),
        "the soul did not reach RunRequest::system_prompt:\n{system}"
    );
    assert_eq!(
        req.prompt, "who am I speaking to?",
        "the user's prompt must be left alone when the engine takes a system flag"
    );
    // The agent's other settings ride along.
    assert_eq!(req.model.as_deref(), Some("tiny-1"));
    assert_eq!(req.permission_mode.as_deref(), Some("default"));

    // ...and the flag the CONFIG declared is what carries it into argv.
    let argv = engine.assemble_argv(&ArgvVars {
        model: req.model.as_deref(),
        system_prompt: req.system_prompt.as_deref(),
        ..ArgvVars::default()
    });
    let flag = argv
        .iter()
        .position(|a| a == "--system")
        .expect("the engine's declared system flag is missing from argv");
    assert!(
        argv[flag + 1].contains(SOUL_SENTINEL),
        "the soul was not spliced into the declared flag: {argv:?}"
    );
}

/// The end of the chain: a real child process, spawned with the argv the
/// config produced, observes the soul text in its own arguments.
#[test]
#[cfg(unix)]
fn the_soul_text_is_observed_arriving_at_a_real_child_process() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let (dir, reg) = fixture();
    let agent = reg.agents.get("oracle").expect("agent");
    let engine = reg.engines.get("fake-echo").expect("engine").clone();

    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("mkdir bin");
    let script = bin_dir.join("fake-echo");
    fs::write(&script, FAKE_ENGINE_SH).expect("write script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

    let mut req = RunRequest {
        prompt: "SENTINEL-PROMPT-7391".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(&engine, Some(agent), &reg, &mut req);
    let argv = engine.assemble_argv(&ArgvVars {
        model: req.model.as_deref(),
        system_prompt: req.system_prompt.as_deref(),
        ..ArgvVars::default()
    });

    let mut child = Command::new(&engine.bin)
        .args(&argv)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the config's `bin` must resolve");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(req.prompt.as_bytes())
        .expect("write prompt");
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

    assert!(out.status.success(), "child failed: {stdout}");
    // The soul is multi-line markdown, so the `argv:` echo spans lines: take
    // everything the child printed before it echoed the prompt.
    let echoed_argv = stdout
        .split_once("\nprompt:")
        .map(|(argv, _)| argv.to_string())
        .unwrap_or_else(|| stdout.clone());
    assert!(
        echoed_argv.starts_with("argv:") && echoed_argv.contains(SOUL_SENTINEL),
        "the soul text never reached the child's argv:\n{stdout}"
    );
    assert!(
        stdout.contains("prompt:SENTINEL-PROMPT-7391"),
        "the user's prompt was not delivered intact:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// 3. Selectable per conversation.
// ---------------------------------------------------------------------------

#[test]
fn the_agent_is_selectable_per_conversation() {
    let (_d, reg) = fixture();
    let mut state = fresh_state();

    let reply = souls::select_agent(&mut state, &reg, "scribe").expect("/agent scribe");
    assert!(
        reply.contains("The Scribe"),
        "the selection reply should name the agent: {reply}"
    );
    assert_eq!(state.agent.as_deref(), Some("scribe"));

    // The conversation's choice beats the configured default (oracle).
    let active = souls::resolve_agent(&state, &reg, None, None).expect("an agent must be active");
    assert_eq!(active.name, "scribe");
    let system = souls::compose_system_prompt(active, &reg).expect("compose");
    assert!(
        system.contains(RIVAL_SENTINEL) && !system.contains(SOUL_SENTINEL),
        "the conversation's agent must be the one whose soul is composed:\n{system}"
    );

    // ...and it can be cleared back to the default.
    souls::select_agent(&mut state, &reg, "none").expect("/agent none");
    assert_eq!(state.agent, None);
    assert_eq!(
        souls::resolve_agent(&state, &reg, None, None).map(|a| a.name.as_str()),
        Some("oracle"),
        "clearing the conversation agent must fall back to the configured default"
    );
}

// ---------------------------------------------------------------------------
// 4. Selectable per command.
// ---------------------------------------------------------------------------

#[test]
fn the_agent_is_selectable_per_command_and_beats_the_conversation() {
    let (_d, reg) = fixture();

    // The command declared `agent = "oracle"` in its own TOML.
    let command = reg
        .commands
        .get("consult")
        .expect("commands/consult.toml must load");
    assert_eq!(command.agent.as_deref(), Some("oracle"));

    // A conversation pinned to the OTHER agent.
    let mut state = fresh_state();
    state.agent = Some("scribe".into());

    let active = souls::resolve_agent(&state, &reg, command.agent.as_deref(), None)
        .expect("the command's agent must resolve");
    assert_eq!(
        active.name, "oracle",
        "a command that names an agent must override the conversation's"
    );
    let system = souls::compose_system_prompt(active, &reg).expect("compose");
    assert!(
        system.contains(SOUL_SENTINEL),
        "the command-selected agent's soul must be the composed one:\n{system}"
    );
}

// ---------------------------------------------------------------------------
// 5. The configured default.
// ---------------------------------------------------------------------------

#[test]
fn the_configured_default_agent_is_used_when_none_is_named() {
    let (_d, reg) = fixture();
    assert_eq!(
        reg.defaults.agent.as_deref(),
        Some("oracle"),
        "config.json bridge.defaultAgent must resolve"
    );

    let state = fresh_state();
    let active = souls::resolve_agent(&state, &reg, None, None)
        .expect("with a configured default, a turn must have an agent");
    assert_eq!(active.name, "oracle");

    let engine = reg.engines.get("fake-echo").expect("engine").clone();
    let mut req = RunRequest {
        prompt: "hello".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(&engine, Some(active), &reg, &mut req);
    assert!(
        req.system_prompt
            .as_deref()
            .is_some_and(|s| s.contains(SOUL_SENTINEL)),
        "the default agent's soul must reach the engine with no explicit selection"
    );
}

#[test]
fn the_env_override_wins_over_the_configured_default() {
    let (_d, reg) = fixture_with_env(&[("STACKHOUR_AGENT", "scribe")]);
    assert_eq!(reg.defaults.agent.as_deref(), Some("scribe"));

    let state = fresh_state();
    let active = souls::resolve_agent(&state, &reg, None, None).expect("agent");
    assert_eq!(active.name, "scribe");
}

/// Without a default and without a selection there is NO agent — the
/// no-config user's run must stay untouched.
#[test]
fn no_default_and_no_selection_means_no_agent_at_all() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "engines/fake-echo.toml", FAKE_ENGINE_TOML);
    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));

    let state = fresh_state();
    assert!(souls::resolve_agent(&state, &reg, None, None).is_none());

    let engine = reg.engines.get("fake-echo").expect("engine").clone();
    let mut req = RunRequest {
        prompt: "hello".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(&engine, None, &reg, &mut req);
    assert_eq!(req.prompt, "hello");
    assert!(req.system_prompt.is_none());
    assert!(req.model.is_none());
}

// ---------------------------------------------------------------------------
// 6. Hand edits are live.
// ---------------------------------------------------------------------------

#[test]
fn editing_the_soul_document_takes_effect_without_a_reload() {
    let (dir, reg) = fixture();
    let agent = reg.agents.get("oracle").expect("agent");
    assert!(souls::compose_system_prompt(agent, &reg)
        .expect("compose")
        .contains(SOUL_SENTINEL));

    // The user opens soul.md in an editor and saves.
    let soul_path = dir.path().join("agents/oracle/soul.md");
    let before = fs::metadata(&soul_path).expect("stat").modified().expect("mtime");
    fs::write(&soul_path, "I have been rewritten. ORACLE-SOUL-EDITED-8823\n").expect("write");
    // Filesystems with coarse mtimes need the stamp to actually move, or the
    // cache would legitimately serve the old text.
    if fs::metadata(&soul_path).expect("stat").modified().expect("mtime") == before {
        let bumped = before + std::time::Duration::from_secs(2);
        fs::File::options()
            .write(true)
            .open(&soul_path)
            .expect("open")
            .set_modified(bumped)
            .expect("set mtime");
    }

    let after = souls::compose_system_prompt(agent, &reg).expect("compose");
    assert!(
        after.contains("ORACLE-SOUL-EDITED-8823") && !after.contains(SOUL_SENTINEL),
        "a hand edit to soul.md must be live on the next composition:\n{after}"
    );
}
