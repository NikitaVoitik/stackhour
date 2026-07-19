//! End-to-end composition of the five config pillars.
//!
//! Every other test in the tree exercises one pillar against hand-built inputs.
//! This one starts from a real on-disk config directory and walks the whole
//! chain the daemon walks:
//!
//!   loader -> command (`kind = "skill"`) -> skill (`uses` + `agent`)
//!          -> agent (`skills` + `extends`) -> soul doc -> prompt template
//!          -> the argv actually handed to the child process.
//!
//! It exists to catch seam drift: a pillar that parses a field nobody reads, a
//! type that two pillars define differently, a template only one side knows
//! the name of.

use indexmap::IndexMap;
use serde_json::json;
use std::fs;

use stackhour_bridge::commands::{self, Dispatch};
use stackhour_bridge::engines::RunRequest;
use stackhour_bridge::skills;
use stackhour_bridge::souls;
use stackhour_bridge::state::BridgeState;
use stackhour_core::registry::{self, Registry};

/// Write a config tree and load it through the same entry point the daemon
/// uses. Returns the tempdir so it outlives the registry.
fn config(files: &[(&str, &str)]) -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (rel, body) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        fs::write(&path, body).expect("write");
    }
    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));
    (dir, reg)
}

/// The full stack: an engine that takes a system prompt as a flag, an agent
/// that extends a base agent and carries both a soul and an overlay, a skill
/// that composes another skill and binds the agent, and a command that invokes
/// the skill.
fn full_stack_files() -> Vec<(&'static str, &'static str)> {
    vec![
        // ---- engine: declares a system-prompt flag and an effort flag ----
        (
            "engines/ollama.toml",
            r#"
label = "Ollama"
bin = "ollama"
kind = "plain-lines"
args = ["run"]
system_prompt_args = ["--system", "{{system_prompt}}"]
effort_args = ["--effort", "{{effort}}"]
"#,
        ),
        // ---- agents: `strict` extends `base` ----
        (
            "agents/base/agent.toml",
            r#"
label = "Base"
engine = "ollama"
skills = ["house-style"]

[tools]
deny = ["WebFetch"]
"#,
        ),
        ("agents/base/soul.md", "You are careful.\n"),
        (
            "agents/strict/agent.toml",
            r#"
label = "Strict Reviewer"
extends = "base"
effort = "high"
model = "llama3"
overlays = ["overlay.md"]

[tools]
allow = ["Bash"]
"#,
        ),
        ("agents/strict/soul.md", "You review code.\n"),
        ("agents/strict/overlay.md", "Lead with the verdict.\n"),
        // ---- skills: `review` uses `house-style` and binds the agent ----
        (
            "skills/house-style/skill.toml",
            r#"
description = "House style"

[tools]
deny = ["Write"]
"#,
        ),
        ("skills/house-style/skill.md", "Prefer short sentences.\n"),
        (
            "skills/review/skill.toml",
            r#"
description = "Review a PR"
agent = "strict"
template = "review-prompt"
uses = ["house-style"]

[[args]]
name = "pr"
required = true

[[args]]
name = "focus"
default = "correctness"
rest = true

[tools]
allow = ["Grep"]

[env]
REVIEW_MODE = "strict"
"#,
        ),
        ("skills/review/skill.md", "Read before you write.\n"),
        // ---- prompt template referenced by the skill ----
        (
            "prompts/review-prompt.md",
            "Review PR {{pr}} focusing on {{focus}}.",
        ),
        // ---- command that invokes the skill ----
        (
            "commands/review.toml",
            r#"
description = "Review a pull request"
kind = "skill"
skill = "review"
aliases = ["rv"]
"#,
        ),
    ]
}

fn state() -> BridgeState {
    BridgeState {
        offset: 0,
        active: "gcp".into(),
        engine: "ollama".into(),
        agent: None,
        sessions: IndexMap::new(),
        raw: json!({}),
    }
}

