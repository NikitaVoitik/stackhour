//! Extensibility proof: a brand-new engine added by CONFIG ONLY.
//!
//! No Rust source knows the name `fake-echo`. It arrives entirely as
//! `engines/fake-echo.toml` in a config directory plus a shell script on the
//! target's extraPath. The test then walks the same chain the daemon walks:
//!
//!   loader -> engine selection -> argv assembly -> spawn -> prompt in ->
//!   streamed status out -> final text.
//!
//! If this ever fails, someone has hard-coded an engine assumption back into
//! the runner.

use std::fs;
use std::path::Path;
use std::sync::mpsc;

use stackhour_bridge::engines::{self, RunRequest};
use stackhour_bridge::souls;
use stackhour_core::registry::engine::ArgvVars;
use stackhour_core::registry::{self, PromptDelivery, Registry};

/// The engine definition, as a user would write it. Nothing here exists in
/// Rust: the name, the binary, the flags and the stream shape are all data.
const FAKE_ENGINE_TOML: &str = r#"
label = "Fake Echo"
emoji = "🧪"
bin = "fake-echo"
kind = "plain-lines"
args = ["--run", "-"]
model_args = ["--model", "{{model}}"]
system_prompt_args = ["--system", "{{system_prompt}}"]
"#;

/// A shell script standing in for a real coding engine. It reads the prompt on
/// stdin and emits status lines the `plain-lines` parser accumulates.
const FAKE_ENGINE_SH: &str = r#"#!/bin/sh
prompt=$(cat)
echo "FAKE-ECHO-BOOTED"
echo "argv:$*"
echo "prompt:$prompt"
echo "FAKE-ECHO-DONE"
"#;

/// Write the config tree + the fake engine binary. Returns (tempdir, registry,
/// bin dir) — the tempdir must outlive both.
/// A target's `extraPath` REPLACES the child's whole PATH (coordinator.mjs
/// spawns with `PATH: tgt.extraPath || process.env.PATH`), so a target that
/// wants the system tools has to list them — as the real config does. The
/// fake engine is a shell script that shells out, so it needs them.
fn target_path(bin_dir: &std::path::Path) -> String {
    match std::env::var("PATH") {
        Ok(p) if !p.is_empty() => format!("{}:{p}", bin_dir.display()),
        _ => bin_dir.display().to_string(),
    }
}

fn fixture() -> (tempfile::TempDir, Registry, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "engines/fake-echo.toml", FAKE_ENGINE_TOML);

    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("mkdir bin");
    let script = bin_dir.join("fake-echo");
    fs::write(&script, FAKE_ENGINE_SH).expect("write script");
    make_executable(&script);

    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));
    (dir, reg, bin_dir)
}

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

/// Step 1 — the loader accepts an engine it has never heard of, with no
/// errors, and every declared field survives the round trip.
#[test]
fn a_config_only_engine_loads_and_is_selectable() {
    let (_d, reg, _bin) = fixture();
    assert!(reg.errors.is_empty(), "registry errors: {:?}", reg.errors);

    let engine = reg
        .engines
        .get("fake-echo")
        .expect("an engine defined purely in config must be selectable by name");
    assert_eq!(engine.label, "Fake Echo");
    assert_eq!(engine.bin, "fake-echo");
    assert_eq!(engine.args, vec!["--run", "-"]);

    // The built-ins are still there alongside it.
    assert!(reg.engines.contains_key("claude"));
    assert!(reg.engines.contains_key("codex"));
}

/// Step 2 — argv assembly is driven by the config, not by a match on the
/// engine name: the declared flags splice before the `-` stdin sentinel.
#[test]
fn argv_is_assembled_from_the_config_alone() {
    let (_d, reg, _bin) = fixture();
    let engine = reg.engines.get("fake-echo").expect("engine");

    let argv = engine.assemble_argv(&ArgvVars {
        model: Some("tiny-1"),
        ..ArgvVars::default()
    });
    assert_eq!(
        argv,
        vec!["--run", "--model", "tiny-1", "-"],
        "argv must come from the TOML template, spliced before the stdin sentinel"
    );
}

/// Step 2b — the bridge's own `build_argv` must agree with the core reference
/// implementation it is documented to delegate to.
#[test]
fn the_runner_assembles_argv_from_the_config_alone() {
    let (_d, reg, _bin) = fixture();
    let engine = reg.engines.get("fake-echo").expect("engine");

    let req = RunRequest {
        prompt: "hello engine".into(),
        model: Some("tiny-1".into()),
        ..RunRequest::default()
    };
    let argv = engines::build_argv(engine, &req);
    assert_eq!(
        argv,
        vec!["--run", "--model", "tiny-1", "-"],
        "argv must come from the TOML template, spliced before the stdin sentinel"
    );
}

