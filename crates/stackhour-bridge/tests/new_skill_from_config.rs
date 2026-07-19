//! Extensibility gate: a brand-new SKILL, added by CONFIG ONLY.
//!
//! Nothing in this file touches Rust source. It writes a config directory a
//! user could write by hand — one `skills/changelog/`, one `agents/scribe/`,
//! one `prompts/changelog.md`, one `commands/changelog.toml`, and a second
//! skill that composes the first — then loads it through the same entry point
//! the daemon uses and asserts the new skill is fully live:
//!
//!   1. it LOADS, with its argument schema, default agent, prompt template
//!      and post hook all attached,
//!   2. it is invocable AS A COMMAND: `/changelog 1.4.0 ...` dispatches, binds
//!      arguments positionally, and renders the template with them,
//!   3. it is invocable FROM ANOTHER SKILL via `uses`: the composing skill
//!      inherits its prose and tool policy while keeping its OWN arity,
//!   4. its default agent is applied, and a command may override it,
//!   5. its post hook runs for real, with `{{arg:*}}`, `{{skill}}` and
//!      `{{agent}}` substituted, and shell metacharacters in an argument stay
//!      inert because hooks are fixed argv.
//!
//! If this file ever needs a source change to pass, the config layer has
//! stopped being extensible and that is the bug.

use std::fs;
use std::path::Path;

use stackhour_bridge::commands::{self, Dispatch};
use stackhour_bridge::skills;
use stackhour_core::registry::command::CommandKind;
use stackhour_core::registry::{self, Registry};

/// The whole of the user's contribution. Six files, zero lines of Rust.
fn changelog_config(receipt: &Path) -> Vec<(String, String)> {
    let receipt = receipt.display().to_string();
    vec![
        // ---- the new skill -------------------------------------------------
        (
            "skills/changelog/skill.toml".into(),
            format!(
                r#"
description = "Draft a release changelog from the git log"
body = "skill.md"
agent = "scribe"
template = "changelog"

[[args]]
name = "version"
required = true
description = "the version being released"

[[args]]
name = "since"
default = "the previous tag"
description = "the ref to diff from"

[[args]]
name = "focus"
rest = true
default = "user-visible changes"
description = "what to emphasise"

[tools]
allow = ["Read", "Grep", "Bash"]
deny = ["Write"]

[env]
CHANGELOG_STYLE = "keepachangelog"

[hooks]
post = ["sh", "-c", "printf '%s\n' \"$1\" > {receipt}", "hook",
        "{{{{skill}}}}/{{{{agent}}}}/{{{{arg:version}}}}/{{{{arg:since}}}}/{{{{arg:focus}}}}"]
timeout_seconds = 30
"#
            ),
        ),
        (
            "skills/changelog/skill.md".into(),
            "Group entries under Added / Changed / Fixed. One line each, in the\n\
             past tense, naming the user-visible effect and never the file.\n"
                .into(),
        ),
        // ---- the prompt template it renders --------------------------------
        (
            "prompts/changelog.md".into(),
            "Write the changelog for {{version}}.\n\n\
             Diff from: {{since}}\n\
             Emphasise: {{focus}}\n\
             Raw request: {{args}}\n"
                .into(),
        ),
        // ---- the agent it defaults to --------------------------------------
        (
            "agents/scribe/agent.toml".into(),
            r#"
label = "Scribe"
engine = "claude"
soul = "soul.md"
skills = ["changelog"]
"#
            .into(),
        ),
        (
            "agents/scribe/soul.md".into(),
            "You write release notes. Terse, factual, no marketing.\n".into(),
        ),
        // ---- the command that invokes it -----------------------------------
        (
            "commands/changelog.toml".into(),
            r#"
description = "Draft a release changelog"
aliases = ["cl"]
kind = "skill"
skill = "changelog"

[[args]]
name = "version"
required = true
description = "the version being released"
"#
            .into(),
        ),
        // ---- a SECOND skill that invokes the first via `uses` ---------------
        (
            "skills/release/skill.toml".into(),
            r#"
description = "Run a release: changelog, then tag"
body = "skill.md"
uses = ["changelog"]

[[args]]
name = "version"
required = true
description = "the version being released"

[tools]
deny = ["Bash"]
"#
            .into(),
        ),
        (
            "skills/release/skill.md".into(),
            "Cut the release only after the changelog has been approved.\n".into(),
        ),
    ]
}

