#![cfg(all(feature = "tracker", feature = "agent", feature = "bridge"))]
// Every test here drives a verb that only exists when the module(s) named
// above are compiled in. Without the file-level gate a reduced-feature
// `cargo test` would run them against a binary that answers exit 2.
//! End-to-end tests for the runtime module gate (Layer 2).
//!
//! These spawn the BUILT `stackhour` binary in a throwaway HOME with a
//! hand-written `config.json`, because the gate's whole job is to sit between
//! argv and the dispatch match — only the real executable exercises it. The
//! compile-time layer (Layer 1) is covered separately once Cargo features
//! exist; a default build has every module compiled in, so everything here
//! reaches the runtime layer.

use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

/// The binary under test. `CARGO_BIN_EXE_*` is resolved by cargo at compile
/// time and always points at the binary built for THIS test run — the
/// hand-rolled `current_exe()`-walking form can silently reuse a stale one.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_stackhour")
}

struct Sandbox {
    home: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Sandbox {
            home: TempDir::new().unwrap(),
        }
    }

    fn config_path(&self) -> PathBuf {
        self.home
            .path()
            .join(".config")
            .join("stackhour")
            .join("config.json")
    }

    /// Write a raw config.json (creating its directory), bypassing `init` so
    /// malformed and partial documents can be pinned exactly.
    fn write_config(&self, text: &str) {
        let path = self.config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .output()
            .expect("the stackhour binary must be executable")
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("the process exited normally")
}

// ---- disabled modules refuse their verbs --------------------------------

#[test]
fn a_disabled_tracker_refuses_serve_with_exit_two() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false } }"#);
    let out = sb.run(&["serve"]);
    assert_eq!(code(&out), 2, "stderr={}", stderr(&out));
    assert!(
        stderr(&out).contains("serve needs the tracker module"),
        "stderr={}",
        stderr(&out)
    );
}

#[test]
fn a_disabled_tracker_refuses_status_token_data_backup_and_import_wakatime() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false } }"#);
    for args in [
        vec!["status"],
        vec!["token", "list"],
        vec!["data", "stats"],
        vec!["backup", "create"],
        vec!["import-wakatime"],
    ] {
        let out = sb.run(&args);
        assert_eq!(code(&out), 2, "{args:?} stderr={}", stderr(&out));
        assert!(
            stderr(&out).contains("needs the tracker module"),
            "{args:?} stderr={}",
            stderr(&out)
        );
    }
}

#[test]
fn a_disabled_agent_refuses_the_agent_verb() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "agent": false } }"#);
    for args in [vec!["agent"], vec!["agent", "--once"]] {
        let out = sb.run(&args);
        assert_eq!(code(&out), 2, "{args:?} stderr={}", stderr(&out));
        assert!(
            stderr(&out).contains("agent needs the agent module"),
            "{args:?} stderr={}",
            stderr(&out)
        );
    }
    // The other two modules are untouched by an agent-only switch-off.
    let out = sb.run(&["data", "stats"]);
    assert_ne!(code(&out), 2, "stderr={}", stderr(&out));
}

#[test]
fn a_disabled_bridge_refuses_every_bridge_subverb() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "bridge": false } }"#);
    for args in [
        vec!["bridge"],
        vec!["bridge", "status", "coordinator"],
        vec!["bridge", "migrate"],
        vec!["bridge", "return", "abc"],
    ] {
        let out = sb.run(&args);
        assert_eq!(code(&out), 2, "{args:?} stderr={}", stderr(&out));
        assert!(
            stderr(&out).contains("bridge needs the bridge module"),
            "{args:?} stderr={}",
            stderr(&out)
        );
    }
}

