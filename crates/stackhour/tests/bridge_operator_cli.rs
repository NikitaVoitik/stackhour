#![cfg(feature = "bridge")]
// Every test here drives a verb that only exists when the bridge module is
// compiled in, and the file names `stackhour_bridge` directly —
// so without this gate a reduced build fails to COMPILE, which is the
// likeliest way a feature break lands looking green.
//! The bridge operator CLI (`bridge install|doctor|status|restart`) driven
//! through the BUILT binary, like tests/cli.rs: these verbs previously fell
//! into the "not implemented in the Rust port yet" catch-all even though the
//! help text advertised them, and only the real executable can prove the
//! dispatch is wired.
//!
//! SAFETY: every run gets `env_clear()` + a throwaway HOME and an explicit
//! `--runtime-dir` in a tempdir, so nothing here can touch the owner's live
//! `~/.claude-remote/` queue or user services. The service steps (systemctl)
//! are expected to fail in the sandbox — the assertions stop at the files the
//! installer writes before them.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_stackhour");

struct Sandbox {
    home: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Sandbox {
            home: TempDir::new().unwrap(),
        }
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", std::env::var("PATH").unwrap_or_default());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("the stackhour binary must be executable")
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A 0755 stand-in for claude/codex.
fn fake_bin(dir: &Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    p.display().to_string()
}

/// `-h`/`--help` print the Node usage banner on stdout and exit 0.
#[test]
fn bridge_help_prints_the_usage_banner_and_exits_zero() {
    let sb = Sandbox::new();
    for args in [
        vec!["bridge", "install", "--help"],
        vec!["bridge", "doctor", "-h"],
        vec!["bridge", "-h"],
    ] {
        let out = sb.run(&args, &[]);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        let text = stdout(&out);
        assert!(
            text.starts_with("stackhour bridge — install and operate the Telegram Claude + Codex bridge\n"),
            "{args:?}: {text}"
        );
        assert!(
            text.contains("stackhour bridge install <coordinator|worker>"),
            "{text}"
        );
        assert_eq!(stderr(&out), "", "help must keep stderr clean");
    }
}

/// A missing/unknown command or role prints the same banner on STDERR and
/// exits 1 — Node's `usage(1)`.
#[test]
fn an_unknown_command_or_role_prints_usage_to_stderr_and_exits_one() {
    let sb = Sandbox::new();
    for args in [
        vec!["bridge"],
        vec!["bridge", "install"],
        vec!["bridge", "install", "frobnicate"],
        vec!["bridge", "frobnicate", "coordinator"],
    ] {
        let out = sb.run(&args, &[]);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            stderr(&out).starts_with("stackhour bridge — install and operate"),
            "{args:?}: {}",
            stderr(&out)
        );
        assert_eq!(stdout(&out), "", "{args:?}: usage(1) must not print to stdout");
    }
}

