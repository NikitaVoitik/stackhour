//! End-to-end tests that spawn the BUILT `stackhour` binary.
//!
//! Why this file exists: the bin crate previously contributed zero tests, so
//! a `main()` that was literally `todo!()` still reported green under
//! `cargo test --workspace`. Unit tests inside the crate cannot catch an
//! unwired dispatch table — only running the real executable can. Every test
//! here therefore drives the shipped binary in a throwaway HOME.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// The binary under test, as built by cargo for this integration target.
fn bin() -> PathBuf {
    // target/<profile>/deps/cli-<hash> -> target/<profile>/stackhour
    let mut path = std::env::current_exe().expect("test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("stackhour")
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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .output()
            .expect("the stackhour binary must be executable")
    }

    fn config(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.config_path()).unwrap()).unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The headline regression: `init server` must create a real config file at
/// the documented path with the documented shape. A `todo!()` main would
/// exit 101 here and leave HOME empty.
#[test]
fn init_server_creates_the_config_at_the_documented_path() {
    let sb = Sandbox::new();
    let root = sb.home.path().join("code");
    std::fs::create_dir(&root).unwrap();

    let out = sb.run(&[
        "init",
        "server",
        "--public-url=https://stack.example.com",
        &format!("--project-root={}", root.display()),
    ]);
    assert!(
        out.status.success(),
        "exit={:?} stderr={}",
        out.status.code(),
        stderr(&out)
    );

    let cfg_path = sb.config_path();
    assert!(cfg_path.exists(), "config was not created at {cfg_path:?}");
    let cfg = sb.config();
    assert_eq!(cfg["server"]["host"], "0.0.0.0");
    assert_eq!(cfg["server"]["port"], 4040);
    assert_eq!(cfg["server"]["publicUrl"], "https://stack.example.com");
    // The `tokens` map, never the legacy scalar `token` key.
    assert!(cfg["server"]["tokens"].is_object());
    assert!(cfg["server"].get("token").is_none());
    // The db path lands under the sandbox HOME, not the real one.
    let db = cfg["server"]["db"].as_str().unwrap();
    assert!(
        db.starts_with(sb.home.path().to_str().unwrap()),
        "db escaped the sandbox: {db}"
    );
    // A server also gets a co-located agent, with the root canonicalized.
    assert_eq!(
        cfg["agent"]["projectRoots"][0].as_str().unwrap(),
        root.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(cfg["agent"]["serverUrl"], "http://127.0.0.1:4040");

    let text = stdout(&out);
    assert!(text.starts_with(&format!("Created server config at {}\n", cfg_path.display())));
    assert!(text.contains("Public URL: https://stack.example.com\n"));
    assert!(text.ends_with("Next: ./bin/stackhour token create <machine>\n"));
    // The generated secret is never echoed.
    let secret = cfg["server"]["tokens"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .as_str()
        .unwrap();
    assert!(!text.contains(secret), "init printed the ingest token");
}

/// The config carries a machine secret, so it must land 0600.
#[test]
fn init_server_writes_the_config_with_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    assert!(sb.run(&["init", "server"]).status.success());
    let mode = std::fs::metadata(sb.config_path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "config must not be group/world readable");
}

#[test]
fn init_server_refuses_to_clobber_without_force() {
    let sb = Sandbox::new();
    assert!(sb.run(&["init", "server"]).status.success());
    let first = sb.config();

    let out = sb.run(&["init", "server"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "stackhour init: server config already exists; pass --force to replace it\n"
    );
    assert_eq!(sb.config(), first, "a refused init must not touch the file");

    assert!(sb.run(&["init", "server", "--force"]).status.success());
}

/// A rejected argument must leave no file behind at all.
#[test]
fn a_rejected_init_creates_no_config() {
    let sb = Sandbox::new();
    let out = sb.run(&["init", "server", "--port=99999"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "stackhour init: port must be an integer from 1 to 65535\n"
    );
    assert!(!sb.config_path().exists());
}

#[test]
fn init_with_an_unknown_role_prints_usage_and_exits_one() {
    let sb = Sandbox::new();
    let out = sb.run(&["init", "frobnicate"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "stackhour init: usage: stackhour init <server|agent> [options]\n"
    );
}

/// `token create` -> `init agent --enrollment=<code>` is the documented
/// two-machine setup flow; it has to work against the real binary.
#[test]
fn token_create_emits_an_enrollment_code_a_second_machine_can_consume() {
    let server = Sandbox::new();
    // `--machine=srv` pins the server's own enrolment name so the assertions
    // below do not depend on this box's hostname.
    assert!(server
        .run(&[
            "init",
            "server",
            "--machine=srv",
            "--public-url=http://server.test:4040"
        ])
        .status
        .success());

    let out = server.run(&["token", "create", "laptop"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("Enrolled laptop. On that machine run:\n\n"));
    let code = text
        .split("--enrollment=")
        .nth(1)
        .expect("an enrollment code")
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    // The token is now listed, without ever printing a secret.
    // `init server` already enrolled the server itself, so both names list.
    let listed = server.run(&["token", "list"]);
    assert!(listed.status.success());
    assert_eq!(stdout(&listed), "laptop\nsrv\n");

    // A DIFFERENT machine enrolls with that code.
    let agent = Sandbox::new();
    let out = agent.run(&["init", "agent", &format!("--enrollment={code}")]);
    assert!(out.status.success(), "{}", stderr(&out));
    let cfg = agent.config();
    assert_eq!(cfg["agent"]["machine"], "laptop");
    assert_eq!(cfg["agent"]["serverUrl"], "http://server.test:4040");
    assert!(cfg.get("server").is_none(), "an agent gets no server section");
    assert!(stdout(&out).ends_with("Next: ./bin/stackhour doctor\n"));

    // And can be revoked on the server.
    let out = server.run(&["token", "revoke", "laptop"]);
    assert_eq!(stdout(&out), "Revoked token for laptop\n");
    assert_eq!(stdout(&server.run(&["token", "list"])), "srv\n");
}

#[test]
fn token_without_a_subcommand_is_a_usage_error() {
    let sb = Sandbox::new();
    let out = sb.run(&["token"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "stackhour token: usage: stackhour token <create MACHINE|revoke MACHINE|list>\n"
    );
}

/// `doctor` must survive a completely empty HOME: it reports rather than
/// crashes, and a missing config is a WARNING (`!`), not an error.
#[test]
fn doctor_runs_on_an_empty_home_and_reports_warnings_not_errors() {
    let sb = Sandbox::new();
    let out = sb.run(&["doctor"]);
    let text = stdout(&out);
    assert!(text.starts_with("Stackhour doctor "), "got: {text}");
    assert!(text.contains("! config: not found: "));
    assert!(text.contains("! token: no ingest token configured\n"));
    assert!(text.contains("! project-roots: none configured"));
    // The trailing summary line is `\n<n> errors, <m> warnings\n`.
    let summary = text.trim_end().lines().last().unwrap();
    let (errors, warnings) = summary
        .split_once(" errors, ")
        .expect("a `<n> errors, <m> warnings` summary");
    assert!(errors.parse::<u32>().is_ok(), "got: {summary}");
    assert!(warnings.ends_with(" warnings"), "got: {summary}");
    assert!(warnings.trim_end_matches(" warnings").parse::<u32>().unwrap() > 0);
}

#[test]
fn doctor_json_emits_a_parsable_report() {
    let sb = Sandbox::new();
    let out = sb.run(&["doctor", "--json"]);
    let report: Value = serde_json::from_str(&stdout(&out)).expect("--json must emit valid JSON");
    assert!(report["checks"].is_array());
    assert!(report["ok"].is_boolean());
    assert_eq!(report["checks"][0]["name"], "node");
    // The exit code follows `ok`.
    assert_eq!(out.status.success(), report["ok"].as_bool().unwrap());
}

/// Any unrecognised verb (including none) prints the usage banner on stdout
/// and exits 0, matching Node's `default:` case.
#[test]
fn no_args_prints_the_usage_banner_and_exits_zero() {
    let sb = Sandbox::new();
    for args in [vec![], vec!["--help"], vec!["totally-unknown"]] {
        let out = sb.run(&args);
        assert!(
            out.status.success(),
            "{args:?} should exit 0, got {:?}",
            out.status.code()
        );
        let text = stdout(&out);
        assert!(text.starts_with("stackhour — self-hosted coding time tracker\n"));
        assert!(text.contains("usage: stackhour <command>\n"));
        assert!(text
            .trim_end()
            .ends_with(&format!("config: {}", sb.config_path().display())));
        assert_eq!(stderr(&out), "", "the help path must keep stderr clean");
    }
}

/// Every verb the CLI advertises must at least be DISPATCHED — none may
/// panic with 'not yet implemented'. This is the guard against a `todo!()`
/// creeping back into any arm of main's match.
#[test]
fn no_advertised_verb_panics() {
    let sb = Sandbox::new();
    assert!(sb.run(&["init", "server"]).status.success());
    let verbs: &[&[&str]] = &[
        &["doctor"],
        &["doctor", "--json"],
        &["init"],
        &["token", "list"],
        &["data", "stats"],
        &["backup", "verify", "/nonexistent"],
        &["install"],
        &["status"],
        // `--once` is mandatory here: a bare `agent` is a daemon and would
        // never return.
        &["agent", "--once"],
        &["import-wakatime"],
        &["bridge"],
        &[],
    ];
    for args in verbs {
        let out = sb.run(args);
        // 101 is the Rust panic exit code; `todo!()` and `unimplemented!()`
        // both land there.
        assert_ne!(
            out.status.code(),
            Some(101),
            "`stackhour {}` panicked: {}",
            args.join(" "),
            stderr(&out)
        );
        assert!(
            !stderr(&out).contains("not yet implemented"),
            "`stackhour {}` hit a todo!(): {}",
            args.join(" "),
            stderr(&out)
        );
    }
}

/// Parity contract: `loadConfig()` runs before the switch, so a corrupt
/// config.json fails even the help path rather than printing usage.
#[test]
fn a_corrupt_config_fails_the_help_path() {
    let sb = Sandbox::new();
    let cfg = sb.config_path();
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, "{ not json at all").unwrap();

    let out = sb.run(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).is_empty(), "usage must not print");
    assert!(stderr(&out).starts_with("stackhour: "));

    // ...but `doctor` deliberately survives it, reporting the breakage.
    let out = sb.run(&["doctor"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("✗ config: cannot load "));
}

/// `STACKHOUR_CONFIG` relocates the config file; `init` must honour it.
#[test]
fn stackhour_config_env_var_relocates_the_config() {
    let sb = Sandbox::new();
    let alt = sb.home.path().join("elsewhere").join("cfg.json");
    let out = Command::new(bin())
        .args(["init", "server"])
        .env_clear()
        .env("HOME", sb.home.path())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STACKHOUR_CONFIG", &alt)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(alt.exists(), "config did not follow STACKHOUR_CONFIG");
    assert!(!sb.config_path().exists());
}

/// Sanity: the tests above are meaningless if they ran against a stale or
/// missing binary.
#[test]
fn the_binary_under_test_exists() {
    assert!(
        Path::new(&bin()).exists(),
        "expected a built binary at {:?}",
        bin()
    );
}

/// Regression: `import-wakatime` must reach the ported importer rather than
/// the "not implemented in the Rust port yet" stub. With no API key anywhere
/// the importer is the only thing that can produce this message — Node's
/// `importWakatime` throws exactly the same string.
#[test]
fn import_wakatime_is_dispatched_to_the_ported_importer() {
    let sb = Sandbox::new();
    assert!(sb.run(&["init", "server"]).status.success());
    let out = sb.run(&["import-wakatime"]);
    let err = stderr(&out);
    assert!(
        err.contains("no wakatime.apiKey in config and no WAKATIME_API_KEY set"),
        "expected the importer's own error, got: {err}"
    );
    assert!(
        !err.contains("not implemented in the Rust port yet"),
        "import-wakatime is still routed to the unimplemented stub"
    );
}