/// `bridge claim` and `bridge return` resolve everything they touch from
/// `--runtime-dir` and never read config.json themselves — but they are still
/// bridge verbs, so the gate refuses them. This is the coupling that forces
/// EVERY binary-spawning bridge test — `mac_worker_cli_wire_compat.rs`,
/// `bridge_targeted_claim.rs`, `session_state_parity.rs` and
/// `bridge_migrate_fixture.rs` — to spawn with `env_clear()` and a throwaway
/// HOME: with an inherited environment they would test the developer's
/// config.json instead of the runtime-dir protocol.
#[test]
fn a_disabled_bridge_refuses_the_runtime_dir_wire_verbs() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "bridge": false } }"#);
    let rt = TempDir::new().unwrap();
    let rt = rt.path().to_str().unwrap();
    for args in [
        vec!["bridge", "claim", "--runtime-dir", rt],
        vec!["bridge", "return", "abc", "--runtime-dir", rt],
    ] {
        let out = sb.run(&args);
        assert_eq!(code(&out), 2, "{args:?} stderr={}", stderr(&out));
        assert!(
            stderr(&out).contains("bridge needs the bridge module"),
            "{args:?} stderr={}",
            stderr(&out)
        );
        // Refused before dispatch: the runtime dir is left completely alone.
        assert_eq!(std::fs::read_dir(rt).unwrap().count(), 0, "{args:?}");
    }
}

#[test]
fn a_gated_verb_writes_nothing_to_stdout() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false, "agent": false, "bridge": false } }"#);
    for args in [
        vec!["serve"],
        vec!["agent"],
        vec!["bridge"],
        vec!["data", "stats"],
    ] {
        let out = sb.run(&args);
        assert_eq!(stdout(&out), "", "{args:?} leaked stdout");
    }
}

#[test]
fn a_gate_message_names_the_config_key_and_the_config_path() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false } }"#);
    let out = sb.run(&["serve"]);
    let err = stderr(&out);
    assert_eq!(
        err.trim_end(),
        format!(
            "stackhour: serve needs the tracker module, which is disabled by \"modules.tracker\": false in {}",
            sb.config_path().display()
        )
    );
    // A single greppable line, and not the compile-time message.
    assert_eq!(err.lines().count(), 1);
    assert!(!err.contains("not compiled into this binary"));
}

// ---- what the gate must NOT touch ---------------------------------------

#[test]
fn doctor_still_runs_with_every_module_disabled() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false, "agent": false, "bridge": false } }"#);
    let out = sb.run(&["doctor"]);
    assert!(
        code(&out) == 0 || code(&out) == 1,
        "doctor exited {} stderr={}",
        code(&out),
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("Stackhour doctor "),
        "stdout={}",
        stdout(&out)
    );
}

#[test]
fn the_help_path_is_unchanged_by_a_modules_block() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false } }"#);
    let out = sb.run(&[]);
    assert_eq!(code(&out), 0, "stderr={}", stderr(&out));
    assert!(stdout(&out).contains("usage: stackhour <command>\n"));
    // The module notes go to STDOUT only, so stderr is clean either way.
    assert_eq!(stderr(&out), "");
}

#[test]
fn an_all_true_modules_block_changes_nothing() {
    let sb = Sandbox::new();
    let out = sb.run(&["init", "server"]);
    assert_eq!(code(&out), 0, "stderr={}", stderr(&out));

    // Normalise the freshly-created config through serde first, so the ONLY
    // difference between the two runs below is the added `modules` key.
    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.config_path()).unwrap()).unwrap();
    std::fs::write(sb.config_path(), serde_json::to_string(&cfg).unwrap()).unwrap();
    let baseline: Vec<(i32, String, String)> = [vec!["data", "stats"], vec![]]
        .iter()
        .map(|args| {
            let out = sb.run(args);
            (code(&out), stdout(&out), stderr(&out))
        })
        .collect();

    let mut with_block = cfg;
    with_block.as_object_mut().unwrap().insert(
        "modules".to_string(),
        serde_json::json!({ "tracker": true, "agent": true, "bridge": true }),
    );
    std::fs::write(sb.config_path(), serde_json::to_string(&with_block).unwrap()).unwrap();

    for (args, expected) in [vec!["data", "stats"], vec![]].iter().zip(baseline) {
        let out = sb.run(args);
        assert_eq!(
            (code(&out), stdout(&out), stderr(&out)),
            expected,
            "{args:?} changed under an all-true modules block"
        );
    }
}

