//! Extensibility proof: ONE prompt template overridden by CONFIG ONLY.
//!
//! No Rust source is touched. A single `prompts/system.md` file in a config
//! directory replaces the shipped `system` template; the test then walks the
//! same chain the daemon walks:
//!
//!   loader -> agent resolution -> prompt composition -> apply_agent ->
//!   argv assembly -> spawn -> the overridden bytes observed in the child
//!
//! and, in the same registry, asserts that every OTHER template in the
//! catalogue still renders byte-for-byte identical to the shipped default.
//!
//! Two failure modes this is here to catch:
//!   * an override that does not actually reach the engine (dead config), and
//!   * an override that perturbs unrelated templates (leaky config).

use std::fs;
use std::path::Path;

use stackhour_bridge::engines::RunRequest;
use stackhour_bridge::souls;
use stackhour_core::registry::engine::ArgvVars;
use stackhour_core::registry::prompt::{builtin_names, PromptStore};
use stackhour_core::registry::{self, PromptDelivery, Registry};

/// The one template the user overrides. Deliberately unlike the shipped body
/// (`{{soul}}\n\n## Skills\n{{skills}}`) in wording, ordering AND structure, so
/// a fallback to the default cannot accidentally pass.
const SYSTEM_OVERRIDE: &str = "<<OVERRIDE-v1>>\nCAPABILITIES:\n{{skills}}\nPERSONA:\n{{soul}}\n<<END>>";

/// What that override must render to once the agent's soul and skill body are
/// substituted in. Written out longhand rather than recomputed, so the test
/// asserts an expected string instead of restating the implementation.
const SYSTEM_EXPECTED: &str =
    "<<OVERRIDE-v1>>\nCAPABILITIES:\nQuote code, don't describe it.\nPERSONA:\nLead with the verdict.\n<<END>>";

const FAKE_ENGINE_TOML: &str = r#"
label = "Fake Echo"
bin = "fake-echo"
kind = "plain-lines"
args = ["--run", "-"]
system_prompt_args = ["--system", "{{system_prompt}}"]
"#;

/// Reads the prompt on stdin and echoes argv + prompt back, so anything that
/// reached the child is observable from the test.
const FAKE_ENGINE_SH: &str = r#"#!/bin/sh
prompt=$(cat)
echo "argv:$*"
echo "prompt:$prompt"
"#;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
    fs::write(&path, body).expect("write");
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// A config tree: one config-only engine, one agent with a soul and a skill,
/// and exactly ONE prompt override.
fn fixture() -> (tempfile::TempDir, Registry, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write(root, "engines/fake-echo.toml", FAKE_ENGINE_TOML);
    write(
        root,
        "agents/reviewer/agent.toml",
        "label = \"Reviewer\"\nengine = \"fake-echo\"\nskills = [\"review\"]\n",
    );
    write(root, "agents/reviewer/soul.md", "Lead with the verdict.\n");
    write(
        root,
        "skills/review/skill.toml",
        "description = \"Review code\"\n",
    );
    write(root, "skills/review/skill.md", "Quote code, don't describe it.\n");

    // The whole extensibility claim, in one file.
    write(root, "prompts/system.md", SYSTEM_OVERRIDE);

    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir).expect("mkdir bin");
    let script = bin_dir.join("fake-echo");
    fs::write(&script, FAKE_ENGINE_SH).expect("write script");
    make_executable(&script);

    let reg = registry::load_with(root, registry::EnvSource::fixed(&[]));
    assert!(reg.errors.is_empty(), "registry errors: {:?}", reg.errors);
    (dir, reg, bin_dir)
}

/// Step 1 — the override is in force, and the store says so.
#[test]
fn the_override_replaces_only_the_named_template() {
    let (_d, reg, _bin) = fixture();
    assert!(
        reg.prompts.is_overridden("system"),
        "prompts/system.md was not picked up at all"
    );
    for name in builtin_names() {
        if *name == "system" {
            continue;
        }
        assert!(
            !reg.prompts.is_overridden(name),
            "writing prompts/system.md must not mark '{name}' as overridden"
        );
    }
}

/// Step 2 — every UNSET template still renders byte-for-byte as shipped.
///
/// Compared against a `PromptStore` with no config dir at all, i.e. the
/// zero-config bytes, over the whole catalogue in one sweep. Placeholders are
/// left unsubstituted on both sides, so the comparison covers the raw bodies.
#[test]
fn every_unset_template_falls_back_to_the_shipped_default_byte_for_byte() {
    let (_d, reg, _bin) = fixture();
    let shipped = PromptStore::new(None);

    let mut checked = 0usize;
    for name in builtin_names() {
        if *name == "system" {
            continue;
        }
        assert_eq!(
            reg.prompts.body(name),
            shipped.body(name),
            "template '{name}' drifted from the shipped default"
        );
        checked += 1;
    }
    assert!(checked > 10, "catalogue looks suspiciously small: {checked}");

    // ... and the overridden one is genuinely different, so the sweep above is
    // not passing merely because nothing ever changes.
    assert_ne!(reg.prompts.body("system"), shipped.body("system"));
    assert_eq!(reg.prompts.body("system").as_deref(), Some(SYSTEM_OVERRIDE));
}