/// The whole chain, in one test, asserting at every seam.
#[test]
fn a_command_invokes_a_skill_that_runs_under_an_agent_with_a_soul_from_the_config_dir() {
    let (_d, reg) = config(&full_stack_files());
    assert!(reg.errors.is_empty(), "registry errors: {:?}", reg.errors);

    // -- seam 1: the loader saw every pillar --------------------------------
    assert!(reg.engines.contains_key("ollama"), "engine not loaded");
    assert!(reg.agents.contains_key("strict"), "agent not loaded");
    assert!(reg.skills.contains_key("review"), "skill not loaded");
    assert!(reg.commands.contains_key("review"), "command not loaded");

    // -- seam 2: the command resolves, by name and by alias ------------------
    let table = commands::table(&reg);
    let by_alias = commands::resolve(&table, "/rv 4821 the error paths");
    assert_eq!(
        by_alias,
        Dispatch::Command {
            command: "review".into(),
            raw: "4821 the error paths".into(),
        },
        "alias did not resolve to the command"
    );

    let Dispatch::Command { command, raw } = commands::resolve(&table, "/review 4821 the error paths") else {
        panic!("expected a command dispatch");
    };
    let def = table.get(&command).expect("command in table");
    let skill_name = def.skill.as_deref().expect("kind=skill declares a skill");
    assert_eq!(skill_name, "review");

    // -- seam 3: the skill plans: args bind, template renders, uses compose --
    let plan = skills::plan_invocation(&reg, skill_name, &raw).expect("plan");
    assert_eq!(plan.args["pr"], "4821");
    assert_eq!(plan.args["focus"], "the error paths");
    assert_eq!(
        plan.user_prompt, "Review PR 4821 focusing on the error paths.",
        "the prompt template did not render with the bound args"
    );
    // dependency-first: house-style before review.
    let fragments: Vec<&str> = plan.system_fragments.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(fragments, vec!["house-style", "review"]);
    assert_eq!(plan.env["REVIEW_MODE"], "strict");
    assert_eq!(plan.agent_override.as_deref(), Some("strict"));

    // -- seam 4: agent resolution honours the skill's override ---------------
    let agent = souls::resolve_agent(
        &state(),
        &reg,
        def.agent.as_deref(),
        plan.agent_override.as_deref(),
    )
    .expect("the skill's agent must be selected");
    assert_eq!(agent.name, "strict");
    // `extends` pulled the engine down from `base`, and the child's own
    // fields won where it set them.
    assert_eq!(agent.engine, "ollama", "engine not inherited from base");
    assert_eq!(agent.model.as_deref(), Some("llama3"));
    assert_eq!(agent.effort.as_deref(), Some("high"));

    // -- seam 5: the soul doc + overlay + skill bodies compose ---------------
    let system = souls::compose_system_prompt(agent, &reg).expect("system prompt");
    assert!(
        system.contains("You review code."),
        "the agent's own soul is missing:\n{system}"
    );
    assert!(
        system.contains("Lead with the verdict."),
        "the overlay is missing:\n{system}"
    );
    assert!(
        system.contains("Prefer short sentences."),
        "the inherited skill body is missing:\n{system}"
    );

    // -- seam 6: applying the agent fills the run request --------------------
    let engine = reg.engines.get("ollama").expect("engine").clone();
    let mut req = RunRequest {
        prompt: plan.user_prompt.clone(),
        ..RunRequest::default()
    };
    souls::apply_agent(&engine, Some(agent), &reg, &mut req);
    assert_eq!(req.model.as_deref(), Some("llama3"));
    let sp = req
        .system_prompt
        .as_deref()
        .expect("an engine with system_prompt_args gets the prompt out of band");
    assert!(sp.contains("You review code."));
    // Out-of-band delivery must leave the user prompt alone.
    assert_eq!(req.prompt, "Review PR 4821 focusing on the error paths.");

    // -- seam 7: effort is spliced because the ENGINE declares the flag ------
    assert_eq!(souls::agent_effort(&engine, Some(agent)), Some("high"));

    // -- seam 8: tool policy unions agent + its skills, deny winning ---------
    let policy = souls::tool_policy(Some(agent), &reg);
    assert!(policy.allow.contains(&"Bash".to_string()));
    assert!(
        policy.deny.contains(&"WebFetch".to_string()),
        "the inherited deny was lost: {policy:?}"
    );
    assert!(
        policy.deny.contains(&"Write".to_string()),
        "the skill's deny was lost: {policy:?}"
    );
}

/// The same tree, but the engine declares no `system_prompt_args`: the system
/// prompt must arrive prepended to the user prompt instead of being dropped.
#[test]
fn an_engine_without_a_system_flag_gets_the_soul_prepended_to_the_prompt() {
    let mut files = full_stack_files();
    files.retain(|(name, _)| *name != "engines/ollama.toml");
    files.push((
        "engines/ollama.toml",
        r#"
label = "Ollama"
bin = "ollama"
kind = "plain-lines"
args = ["run"]
"#,
    ));
    let (_d, reg) = config(&files);
    assert!(reg.errors.is_empty(), "registry errors: {:?}", reg.errors);

    let agent = reg.agents.get("strict").expect("agent");
    let engine = reg.engines.get("ollama").expect("engine").clone();
    let mut req = RunRequest {
        prompt: "Review PR 1.".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(&engine, Some(agent), &reg, &mut req);

    assert!(
        req.system_prompt.is_none(),
        "an engine with no system flag must not carry an out-of-band prompt"
    );
    assert!(
        req.prompt.contains("You review code."),
        "the soul was dropped instead of being folded into the prompt:\n{}",
        req.prompt
    );
    assert!(
        req.prompt.contains("Review PR 1."),
        "the user's own text was lost:\n{}",
        req.prompt
    );

    // An engine with no effort flag ignores the agent's effort rather than
    // failing the turn.
    assert_eq!(souls::agent_effort(&engine, Some(agent)), None);
}

/// A command naming a skill that does not exist must be reported by the
/// loader, not discovered at dispatch time.
#[test]
fn a_command_pointing_at_an_unknown_skill_is_a_load_time_error() {
    let (_d, reg) = config(&[(
        "commands/review.toml",
        "description = \"Review\"\nkind = \"skill\"\nskill = \"nope\"\n",
    )]);
    assert!(
        reg.errors.iter().any(|e| e.message.contains("nope")),
        "expected a dangling-skill error, got: {:?}",
        reg.errors
    );
    assert!(
        !reg.commands.contains_key("review"),
        "a command with a dangling skill must not be dispatchable"
    );
}