#[test]
fn a_malformed_modules_block_gates_nothing() {
    for block in ["3", "[]", "null", r#""bridge""#, "false"] {
        let sb = Sandbox::new();
        sb.write_config(&format!(r#"{{ "modules": {block} }}"#));
        for args in [vec!["data", "stats"], vec!["bridge"], vec!["agent", "--once"]] {
            let out = sb.run(&args);
            assert_ne!(
                code(&out),
                2,
                "modules:{block} {args:?} was gated; stderr={}",
                stderr(&out)
            );
            assert!(
                !stderr(&out).contains("needs the "),
                "modules:{block} {args:?} stderr={}",
                stderr(&out)
            );
        }
    }
}

// ---- the corrupt-config ordering contract --------------------------------

#[test]
fn a_corrupt_config_still_fails_the_help_path_before_any_module_message() {
    let sb = Sandbox::new();
    sb.write_config("{ this is not json");
    let out = sb.run(&[]);
    assert_eq!(code(&out), 1, "stderr={}", stderr(&out));
    assert!(stderr(&out).starts_with("stackhour: "), "stderr={}", stderr(&out));
    assert!(!stderr(&out).contains("needs the "), "stderr={}", stderr(&out));
}

#[test]
fn a_corrupt_config_lets_a_gated_verb_fail_with_todays_message() {
    let sb = Sandbox::new();
    sb.write_config("{ this is not json");
    let out = sb.run(&["serve"]);
    assert_eq!(code(&out), 1, "stderr={}", stderr(&out));
    assert!(!stderr(&out).contains("needs the "), "stderr={}", stderr(&out));
}

// ---- role-dependent verbs ------------------------------------------------

#[test]
fn init_server_is_gated_on_tracker_and_init_agent_on_agent() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false } }"#);
    let out = sb.run(&["init", "server"]);
    assert_eq!(code(&out), 2, "stderr={}", stderr(&out));
    assert!(
        stderr(&out).contains("init server needs the tracker module"),
        "stderr={}",
        stderr(&out)
    );
    // `init agent` belongs to the agent module, which is still on: it fails
    // with its own error, not the gate's.
    let out = sb.run(&["init", "agent"]);
    assert_ne!(code(&out), 2, "stderr={}", stderr(&out));
    assert!(!stderr(&out).contains("needs the "), "stderr={}", stderr(&out));

    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "agent": false } }"#);
    let out = sb.run(&["init", "agent", "--enrollment=abc"]);
    assert_eq!(code(&out), 2, "stderr={}", stderr(&out));
    assert!(
        stderr(&out).contains("init agent needs the agent module"),
        "stderr={}",
        stderr(&out)
    );
}

#[test]
fn install_server_is_gated_on_tracker_and_install_agent_on_agent() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false } }"#);
    let out = sb.run(&["install", "server"]);
    assert_eq!(code(&out), 2, "stderr={}", stderr(&out));
    assert!(
        stderr(&out).contains("install server needs the tracker module"),
        "stderr={}",
        stderr(&out)
    );

    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "agent": false } }"#);
    let out = sb.run(&["install", "agent"]);
    assert_eq!(code(&out), 2, "stderr={}", stderr(&out));
    assert!(
        stderr(&out).contains("install agent needs the agent module"),
        "stderr={}",
        stderr(&out)
    );
}