fn loaded() -> (tempfile::TempDir, Registry, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let receipt = dir.path().join("hook-receipt.txt");
    for (rel, body) in changelog_config(&receipt) {
        let path = dir.path().join(&rel);
        fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        fs::write(&path, body).expect("write");
    }
    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));
    assert!(
        reg.skills.contains_key("changelog"),
        "skills/changelog/ did not load; registry errors: {:?}",
        reg.errors
    );
    (dir, reg, receipt)
}

#[test]
fn config_only_skill_loads_with_every_declared_part() {
    let (_dir, reg, _) = loaded();
    let def = reg.skills.get("changelog").expect("loaded");

    assert_eq!(def.description, "Draft a release changelog from the git log");
    assert_eq!(def.agent.as_deref(), Some("scribe"));
    assert_eq!(def.template.as_deref(), Some("changelog"));

    // The argument schema, in declaration order.
    let names: Vec<&str> = def.args.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["version", "since", "focus"]);
    assert!(def.args[0].required, "version should be required");
    assert!(def.args[2].rest, "focus should swallow the rest of the line");

    // The prose body is read from skill.md, not from the TOML.
    assert!(
        def.body().expect("body").contains("Added / Changed / Fixed"),
        "skill.md was not picked up"
    );

    // The post hook survived load; the pre hook was never declared.
    assert!(def.hooks.pre.is_empty(), "unexpected pre hook");
    assert!(!def.hooks.post.is_empty(), "post hook was dropped");

    // The agent it names was itself loaded from config.
    assert!(
        reg.agents.contains_key("scribe"),
        "agents/scribe/ did not load; errors: {:?}",
        reg.errors
    );
    // And the prompt template it names is registered.
    assert!(reg.prompts.has("changelog"), "prompts/changelog.md missing");
}

#[test]
fn config_only_skill_is_invocable_as_a_command() {
    let (_dir, reg, _) = loaded();
    let table = commands::table(&reg);

    // The command dispatches, bare and by alias, and carries the skill link.
    for text in ["/changelog", "/CHANGELOG", "/changelog@mybot", "/cl"] {
        assert_eq!(
            commands::resolve(&table, text),
            Dispatch::Command {
                command: "changelog".into(),
                raw: String::new(),
            },
            "dispatch failed for {text:?}"
        );
    }
    let def = table.get("changelog").expect("in table");
    assert_eq!(def.kind, CommandKind::Skill);
    assert_eq!(def.skill.as_deref(), Some("changelog"));

    // It is advertised to Telegram like any shipped command.
    let payload = commands::my_commands_payload(&reg);
    let entries = payload["commands"].as_array().expect("commands array");
    assert!(
        entries.iter().any(|e| e["command"] == "changelog"),
        "/changelog missing from setMyCommands: {payload}"
    );
}

#[test]
fn command_invocation_binds_arguments_and_renders_the_template() {
    let (_dir, reg, _) = loaded();
    let table = commands::table(&reg);
    let Dispatch::Command { command, raw } =
        commands::resolve(&table, "/changelog 1.4.0 v1.3.0 the new export pipeline")
    else {
        panic!("did not dispatch");
    };
    assert_eq!(command, "changelog");

    let plan = skills::plan_invocation(&reg, "changelog", &raw).expect("plan");

    // Positional binding, with `focus` swallowing the remainder verbatim.
    assert_eq!(plan.args["version"], "1.4.0");
    assert_eq!(plan.args["since"], "v1.3.0");
    assert_eq!(plan.args["focus"], "the new export pipeline");

    // The template rendered with those values — not the raw string.
    assert_eq!(
        plan.user_prompt,
        "Write the changelog for 1.4.0.\n\n\
         Diff from: v1.3.0\n\
         Emphasise: the new export pipeline\n\
         Raw request: 1.4.0 v1.3.0 the new export pipeline\n"
    );

    // The skill's default agent is applied, and its env and policy come along.
    assert_eq!(plan.agent_override.as_deref(), Some("scribe"));
    assert_eq!(plan.env["CHANGELOG_STYLE"], "keepachangelog");
    assert!(plan.tools.deny.contains(&"Write".to_string()));
    assert!(!plan.tools.allow.contains(&"Write".to_string()));
}

#[test]
fn omitted_optional_arguments_fall_back_to_their_defaults() {
    let (_dir, reg, _) = loaded();
    let plan = skills::plan_invocation(&reg, "changelog", "2.0.0").expect("plan");
    assert_eq!(plan.args["version"], "2.0.0");
    assert_eq!(plan.args["since"], "the previous tag");
    assert_eq!(plan.args["focus"], "user-visible changes");
    assert!(
        plan.user_prompt.contains("Diff from: the previous tag"),
        "defaults did not reach the template:\n{}",
        plan.user_prompt
    );
}