/// Step 3 — composition uses the override, with placeholders substituted.
#[test]
fn composition_uses_the_override_with_placeholders_substituted() {
    let (_d, reg, _bin) = fixture();
    let agent = reg.agents.get("reviewer").expect("agent loaded from config");
    let system = souls::compose_system_prompt(agent, &reg).expect("compose");

    assert_eq!(system, SYSTEM_EXPECTED);
    // No placeholder survived, and none of the shipped scaffolding leaked in.
    assert!(!system.contains("{{"), "unsubstituted placeholder in: {system}");
    assert!(
        !system.contains("## Skills"),
        "the shipped `system` body leaked through the override: {system}"
    );
}

/// Step 4 — the overridden text is what `apply_agent` hands to the run
/// request, and what argv assembly splices into the child's command line.
#[test]
fn the_overridden_text_reaches_the_run_request_and_the_argv() {
    let (_d, reg, _bin) = fixture();
    let engine = reg.engines.get("fake-echo").expect("engine");

    let mut req = RunRequest {
        prompt: "review this".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(engine, reg.agents.get("reviewer"), &reg, &mut req);
    assert_eq!(req.system_prompt.as_deref(), Some(SYSTEM_EXPECTED));

    let argv = engine.assemble_argv(&ArgvVars {
        system_prompt: req.system_prompt.as_deref(),
        ..ArgvVars::default()
    });
    assert_eq!(
        argv,
        vec!["--run", "--system", SYSTEM_EXPECTED, "-"],
        "the overridden system prompt did not reach argv"
    );
}

/// Step 5 — the acceptance criterion: spawn the config-declared engine and see
/// the overridden bytes arrive in the child process.
///
/// Drives the documented spawn contract by hand (bin resolved on the target's
/// extraPath, argv from `assemble_argv`, prompt on stdin) because
/// `engines::run_engine` is still a `todo!()` scaffold. Everything upstream of
/// the spawn is the real code path.
#[test]
#[cfg(unix)]
fn the_overridden_prompt_reaches_the_spawned_engine() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let (_d, reg, bin_dir) = fixture();
    let engine = reg.engines.get("fake-echo").expect("engine");
    assert_eq!(engine.prompt_delivery, PromptDelivery::Stdin);

    let mut req = RunRequest {
        prompt: "SENTINEL-PROMPT-4417".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(engine, reg.agents.get("reviewer"), &reg, &mut req);

    let argv = engine.assemble_argv(&ArgvVars {
        system_prompt: req.system_prompt.as_deref(),
        ..ArgvVars::default()
    });

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(&engine.bin)
        .args(&argv)
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn config-declared engine");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(req.prompt.as_bytes())
        .expect("write prompt");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "engine exited badly: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);

    // The override, fully substituted, verbatim in the child's own report of
    // the argv it was handed.
    assert!(
        stdout.contains(&format!("argv:--run --system {SYSTEM_EXPECTED} -")),
        "the substituted override text is not what the engine received:\n{stdout}"
    );
    // The shipped default did NOT reach it.
    assert!(
        !stdout.contains("## Skills"),
        "the shipped `system` body reached the engine instead of the override:\n{stdout}"
    );
    assert!(
        stdout.contains("prompt:SENTINEL-PROMPT-4417"),
        "the user prompt was not delivered:\n{stdout}"
    );
}

/// Step 6 — an unset template used on the SAME turn still emits shipped bytes.
///
/// `agent-turn` is the sibling template that wraps a system prompt for engines
/// with no `system_prompt_args`. Overriding `system` must not disturb it, so
/// the same registry is asked for both on one composition.
#[test]
fn a_sibling_template_on_the_same_turn_still_emits_shipped_bytes() {
    let (_d, reg, _bin) = fixture();
    let shipped = PromptStore::new(None);

    let system =
        souls::compose_system_prompt(reg.agents.get("reviewer").expect("agent"), &reg).expect("compose");

    let ours = reg
        .prompts
        .render("agent-turn", &[("system", &system), ("prompt", "go")]);
    let theirs = shipped.render("agent-turn", &[("system", &system), ("prompt", "go")]);
    assert_eq!(ours, theirs);
    assert_eq!(ours, format!("{SYSTEM_EXPECTED}\n\n---\n\ngo"));
}
