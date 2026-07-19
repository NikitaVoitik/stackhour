//! An agent's `[tools]` allow/deny policy must reach the CHILD PROCESS.
//!
//! It used to parse, validate, inherit and merge cleanly — `souls::tool_policy`
//! and `skills::merge_tools` computed a full `ToolPolicy` — and then stop dead:
//! `RunRequest` had no tools field and `EngineDef` declared no tool flag
//! templates, so nothing spliced it into argv. A user who wrote
//! `[tools] deny = ["Bash"]` got a config that reported no error while the
//! agent kept full tool access. That is a silent, security-shaped failure, so
//! these tests assert on the assembled argv rather than on the policy struct.

use std::fs;

use stackhour_bridge::engines::{self, RunRequest};
use stackhour_bridge::souls;
use stackhour_core::registry::{self, Registry};

/// An engine that DECLARES both tool flags, so a policy is enforceable.
const ENFORCING_ENGINE: &str = r#"
label = "Enforcer"
bin = "enforcer"
kind = "plain-lines"
args = ["--run", "-"]
allowed_tools_args = ["--allowedTools", "{{allowed_tools}}"]
disallowed_tools_args = ["--disallowedTools", "{{disallowed_tools}}"]
"#;

/// An engine that declares NEITHER, so a policy cannot be honoured.
const BLIND_ENGINE: &str = r#"
label = "Blind"
bin = "blind"
kind = "plain-lines"
args = ["--run", "-"]
"#;

fn load(files: &[(&str, &str)]) -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (rel, body) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        fs::write(&path, body).expect("write");
    }
    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));
    (dir, reg)
}