/// The gate applies to EVERY verb, so any integration test that spawns the
/// binary is implicitly a test of the developer's ambient config.json unless
/// it clears the environment. Three files were given `env_clear()` when the
/// gate landed and a fourth was missed, which is exactly the failure this
/// guards: it reads the sibling test sources and requires every spawn of the
/// stackhour binary in them to be matched by an `.env_clear()`.
///
/// DELIBERATE DIVERGENCE: this is a source-text check, not a behavioural one.
/// It cannot run the offending file under a hostile config, but it fails on
/// the machine where a missed spawner is invisible — a box whose config.json
/// enables everything.
#[test]
fn every_integration_test_that_spawns_the_binary_clears_the_environment() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("tests dir").flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // This file names both needles in prose and in the assert below, so
        // counting them here would only measure its own wording.
        if path.file_name().and_then(|n| n.to_str()) == Some("modules_gate.rs") {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap();
        // Only spawns of the stackhour binary itself — the suite also shells
        // out to `curl` and to `node`, which the gate knows nothing about.
        let spawns: usize = ["(bin())", "(BIN)", "(env!(\"CARGO_BIN_EXE_stackhour\"))"]
            .iter()
            .map(|form| body.matches(&format!("Command::new{form}")).count())
            .sum();
        if spawns == 0 {
            continue;
        }
        let cleared = body.matches("env_clear").count();
        assert!(
            cleared >= spawns,
            "{}: {spawns} stackhour spawn(s) but only {cleared} env_clear — an \
             inherited $HOME/$STACKHOUR_CONFIG lets the module gate read the \
             developer's config.json and refuse the verb under test",
            path.file_name().unwrap().to_string_lossy()
        );
        checked += 1;
    }
    assert!(
        checked >= 5,
        "only {checked} spawning test files found; did the glob break?"
    );
}

#[test]
fn a_bare_init_with_no_role_still_prints_the_usage_error() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false, "agent": false } }"#);
    let out = sb.run(&["init"]);
    assert_eq!(code(&out), 1, "stderr={}", stderr(&out));
    assert!(
        stderr(&out).contains("usage: stackhour init <server|agent>"),
        "stderr={}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("needs the "), "stderr={}", stderr(&out));
}

// ---- the surfaces: module-aware help ------------------------------------

/// The banner itself is pinned byte-for-byte, so module awareness is one
/// APPENDED note per off module rather than a filtered verb list.
#[test]
fn the_help_banner_gains_a_note_line_only_when_a_module_is_off() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false, "bridge": false } }"#);
    let out = sb.run(&[]);
    assert_eq!(code(&out), 0, "stderr={}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains(
            "note: the tracker module is disabled by \"modules.tracker\": false; \
             its commands above exit 2.\n"
        ),
        "stdout={text}"
    );
    assert!(
        text.contains(
            "note: the bridge module is disabled by \"modules.bridge\": false; \
             its commands above exit 2.\n"
        ),
        "stdout={text}"
    );
    // Only the off ones, in Module::ALL order, after the pinned banner.
    assert!(!text.contains("note: the agent module"), "stdout={text}");
    let tracker = text.find("note: the tracker module").unwrap();
    let bridge = text.find("note: the bridge module").unwrap();
    let config = text.find("config: ").unwrap();
    assert!(config < tracker && tracker < bridge, "stdout={text}");
}

/// The prime constraint at the help surface: a default build reading a config
/// with no `modules` key — and one whose block turns nothing off — must print
/// exactly what it printed before modules existed.
#[test]
fn the_help_banner_is_byte_identical_when_every_module_is_enabled() {
    let plain = Sandbox::new();
    plain.write_config("{}");
    let baseline = plain.run(&[]);
    assert_eq!(code(&baseline), 0, "stderr={}", stderr(&baseline));
    let baseline = stdout(&baseline);
    assert!(baseline.contains("usage: stackhour <command>\n"), "{baseline}");
    assert!(!baseline.contains("note: the "), "{baseline}");
    // The last line is still `config: <path>` with nothing after it.
    assert!(baseline
        .trim_end()
        .lines()
        .last()
        .unwrap()
        .starts_with("config: "));

    // An explicitly all-true block, and a JS-truthy `"false"` string, both
    // resolve to "nothing is off" and so must print the same bytes.
    for body in [
        r#"{ "modules": { "tracker": true, "agent": true, "bridge": true } }"#,
        r#"{ "modules": { "bridge": "false" } }"#,
        r#"{ "modules": null }"#,
    ] {
        let sb = Sandbox::new();
        sb.write_config(body);
        let out = sb.run(&[]);
        assert_eq!(code(&out), 0, "{body}: stderr={}", stderr(&out));
        // The config path differs per sandbox, so compare everything above it.
        let cut = |t: &str| t[..t.rfind("config: ").unwrap()].to_string();
        assert_eq!(cut(&stdout(&out)), cut(&baseline), "{body}");
        assert!(!stdout(&out).contains("note: the "), "{body}");
    }
}