#[test]
fn a_missing_required_argument_is_a_user_facing_error() {
    let (_dir, reg, _) = loaded();
    let err = skills::plan_invocation(&reg, "changelog", "").expect_err("should reject");
    assert!(
        err.contains("changelog") && err.contains("version"),
        "error should name the skill and the argument: {err}"
    );
}

#[test]
fn config_only_skill_is_invocable_from_another_skill() {
    let (_dir, reg, _) = loaded();
    // `/release 1.4.0` — the composing skill, which `uses` the new one.
    let plan = skills::plan_invocation(&reg, "release", "1.4.0").expect("plan");

    // Dependency-first: the used skill's prose comes BEFORE the composer's.
    let order: Vec<&str> = plan.system_fragments.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        order,
        ["changelog", "release"],
        "composition order is wrong: {order:?}"
    );
    assert!(
        plan.system_fragments[0].body.contains("Added / Changed / Fixed"),
        "the used skill's body was not composed in"
    );

    // Composition inherits the used skill's env and agent...
    assert_eq!(plan.env["CHANGELOG_STYLE"], "keepachangelog");
    assert_eq!(plan.agent_override.as_deref(), Some("scribe"));

    // ...and can only TIGHTEN the tool policy: `release` denies Bash, which
    // `changelog` allowed, so Bash ends up denied and out of the allow list.
    assert!(plan.tools.deny.contains(&"Bash".to_string()));
    assert!(!plan.tools.allow.contains(&"Bash".to_string()));
    assert!(plan.tools.allow.contains(&"Read".to_string()));

    // But arity is the COMPOSER's: `release` declares only `version`, so the
    // used skill's `since`/`focus` do not leak into the argument binding.
    assert_eq!(plan.args["version"], "1.4.0");
    assert!(
        !plan.args.contains_key("focus"),
        "the used skill's arity leaked into the composer: {:?}",
        plan.args
    );

    // `release` declares no template, so the prompt is what the user typed.
    assert_eq!(plan.user_prompt, "1.4.0");
}

#[test]
fn an_agent_listing_the_skill_gets_its_prose_without_binding_arguments() {
    let (_dir, reg, _) = loaded();
    // agents/scribe declares skills = ["changelog"], whose `version` argument
    // is required. Listing is not invoking, so this must not error.
    let frags = skills::fragments_for(&reg, "changelog").expect("fragments");
    assert_eq!(frags.len(), 1);
    assert_eq!(frags[0].name, "changelog");
    assert!(frags[0].body.contains("Added / Changed / Fixed"));
}

#[test]
fn the_post_hook_runs_with_placeholders_substituted() {
    let (_dir, reg, receipt) = loaded();
    let plan = skills::plan_invocation(&reg, "changelog", "1.4.0 v1.3.0 the export pipeline").expect("plan");

    // Substituted at PLAN time, per argv element — never re-split.
    let joined = plan.hooks.post.join(" ");
    assert!(
        !joined.contains("{{"),
        "a placeholder survived substitution: {joined}"
    );
    assert_eq!(
        plan.hooks.post.last().expect("payload"),
        "changelog/scribe/1.4.0/v1.3.0/the export pipeline",
        "hook argv: {:?}",
        plan.hooks.post
    );

    assert!(!receipt.exists(), "receipt exists before the hook ran");
    assert_eq!(skills::run_post_hooks(&plan), None, "post hook failed");
    let written = fs::read_to_string(&receipt).expect("hook did not write its receipt");
    assert_eq!(
        written.trim(),
        "changelog/scribe/1.4.0/v1.3.0/the export pipeline"
    );
}

#[test]
fn hook_arguments_are_never_re_parsed_as_shell() {
    let (_dir, reg, receipt) = loaded();
    // A `focus` argument full of shell metacharacters. Hooks are fixed argv,
    // so this must land in the receipt as literal text and must not execute.
    let canary = _dir.path().join("pwned");
    let raw = format!(
        "9.9.9 HEAD ; touch {} && echo `whoami` $(id -u)",
        canary.display()
    );
    let plan = skills::plan_invocation(&reg, "changelog", &raw).expect("plan");
    assert_eq!(skills::run_post_hooks(&plan), None, "post hook failed");

    let written = fs::read_to_string(&receipt).expect("receipt");
    assert!(
        written.contains("; touch") && written.contains("`whoami`"),
        "metacharacters were mangled instead of passed through: {written}"
    );
    assert!(
        !canary.exists(),
        "a hook argument was executed as shell — injection is possible"
    );
}