/// Assemble the argv an agent's turn would actually spawn.
fn argv_for(reg: &Registry, engine: &str, agent: &str) -> Vec<String> {
    let def = reg.engines.get(engine).expect("engine loaded");
    let agent_def = reg.agents.get(agent).expect("agent loaded");
    let mut req = RunRequest {
        prompt: "hello".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(def, Some(agent_def), reg, &mut req);
    engines::build_argv(def, &req)
}

/// The headline regression: a denied tool appears in the child's argv.
#[test]
fn a_denied_tool_reaches_the_child_argv() {
    let (_d, reg) = load(&[
        ("engines/enforcer.toml", ENFORCING_ENGINE),
        (
            "agents/careful/agent.toml",
            "label = \"Careful\"\nengine = \"enforcer\"\n\n[tools]\ndeny = [\"Bash\", \"WebFetch\"]\n",
        ),
    ]);
    assert!(reg.errors.is_empty(), "{:#?}", reg.errors);

    let argv = argv_for(&reg, "enforcer", "careful");
    let at = argv
        .iter()
        .position(|a| a == "--disallowedTools")
        .unwrap_or_else(|| panic!("deny list never reached argv: {argv:?}"));
    assert_eq!(argv[at + 1], "Bash,WebFetch");
    // Nothing was allowed, so no whitelist flag should appear at all.
    assert!(
        !argv.iter().any(|a| a == "--allowedTools"),
        "an empty allow list must not become a flag: {argv:?}"
    );
}

/// An allow list is a whitelist and must reach argv too.
#[test]
fn an_allowed_tool_list_reaches_the_child_argv() {
    let (_d, reg) = load(&[
        ("engines/enforcer.toml", ENFORCING_ENGINE),
        (
            "agents/narrow/agent.toml",
            "label = \"Narrow\"\nengine = \"enforcer\"\n\n[tools]\nallow = [\"Read\", \"Grep\"]\n",
        ),
    ]);
    let argv = argv_for(&reg, "enforcer", "narrow");
    let at = argv
        .iter()
        .position(|a| a == "--allowedTools")
        .unwrap_or_else(|| panic!("allow list never reached argv: {argv:?}"));
    assert_eq!(argv[at + 1], "Read,Grep");
}

/// A skill's `[tools]` composes into the agent's, and the UNION reaches argv —
/// this is the merge that `tool_policy` computes and that used to be discarded.
#[test]
fn a_skills_tool_policy_composes_into_the_child_argv() {
    let (_d, reg) = load(&[
        ("engines/enforcer.toml", ENFORCING_ENGINE),
        (
            "skills/readonly/skill.toml",
            "description = \"Read only\"\nprompt = \"Do not write.\"\n\n[tools]\ndeny = [\"Write\"]\nallow = [\"Read\"]\n",
        ),
        (
            "agents/composed/agent.toml",
            "label = \"Composed\"\nengine = \"enforcer\"\nskills = [\"readonly\"]\n\n[tools]\ndeny = [\"Bash\"]\n",
        ),
    ]);
    assert!(reg.errors.is_empty(), "{:#?}", reg.errors);

    let argv = argv_for(&reg, "enforcer", "composed");
    let deny_at = argv.iter().position(|a| a == "--disallowedTools").unwrap();
    let deny: Vec<&str> = argv[deny_at + 1].split(',').collect();
    assert!(deny.contains(&"Bash"), "agent's own deny lost: {argv:?}");
    assert!(deny.contains(&"Write"), "skill's deny lost: {argv:?}");

    let allow_at = argv.iter().position(|a| a == "--allowedTools").unwrap();
    assert_eq!(argv[allow_at + 1], "Read");
}

/// `deny` beats `allow` all the way to argv: a tool both sides mention must
/// not be handed back through the whitelist.
#[test]
fn deny_beats_allow_in_the_child_argv() {
    let (_d, reg) = load(&[
        ("engines/enforcer.toml", ENFORCING_ENGINE),
        (
            "skills/permissive/skill.toml",
            "description = \"Permissive\"\nprompt = \"x\"\n\n[tools]\nallow = [\"Bash\", \"Read\"]\n",
        ),
        (
            "agents/strict/agent.toml",
            "label = \"Strict\"\nengine = \"enforcer\"\nskills = [\"permissive\"]\n\n[tools]\ndeny = [\"Bash\"]\n",
        ),
    ]);
    let argv = argv_for(&reg, "enforcer", "strict");
    let allow_at = argv.iter().position(|a| a == "--allowedTools").unwrap();
    let allow: Vec<&str> = argv[allow_at + 1].split(',').collect();
    assert!(
        !allow.contains(&"Bash"),
        "a denied tool was re-granted through the allow list: {argv:?}"
    );
    assert!(allow.contains(&"Read"));
}

/// An engine that cannot express the policy must REFUSE, not silently grant.
///
/// This is the asymmetry with `effort`: dropping an unsupported effort flag
/// costs tuning, dropping a deny list costs the security boundary.
#[test]
fn an_engine_that_cannot_enforce_a_policy_reports_it_rather_than_ignoring_it() {
    let (_d, reg) = load(&[
        ("engines/blind.toml", BLIND_ENGINE),
        (
            "agents/hopeful/agent.toml",
            "label = \"Hopeful\"\nengine = \"blind\"\n\n[tools]\ndeny = [\"Bash\"]\n",
        ),
    ]);
    let def = reg.engines.get("blind").expect("engine loaded");
    let agent = reg.agents.get("hopeful").expect("agent loaded");
    let policy = souls::tool_policy(Some(agent), &reg);

    let complaint = def
        .unenforceable_policy(&policy)
        .expect("an engine with no tool flags must report an unenforceable deny list");
    assert!(complaint.contains("blind"), "{complaint}");
    assert!(complaint.contains("Bash"), "{complaint}");

    // And an agent with NO policy on the same engine is perfectly fine.
    let (_d2, reg2) = load(&[
        ("engines/blind.toml", BLIND_ENGINE),
        (
            "agents/plain/agent.toml",
            "label = \"Plain\"\nengine = \"blind\"\n",
        ),
    ]);
    let def2 = reg2.engines.get("blind").unwrap();
    let policy2 = souls::tool_policy(reg2.agents.get("plain"), &reg2);
    assert_eq!(def2.unenforceable_policy(&policy2), None);
}

/// Backward compatibility: an agent with no `[tools]` at all must produce the
/// exact argv it produced before tool policy existed — no empty flags.
#[test]
fn an_agent_without_a_tools_table_adds_no_flags() {
    let (_d, reg) = load(&[
        ("engines/enforcer.toml", ENFORCING_ENGINE),
        (
            "agents/plain/agent.toml",
            "label = \"Plain\"\nengine = \"enforcer\"\n",
        ),
    ]);
    let argv = argv_for(&reg, "enforcer", "plain");
    assert!(
        !argv.iter().any(|a| a == "--allowedTools" || a == "--disallowedTools"),
        "tool flags leaked into an agent that declares no policy: {argv:?}"
    );
}