/// Non-interactive install must fail with the Node error text — and BEFORE
/// any write: the runtime dir stays empty.
#[test]
fn non_interactive_install_fails_before_any_write_when_env_is_missing() {
    let sb = Sandbox::new();
    let rt = sb.home.path().join("rt");
    let rt_str = rt.display().to_string();

    let out = sb.run(
        &[
            "bridge",
            "install",
            "coordinator",
            "--non-interactive",
            "--runtime-dir",
            &rt_str,
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("Error: TELEGRAM_BOT_TOKEN is required for non-interactive setup."),
        "{}",
        stderr(&out)
    );
    assert!(!rt.exists(), "a refused install must write nothing");

    let out = sb.run(
        &[
            "bridge",
            "install",
            "coordinator",
            "--non-interactive",
            "--runtime-dir",
            &rt_str,
        ],
        &[("TELEGRAM_BOT_TOKEN", "tok")],
    );
    assert!(
        stderr(&out).contains(
            "Error: Authorized Telegram chat ID is required (set TELEGRAM_CHAT_ID for non-interactive setup)."
        ),
        "{}",
        stderr(&out)
    );
    assert!(!rt.exists());
}

/// The full non-interactive coordinator install on Linux. `systemctl --user`
/// has no session bus in the sandbox, so the run may exit 1 at the
/// daemon-reload step — but by then the config, runtime and unit file must
/// all be on disk, which is what this test pins.
#[cfg(target_os = "linux")]
#[test]
fn coordinator_install_writes_config_runtime_and_unit() {
    let sb = Sandbox::new();
    let rt = sb.home.path().join("rt");
    let rt_str = rt.display().to_string();
    let bins = sb.home.path().join("bins");
    std::fs::create_dir(&bins).unwrap();
    let claude = fake_bin(&bins, "claude");
    let codex = fake_bin(&bins, "codex");
    let work = sb.home.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let env: Vec<(&str, &str)> = vec![
        ("TELEGRAM_BOT_TOKEN", "tok-abc"),
        ("TELEGRAM_CHAT_ID", "-100123"),
        ("BRIDGE_WORKDIR", work.to_str().unwrap()),
        ("CLAUDE_BIN", &claude),
        ("CODEX_BIN", &codex),
    ];

    let out = sb.run(
        &[
            "bridge",
            "install",
            "coordinator",
            "--non-interactive",
            "--no-start",
            "--runtime-dir",
            &rt_str,
        ],
        &env,
    );
    // Exit 0 with a session bus, exit 1 at daemon-reload without one; either
    // way it must not panic and must not hit the old catch-all.
    assert_ne!(out.status.code(), Some(101), "panicked: {}", stderr(&out));
    assert!(!stderr(&out).contains("not implemented in the Rust port yet"));
    if out.status.code() == Some(1) {
        assert!(
            stderr(&out).contains("systemctl"),
            "unexpected failure: {}",
            stderr(&out)
        );
    }

    // Config: 0600, the Node key set, the answers we fed in.
    let cfg_path = rt.join("config.json");
    let cfg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
    assert_eq!(cfg["token"], "tok-abc");
    assert_eq!(cfg["chatId"], -100123);
    assert_eq!(cfg["defaultTarget"], "gcp");
    assert_eq!(cfg["targets"]["gcp"]["claudeBin"], claude);
    assert_eq!(cfg["targets"]["mac"]["type"], "remote");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config holds the bot token");
    }

    // Runtime: the copied binary plus the three coordinator shims, all 0755.
    for f in ["stackhour", "claim.mjs", "return.mjs", "tg-send.mjs"] {
        let p = rt.join(f);
        assert!(p.is_file(), "missing {f}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o755,
                "{f}"
            );
        }
    }

    // Unit: written before the systemctl step, ExecStart = the INSTALLED
    // binary with the coordinator verb — never node, never a .mjs file.
    let unit_path = sb
        .home
        .path()
        .join(".config/systemd/user/stackhour-bridge.service");
    let unit = std::fs::read_to_string(&unit_path).expect("unit file written");
    assert!(
        unit.contains(&format!(
            "ExecStart=\"{}\" bridge coordinator",
            rt.join("stackhour").display()
        )),
        "{unit}"
    );
    assert!(!unit.contains("coordinator.mjs"), "{unit}");

    // Re-running reuses the config rather than reprompting.
    let out = sb.run(
        &[
            "bridge",
            "install",
            "coordinator",
            "--non-interactive",
            "--no-start",
            "--runtime-dir",
            &rt_str,
        ],
        &[],
    );
    assert!(
        stdout(&out).contains(&format!(
            "Reusing {}; pass --reconfigure to replace it.",
            cfg_path.display()
        )),
        "{}",
        stdout(&out)
    );

    // ...and doctor now sees the installed runtime.
    let out = sb.run(
        &["bridge", "doctor", "coordinator", "--runtime-dir", &rt_str],
        &env,
    );
    let text = stdout(&out);
    for line in [
        "✓ Config exists: ",
        "✓ Config validation\n",
        "✓ Config permissions exclude group/other access\n",
        "✓ Installed stackhour\n",
        "✓ Installed claim.mjs\n",
        "✓ Installed return.mjs\n",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    assert!(text.contains("Node.js >=22 ("), "{text}");
}