/// Step 3 — the whole point: spawn the config-declared binary, hand it the
/// prompt, and read its streamed status back.
///
/// This is THE acceptance criterion for config-only engine extensibility: the
/// runner reaches an engine it has never been compiled against, purely through
/// the `EngineDef` the loader built from TOML.
#[test]
fn the_bridge_spawns_the_config_only_engine_and_streams_its_status_back() {
    let (_d, reg, bin_dir) = fixture();
    let engine = reg.engines.get("fake-echo").expect("engine");

    let (tx, rx) = mpsc::channel();
    let req = RunRequest {
        prompt: "SENTINEL-PROMPT-7391".into(),
        model: Some("tiny-1".into()),
        extra_path: Some(target_path(&bin_dir)),
        ..RunRequest::default()
    };
    let result = engines::run_engine(engine, req, Some(tx));

    assert_eq!(result.code, Some(0), "engine exited badly: {result:?}");
    assert!(
        result.text.contains("FAKE-ECHO-BOOTED"),
        "the engine's output never reached the runner:\n{}",
        result.text
    );
    assert!(
        result.text.contains("prompt:SENTINEL-PROMPT-7391"),
        "the prompt was not delivered on stdin:\n{}",
        result.text
    );
    assert!(
        result.text.contains("argv:--run --model tiny-1"),
        "the config-declared flags did not reach the child:\n{}",
        result.text
    );

    // Status must have STREAMED, not just arrived at the end.
    let streamed: Vec<String> = rx.iter().collect();
    assert!(
        streamed.iter().any(|l| l.contains("FAKE-ECHO-BOOTED")),
        "no activity was streamed while the engine ran: {streamed:?}"
    );
}

/// Step 3b — proof that the CONFIG is sufficient to spawn, independent of the
/// bridge runner: drive the same spawn contract (`bin` resolved on the target's
/// extraPath, argv from `assemble_argv`, prompt on stdin, `plain-lines` status
/// accumulated as it arrives) by hand.
///
/// Step 3 asserts the same thing THROUGH the runner. This one keeps the
/// fixture honest: if step 3 passes and this fails, the runner is compensating
/// for a config format that cannot actually describe a spawnable engine.
#[test]
#[cfg(unix)]
fn the_config_alone_describes_a_spawnable_streaming_engine() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let (_d, reg, bin_dir) = fixture();
    let engine = reg.engines.get("fake-echo").expect("engine");
    assert_eq!(engine.prompt_delivery, PromptDelivery::Stdin);

    let argv = engine.assemble_argv(&ArgvVars {
        model: Some("tiny-1"),
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
        .expect("the config's `bin` must resolve on the target extraPath");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"SENTINEL-PROMPT-7391")
        .expect("write prompt");

    // Accumulate as it arrives — the `plain-lines` stream contract.
    let mut streamed: Vec<String> = Vec::new();
    for line in BufReader::new(child.stdout.take().expect("stdout")).lines() {
        streamed.push(line.expect("read line"));
    }
    let code = child.wait().expect("wait").code();

    assert_eq!(code, Some(0));
    assert_eq!(streamed.first().map(String::as_str), Some("FAKE-ECHO-BOOTED"));
    assert!(
        streamed.iter().any(|l| l == "argv:--run --model tiny-1 -"),
        "the config-declared flags did not reach the child: {streamed:?}"
    );
    assert!(
        streamed.iter().any(|l| l == "prompt:SENTINEL-PROMPT-7391"),
        "the prompt was not delivered on stdin: {streamed:?}"
    );
    assert_eq!(streamed.last().map(String::as_str), Some("FAKE-ECHO-DONE"));
}

/// Step 4 — a config-only engine composes with the other pillars: an agent
/// bound to it gets its soul delivered through the flag the ENGINE declared.
#[test]
fn an_agent_can_bind_to_the_config_only_engine() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "engines/fake-echo.toml", FAKE_ENGINE_TOML);
    write(
        dir.path(),
        "agents/tester/agent.toml",
        "label = \"Tester\"\nengine = \"fake-echo\"\nmodel = \"tiny-1\"\n",
    );
    write(dir.path(), "agents/tester/soul.md", "You are a fake.\n");
    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));
    assert!(reg.errors.is_empty(), "registry errors: {:?}", reg.errors);

    let agent = reg.agents.get("tester").expect("agent");
    assert_eq!(agent.engine, "fake-echo");

    let engine = reg.engines.get("fake-echo").expect("engine").clone();
    let mut req = RunRequest {
        prompt: "do the thing".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(&engine, Some(agent), &reg, &mut req);
    assert_eq!(req.model.as_deref(), Some("tiny-1"));
    assert!(
        req.system_prompt
            .as_deref()
            .is_some_and(|s| s.contains("You are a fake.")),
        "the soul did not reach the engine's declared system-prompt flag"
    );
}