#[test]
fn the_help_path_keeps_stderr_clean_with_a_module_disabled() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false, "agent": false, "bridge": false } }"#);
    let out = sb.run(&[]);
    assert_eq!(code(&out), 0, "stderr={}", stderr(&out));
    assert_eq!(stderr(&out), "", "the help path must never write to stderr");
    assert_eq!(stdout(&out).matches("note: the ").count(), 3);
}

// ---- the surfaces: module-aware doctor -----------------------------------

#[test]
fn doctor_reports_a_module_line_for_a_disabled_module() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "bridge": false } }"#);
    let out = sb.run(&["doctor"]);
    let text = stdout(&out);
    assert!(
        text.contains(&format!(
            "module-bridge: disabled by \"modules.bridge\": false in {}",
            sb.config_path().display()
        )),
        "stdout={text}"
    );
    // Ok status, so the line renders with the tick and not the cross.
    assert!(text.contains("✓ module-bridge:"), "stdout={text}");
    // Nothing is said about the modules that are still on.
    assert!(!text.contains("module-tracker"), "stdout={text}");
    assert!(!text.contains("module-agent"), "stdout={text}");
}

/// `--json` carries module state as ordinary `Check` entries, so the document
/// keeps its exact three top-level keys in their exact order and `checks[0]`
/// is still `node`.
#[test]
fn doctor_json_keeps_its_three_top_level_keys_with_a_module_disabled() {
    let sb = Sandbox::new();
    sb.write_config(r#"{ "modules": { "tracker": false, "bridge": false } }"#);
    let out = sb.run(&["doctor", "--json"]);
    let text = stdout(&out);
    let doc: serde_json::Value = serde_json::from_str(&text).expect("doctor --json is parsable");
    let keys: Vec<&str> = doc.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["ok", "version", "checks"]);
    let checks = doc["checks"].as_array().unwrap();
    assert_eq!(checks[0]["name"], "runtime");
    let module_lines: Vec<&serde_json::Value> = checks
        .iter()
        .filter(|c| c["name"].as_str().unwrap().starts_with("module-"))
        .collect();
    assert_eq!(module_lines.len(), 2, "{text}");
    for c in module_lines {
        assert_eq!(c["status"], "ok");
    }
    // `exit_success == ok` still holds: Ok-status lines cannot flip it.
    assert_eq!(doc["ok"].as_bool().unwrap(), code(&out) == 0);
}

/// doctor belongs to no module and must never be mistaken for a gated verb —
/// exit 2 is the gate's code and doctor may not produce it.
#[test]
fn doctor_exits_zero_or_one_but_never_two_with_modules_disabled() {
    for body in [
        r#"{ "modules": { "tracker": false } }"#,
        r#"{ "modules": { "agent": false } }"#,
        r#"{ "modules": { "tracker": false, "agent": false, "bridge": false } }"#,
    ] {
        let sb = Sandbox::new();
        sb.write_config(body);
        let out = sb.run(&["doctor"]);
        assert_ne!(code(&out), 2, "{body}: stderr={}", stderr(&out));
        assert!(code(&out) == 0 || code(&out) == 1, "{body}");
        assert!(stdout(&out).contains("Stackhour doctor "), "{body}");
    }
}