/// A leader-only non-interactive install: no engine env at all, zero local
/// targets in the written config (which still passes the strict validator),
/// and doctor prints the leader-only line.
#[cfg(target_os = "linux")]
#[test]
fn leader_only_coordinator_install_writes_a_workerless_roster() {
    let sb = Sandbox::new();
    let rt = sb.home.path().join("rt");
    let rt_str = rt.display().to_string();
    let env: Vec<(&str, &str)> = vec![
        ("TELEGRAM_BOT_TOKEN", "tok"),
        ("TELEGRAM_CHAT_ID", "7"),
        ("BRIDGE_COORDINATOR_ROLE", "leader-only"),
    ];

    let out = sb.run(
        &[
            "bridge",
            "install",
            "coordinator",
            "--non-interactive",
            "--no-start",
            "--runtime-dir",
            &rt_str,
        ],
        &env,
    );
    assert_ne!(out.status.code(), Some(101), "panicked: {}", stderr(&out));
    if out.status.code() == Some(1) {
        assert!(
            stderr(&out).contains("systemctl"),
            "unexpected failure: {}",
            stderr(&out)
        );
    }

    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(rt.join("config.json")).unwrap()).unwrap();
    assert!(
        stackhour_bridge::config::validate_coordinator_config(&cfg).is_empty(),
        "strict validator must accept the leader-only config: {cfg}"
    );
    assert_eq!(cfg["defaultTarget"], "mac");
    let targets = cfg["targets"].as_object().unwrap();
    assert_eq!(targets.len(), 1);
    assert!(
        targets.values().all(|t| t["type"] == "remote"),
        "zero local targets: {cfg}"
    );
    assert!(
        cfg["targets"]["mac"].get("cwd").is_none(),
        "no engine keys anywhere"
    );

    // The unit is still written (the leader runs the bot + queue), with no
    // engine PATH to export.
    let unit = std::fs::read_to_string(
        sb.home
            .path()
            .join(".config/systemd/user/stackhour-bridge.service"),
    )
    .expect("unit file written");
    assert!(unit.contains("bridge coordinator"), "{unit}");
    assert!(
        unit.contains("Environment=\"PATH=\""),
        "leader-only exports no engine PATH: {unit}"
    );

    let out = sb.run(
        &["bridge", "doctor", "coordinator", "--runtime-dir", &rt_str],
        &env,
    );
    let text = stdout(&out);
    assert!(
        text.contains("✓ leader-only coordinator (no local engines)"),
        "{text}"
    );
    assert!(!text.contains("Working directory"), "{text}");
}