/// Step 5 — the resume-retry rule is engine-agnostic too. A config-only engine
/// that fails a resume with no output gets exactly ONE fresh (session-less)
/// rerun, driven by the `resume` style it declared in TOML.
#[test]
#[cfg(unix)]
fn a_config_only_engine_gets_the_resume_retry_rule_for_free() {
    // Fails whenever `--resume` is on its argv, succeeds otherwise.
    const FLAKY_SH: &str = r#"#!/bin/sh
cat >/dev/null
case "$*" in
  *--resume*) echo "stale session" >&2; exit 7 ;;
esac
echo "FRESH-RUN-OK"
"#;
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "engines/flaky.toml",
        "bin = \"flaky\"\nkind = \"plain-lines\"\nargs = [\"-\"]\n[resume]\nflag = [\"--resume\", \"{{session_id}}\"]\n",
    );
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("mkdir bin");
    let script = bin_dir.join("flaky");
    fs::write(&script, FLAKY_SH).expect("write script");
    make_executable(&script);

    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));
    assert!(reg.errors.is_empty(), "registry errors: {:?}", reg.errors);
    let engine = reg.engines.get("flaky").expect("engine");

    let req = RunRequest {
        prompt: "hi".into(),
        session_id: Some("dead-session".into()),
        extra_path: Some(target_path(&bin_dir)),
        ..RunRequest::default()
    };

    // Without the rule, the resume failure is what the user would see.
    let once = engines::run_engine(engine, req.clone(), None);
    assert_eq!(once.code, Some(7), "the resume attempt should have failed");
    assert!(once.text.is_empty());

    // With it, the fresh rerun's output is what comes back.
    let retried = engines::run_with_resume_retry(engine, req, None, None);
    assert_eq!(retried.code, Some(0), "the fresh rerun should succeed");
    assert!(retried.retried_fresh, "the retry flag must be set");
    assert_eq!(retried.text, "FRESH-RUN-OK");
}

/// Step 6 — an agent's `effort` reaches a config-only engine through the
/// `effort_args` template that engine declared, and is silently ignored by
/// engines that declare none. Both halves are config, not code.
#[test]
#[cfg(unix)]
fn effort_reaches_a_config_only_engine_that_declares_effort_args() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "engines/thinky.toml",
        "bin = \"fake-echo\"\nkind = \"plain-lines\"\nargs = [\"-\"]\neffort_args = [\"--effort\", \"{{effort}}\"]\n",
    );
    write(dir.path(), "engines/dumb.toml", FAKE_ENGINE_TOML);
    write(
        dir.path(),
        "agents/deep/agent.toml",
        "engine = \"thinky\"\neffort = \"high\"\n",
    );
    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));
    assert!(reg.errors.is_empty(), "registry errors: {:?}", reg.errors);
    let agent = reg.agents.get("deep").expect("agent");

    let thinky = reg.engines.get("thinky").expect("engine").clone();
    let mut req = RunRequest {
        prompt: "think".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(&thinky, Some(agent), &reg, &mut req);
    assert_eq!(
        engines::build_argv(&thinky, &req),
        vec!["--effort", "high", "-"],
        "the agent's effort must splice through the engine's declared template"
    );

    // The same agent on an engine with no `effort_args` is unaffected.
    let dumb = reg.engines.get("dumb").expect("engine").clone();
    let mut req = RunRequest {
        prompt: "think".into(),
        ..RunRequest::default()
    };
    souls::apply_agent(&dumb, Some(agent), &reg, &mut req);
    assert_eq!(req.effort, None);
    assert!(!engines::build_argv(&dumb, &req).contains(&"--effort".to_string()));
}

/// Step 7 — `/stop` works against a config-only engine: the runner hands back
/// a terminate handle that SIGTERMs whatever the config told it to spawn.
#[test]
#[cfg(unix)]
fn a_config_only_engine_can_be_stopped_through_the_running_job_handle() {
    // `exec` so the process we spawn IS the long sleep: otherwise the shell
    // dies on SIGTERM but its orphaned child keeps our stdout pipe open.
    const SLEEPER_SH: &str = "#!/bin/sh\ncat >/dev/null\necho STARTED\nexec sleep 60\n";
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "engines/sleeper.toml",
        "bin = \"sleeper\"\nkind = \"plain-lines\"\nargs = [\"-\"]\n",
    );
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("mkdir bin");
    let script = bin_dir.join("sleeper");
    fs::write(&script, SLEEPER_SH).expect("write script");
    make_executable(&script);

    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));
    let engine = reg.engines.get("sleeper").expect("engine");

    let (tx, rx) = mpsc::channel();
    let (job, handle) = engines::spawn_engine(
        engine,
        RunRequest {
            prompt: "go".into(),
            extra_path: Some(target_path(&bin_dir)),
            ..RunRequest::default()
        },
        Some(tx),
    );

    // Wait until the child is demonstrably running, then stop it.
    let first = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the engine never started");
    assert_eq!(first, "STARTED");
    job.terminate();

    let result = handle.join().expect("runner thread");
    assert_ne!(result.code, Some(0), "the child should not have run to completion");
    // Terminating an already-reaped job is a no-op, not a panic.
    job.terminate();
}