/// The Linux worker install: no macOS refusal, leader* config spellings and
/// a claim target, plus a systemd user unit that runs `bridge worker`.
#[cfg(target_os = "linux")]
#[test]
fn worker_install_on_linux_writes_config_and_a_systemd_unit() {
    let sb = Sandbox::new();
    let rt = sb.home.path().join("rt");
    let rt_str = rt.display().to_string();
    let bins = sb.home.path().join("bins");
    std::fs::create_dir(&bins).unwrap();
    let claude = fake_bin(&bins, "claude");
    let codex = fake_bin(&bins, "codex");
    let work = sb.home.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let key = sb.home.path().join("id_test");
    std::fs::write(&key, "KEY").unwrap();
    let key = key.display().to_string();
    let env: Vec<(&str, &str)> = vec![
        ("BRIDGE_LEADER_SSH", "user@leader.example"),
        ("BRIDGE_LEADER_KEY", &key),
        ("BRIDGE_TARGET", "attic"),
        ("BRIDGE_WORKDIR", work.to_str().unwrap()),
        ("CLAUDE_BIN", &claude),
        ("CODEX_BIN", &codex),
    ];

    let out = sb.run(
        &[
            "bridge",
            "install",
            "worker",
            "--non-interactive",
            "--no-start",
            "--runtime-dir",
            &rt_str,
        ],
        &env,
    );
    assert_ne!(out.status.code(), Some(101), "panicked: {}", stderr(&out));
    assert!(
        !stderr(&out).contains("The worker installer currently targets macOS."),
        "{}",
        stderr(&out)
    );
    if out.status.code() == Some(1) {
        assert!(
            stderr(&out).contains("systemctl"),
            "unexpected failure: {}",
            stderr(&out)
        );
    }

    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(rt.join("worker-config.json")).unwrap()).unwrap();
    assert!(
        stackhour_bridge::config::validate_worker_config(&cfg).is_empty(),
        "{cfg}"
    );
    assert_eq!(cfg["leaderSsh"], "user@leader.example");
    assert_eq!(cfg["leaderKey"], key);
    assert_eq!(cfg["target"], "attic");
    assert!(
        cfg.get("gcpSsh").is_none(),
        "new configs use the leader spellings: {cfg}"
    );

    // Runtime: the binary + the worker-side tg-send shim only.
    assert!(rt.join("stackhour").is_file());
    assert!(rt.join("tg-send.mjs").is_file());
    assert!(
        !rt.join("claim.mjs").exists(),
        "claim/return are coordinator-side"
    );

    // The systemd user unit execs the installed binary with `bridge worker`.
    let unit = std::fs::read_to_string(
        sb.home
            .path()
            .join(".config/systemd/user/stackhour-bridge-worker.service"),
    )
    .expect("worker unit written");
    assert!(
        unit.contains(&format!(
            "ExecStart=\"{}\" bridge worker",
            rt.join("stackhour").display()
        )),
        "{unit}"
    );
}

/// The claim/return shims the installer writes must be REAL node scripts that
/// drive the installed binary — this is the wire contract the Node Mac worker
/// depends on (`<remoteNode> <remoteDir>/claim.mjs` over SSH).
#[cfg(target_os = "linux")]
#[test]
fn the_installed_shims_speak_the_job_protocol_under_node() {
    let Some(node) = find_node() else {
        eprintln!("node not on PATH; skipping the shim round trip");
        return;
    };
    let sb = Sandbox::new();
    let rt = sb.home.path().join("rt");
    let rt_str = rt.display().to_string();
    let bins = sb.home.path().join("bins");
    std::fs::create_dir(&bins).unwrap();
    let claude = fake_bin(&bins, "claude");
    let codex = fake_bin(&bins, "codex");
    let out = sb.run(
        &[
            "bridge",
            "install",
            "coordinator",
            "--non-interactive",
            "--no-start",
            "--runtime-dir",
            &rt_str,
        ],
        &[
            ("TELEGRAM_BOT_TOKEN", "tok"),
            ("TELEGRAM_CHAT_ID", "1"),
            ("BRIDGE_WORKDIR", "/"),
            ("CLAUDE_BIN", &claude),
            ("CODEX_BIN", &codex),
        ],
    );
    assert!(
        rt.join("claim.mjs").is_file(),
        "install did not write shims: {}",
        stderr(&out)
    );

    // A job in the exact dispatch shape, claimed through `node claim.mjs`.
    const ID: &str = "45f2db13-ee19-4487-96e3-5a1467041246";
    std::fs::create_dir_all(rt.join("jobs")).unwrap();
    let body = format!("{{\"id\":\"{ID}\",\"prompt\":\"hi\",\"engine\":\"codex\",\"media\":null,\"sessionId\":null,\"ts\":1}}");
    std::fs::write(rt.join("jobs").join(format!("{ID}.json")), &body).unwrap();

    let out = Command::new(&node)
        .arg(rt.join("claim.mjs"))
        .env_clear()
        .env("HOME", sb.home.path())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("run node claim.mjs");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), body, "claim must print the job verbatim");
    assert!(rt.join("inprogress").join(format!("{ID}.json")).exists());

    // ...and returned through `node return.mjs <id>` with the result on stdin.
    let mut child = Command::new(&node)
        .arg(rt.join("return.mjs"))
        .arg(ID)
        .env_clear()
        .env("HOME", sb.home.path())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run node return.mjs");
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(format!("{{\"id\":\"{ID}\",\"text\":\"4\",\"code\":0}}").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(rt.join("results").join(format!("{ID}.json")).exists());
    assert!(!rt.join("inprogress").join(format!("{ID}.json")).exists());
}

/// `bridge doctor` on an empty runtime reports and exits 1 — it must never
/// hit the old "not implemented" stub or panic.
#[test]
fn bridge_doctor_reports_a_missing_config_and_exits_one() {
    let sb = Sandbox::new();
    let rt = sb.home.path().join("rt");
    std::fs::create_dir(&rt).unwrap();
    let out = sb.run(
        &[
            "bridge",
            "doctor",
            "coordinator",
            "--runtime-dir",
            &rt.display().to_string(),
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("✗ Config exists: "), "{text}");
    assert!(!stderr(&out).contains("not implemented in the Rust port yet"));
}

/// `bridge status`/`restart` dispatch to the service layer. In the sandbox
/// there is no user service, so they fail — through the Node `\nError:` path,
/// never a panic or the unimplemented stub.
#[test]
fn bridge_status_and_restart_are_dispatched() {
    let sb = Sandbox::new();
    for verb in ["status", "restart"] {
        let out = sb.run(&["bridge", verb, "coordinator"], &[]);
        assert_ne!(out.status.code(), Some(101), "{verb} panicked: {}", stderr(&out));
        assert!(
            !stderr(&out).contains("not implemented in the Rust port yet"),
            "{verb}"
        );
        if !out.status.success() {
            assert!(stderr(&out).contains("Error: "), "{verb}: {}", stderr(&out));
        }
        // DELIBERATE DIVERGENCE: the worker role is no longer macOS-only —
        // on Linux it drives the systemd user unit. In the sandbox there is
        // no user manager, so it fails through the clean error path.
        #[cfg(target_os = "linux")]
        {
            let out = sb.run(&["bridge", verb, "worker"], &[]);
            assert_ne!(out.status.code(), Some(101), "{verb} panicked: {}", stderr(&out));
            assert!(
                !stderr(&out).contains("Worker service commands require macOS."),
                "{verb}: {}",
                stderr(&out)
            );
            if !out.status.success() {
                assert!(stderr(&out).contains("Error: "), "{verb}: {}", stderr(&out));
            }
        }
    }
}

/// `bridge tg-send` reaches the ported tgsend runner: with no config anywhere
/// it exits 2 with tg-send's own error — proof the dispatch is wired.
#[test]
fn bridge_tg_send_is_dispatched_to_the_ported_runner() {
    let sb = Sandbox::new();
    let out = sb.run(&["bridge", "tg-send", "hello"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.starts_with("tg-send: cannot read config at "), "{err}");
    assert!(!err.contains("not implemented in the Rust port yet"));
}

/// An unknown flag exits 1 through the clean error line (Node died on an
/// uncaught parseArgs throw with the same exit code).
#[test]
fn an_unknown_flag_is_a_clean_error() {
    let sb = Sandbox::new();
    let out = sb.run(&["bridge", "install", "coordinator", "--frobnicate"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("Error: Unknown option '--frobnicate'"),
        "{}",
        stderr(&out)
    );
}

fn find_node() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("node"))
        .find(|c| c.is_file())
}
