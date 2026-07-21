//! Bridge doctor / status / restart, split from installer.rs (line budget).
//!
//! Per-role checks with ✓/✗ output and exit code: the runtime analogue of
//! the node>=22 check (check name kept), config validity + mode, binaries
//! executable, runtime files present, systemd is-active / launchctl print,
//! ssh remote-helper probe using shell_quote. restart = the exact
//! systemctl/launchctl sequences. NEW: registry validation errors surfaced
//! as additional ✗/! lines AFTER the existing checks.

use serde_json::Value;
use std::io::Write;
use std::path::Path;

use crate::config::{
    self, shell_quote, validate_coordinator_config, validate_worker_config, LAUNCHD_LABEL, SERVICE_NAME,
    WORKER_SERVICE_NAME,
};
use crate::installer::{run_cmd, uid};

/// Run `stackhour bridge doctor <role>`; returns the exit code.
pub fn run_doctor(role: &str, runtime_dir: &Path) -> i32 {
    let config_dir = stackhour_core::paths::resolve_storage_paths_from_process_env().config_dir;
    let mut out = std::io::stdout();
    match doctor_checks(role, runtime_dir, &config_dir, &mut out) {
        Ok(code) => code,
        // The shared runBridgeCli catch: a config that exists but does not
        // parse dies here, exactly as JSON.parse threw in the Node loadConfig.
        Err(message) => {
            eprintln!("\nError: {message}");
            1
        }
    }
}

/// The check list itself, with the output stream and registry root injected
/// so tests can drive it against temp dirs and read every line back.
pub fn doctor_checks(
    role: &str,
    runtime_dir: &Path,
    config_dir: &Path,
    out: &mut dyn Write,
) -> Result<i32, String> {
    let mut failures: Vec<String> = Vec::new();

    // PARITY: the JS doctor checks its own interpreter (`Node.js >=22
    // (${process.version})`). The Rust binary carries no Node, but the
    // claim.mjs/return.mjs shims in the runtime dir are node scripts (a Node
    // Mac worker runs them over SSH), so the check survives as a PATH probe,
    // name kept.
    let (node_ok, node_version) = node_version_probe();
    check(
        out,
        &mut failures,
        node_ok,
        format!("Node.js >=22 ({node_version})"),
    );

    let name = if role == "coordinator" {
        "config.json"
    } else {
        "worker-config.json"
    };
    let path = runtime_dir.join(name);
    let config: Option<Value> = match std::fs::read_to_string(&path) {
        Ok(text) => Some(serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?),
        Err(_) => None,
    };
    check(
        out,
        &mut failures,
        config.is_some(),
        format!("Config exists: {}", path.display()),
    );
    // `if (!config) return 1` — no summary line, straight out.
    let Some(config) = config else { return Ok(1) };

    let errors = if role == "coordinator" {
        validate_coordinator_config(&config)
    } else {
        validate_worker_config(&config)
    };
    check(
        out,
        &mut failures,
        errors.is_empty(),
        if errors.is_empty() {
            "Config validation".to_string()
        } else {
            format!("Config validation: {}", errors.join("; "))
        },
    );
    check(
        out,
        &mut failures,
        private_mode(&path),
        "Config permissions exclude group/other access".to_string(),
    );

    // PARITY: on a missing key the Node template literal prints `undefined`;
    // kept, so the two doctors read identically. DELIBERATE DIVERGENCE from
    // the Node doctor, which only ever inspected the hardcoded `targets.gcp`
    // (and would CRASH on a missing `targets` object): the coordinator now
    // audits EVERY `type == "local"` target — the single-local form keeps
    // the Node's exact lines, extra locals suffix their target name — and a
    // leader-only roster (zero local targets) reports itself instead of
    // failing.
    if role == "coordinator" {
        let locals: Vec<(&String, &Value)> = config
            .get("targets")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(name, t)| config::target_kind(name, t) == "local")
            .collect();
        if locals.is_empty() {
            check(
                out,
                &mut failures,
                true,
                "leader-only coordinator (no local engines)".to_string(),
            );
        } else {
            for (name, t) in &locals {
                let suffix = if locals.len() > 1 {
                    format!(" ({name})")
                } else {
                    String::new()
                };
                local_engine_checks(out, &mut failures, t, &suffix);
            }
        }
    } else {
        local_engine_checks(out, &mut failures, &config, "");
    }

    // PARITY (deliberate divergence): the JS checks coordinator.mjs /
    // claim.mjs / return.mjs and worker.mjs. The Rust install replaces
    // coordinator.mjs/worker.mjs with the copied binary, so `stackhour`
    // stands in for the daemon file; the claim/return shims keep their names
    // because the Node worker still calls them by name over SSH.
    let files: &[&str] = if role == "coordinator" {
        &["stackhour", "claim.mjs", "return.mjs"]
    } else {
        &["stackhour"]
    };
    for file in files {
        check(
            out,
            &mut failures,
            is_executable(&runtime_dir.join(file)),
            format!("Installed {file}"),
        );
    }

    if role == "coordinator" && cfg!(target_os = "linux") {
        let active = std::process::Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", SERVICE_NAME])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        check(
            out,
            &mut failures,
            active,
            format!("User service active: {SERVICE_NAME}"),
        );
    }
    if role == "coordinator" {
        // NEW vs the Node doctor: per-target worker liveness off the
        // heartbeat files ([`crate::jobs::worker_alive_for`] — the targeted
        // `worker-heartbeat-<target>` first, legacy shared `worker-heartbeat`
        // fallback). One line per worker target; the single-worker form
        // keeps the familiar "<label> worker: online|offline" wording.
        let paths = crate::BridgePaths::from_runtime_dir(runtime_dir);
        let workers: Vec<(&String, &Value)> = config
            .get("targets")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(name, t)| config::target_kind(name, t) != "local")
            .collect();
        let solo = workers.len() == 1;
        for (name, t) in workers {
            let alive = crate::jobs::worker_alive_for(&paths, name);
            let state = if alive { "online" } else { "offline" };
            let message = if solo {
                let label = t
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(name);
                format!("{label} worker: {state}")
            } else {
                format!("Worker {name}: {state}")
            };
            check(out, &mut failures, alive, message);
        }
    }
    if role == "worker" {
        // The leader keys accept either spelling (leader* preferred, the
        // legacy gcp* fallback), exactly as the loader reads them.
        let key = aliased_display(&config, "leaderKey", "gcpKey");
        check(
            out,
            &mut failures,
            Path::new(&key).exists() && private_mode(Path::new(&key)),
            format!("Private SSH key: {key}"),
        );
        let leader_ssh = aliased_display(&config, "leaderSsh", "gcpSsh");
        let remote_dir = display_str(&config, "remoteDir");
        let remote_check = format!(
            "test -x {} && test -f {} && test -f {}",
            shell_quote(&display_str(&config, "remoteNode")),
            shell_quote(&posix_join(&remote_dir, "claim.mjs")),
            shell_quote(&posix_join(&remote_dir, "return.mjs")),
        );
        let ssh_ok = std::process::Command::new("ssh")
            .args([
                "-i",
                &key,
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                &leader_ssh,
                &remote_check,
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        check(
            out,
            &mut failures,
            ssh_ok,
            format!("SSH and remote coordinator helpers: {leader_ssh}"),
        );
        // NEW: the name this worker claims jobs under, when configured
        // (absent = the legacy claim-anything mode).
        if let Some(target) = config
            .get("target")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            check(out, &mut failures, true, format!("Claim target: {target}"));
        }
        // DELIBERATE DIVERGENCE: the Node doctor only knew the macOS
        // LaunchAgent; a Linux worker checks its systemd user unit.
        if cfg!(target_os = "macos") {
            let loaded = std::process::Command::new("launchctl")
                .args(["print", &format!("gui/{}/{LAUNCHD_LABEL}", uid())])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            check(
                out,
                &mut failures,
                loaded,
                format!("LaunchAgent loaded: {LAUNCHD_LABEL}"),
            );
        } else if cfg!(target_os = "linux") {
            let active = std::process::Command::new("systemctl")
                .args(["--user", "is-active", "--quiet", WORKER_SERVICE_NAME])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            check(
                out,
                &mut failures,
                active,
                format!("User service active: {WORKER_SERVICE_NAME}"),
            );
        }
    }

    // NEW vs the Node doctor: the Rust coordinator also reads the config
    // registry (commands/agents/engines/prompts under <config-dir>), so its
    // validation errors surface here too — one ✗ line each, after the parity
    // checks, in the RegistryError one-line form.
    if role == "coordinator" {
        let registry = stackhour_core::registry::load(config_dir);
        for err in &registry.errors {
            check(out, &mut failures, false, format!("registry: {err}"));
        }
    }

    if failures.is_empty() {
        let _ = writeln!(out, "\n✓ Ready.");
        Ok(0)
    } else {
        let _ = writeln!(out, "\n{} check(s) failed.", failures.len());
        Ok(1)
    }
}

/// Run `stackhour bridge status|restart <role>` (the exact systemctl /
/// launchctl sequences); returns the exit code.
pub fn run_service_cmd(role: &str, verb: &str) -> i32 {
    match service_action(role, verb) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("\nError: {message}");
            1
        }
    }
}

/// `serviceAction` — stdio is inherited, so systemctl/launchctl output IS
/// the status report.
fn service_action(role: &str, verb: &str) -> Result<(), String> {
    if role == "coordinator" {
        if !cfg!(target_os = "linux") {
            return Err("Coordinator service commands require Linux.".to_string());
        }
        run_cmd("systemctl", &["--user", verb, SERVICE_NAME])
    } else if cfg!(target_os = "macos") {
        let target = format!("gui/{}/{LAUNCHD_LABEL}", uid());
        if verb == "status" {
            run_cmd("launchctl", &["print", &target])
        } else {
            run_cmd("launchctl", &["kickstart", "-k", &target])
        }
    } else if cfg!(target_os = "linux") {
        // DELIBERATE DIVERGENCE: the Node CLI threw 'Worker service commands
        // require macOS.' — Linux workers now drive their systemd user unit.
        run_cmd("systemctl", &["--user", verb, WORKER_SERVICE_NAME])
    } else {
        Err("Worker service commands require Linux or macOS.".to_string())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// One ✓/✗ line; a failed condition is remembered for the summary.
fn check(out: &mut dyn Write, failures: &mut Vec<String>, cond: bool, message: String) {
    let _ = writeln!(out, "{} {message}", if cond { '✓' } else { '✗' });
    if !cond {
        failures.push(message);
    }
}

/// The three local-engine lines (cwd / claudeBin / codexBin), shared by the
/// worker's own config and each of the coordinator's local targets. `suffix`
/// is `" (<target>)"` when more than one local target needs telling apart.
fn local_engine_checks(out: &mut dyn Write, failures: &mut Vec<String>, local: &Value, suffix: &str) {
    let cwd = display_str(local, "cwd");
    check(
        out,
        failures,
        is_directory(&cwd),
        format!("Working directory{suffix}: {cwd}"),
    );
    let claude_bin = display_str(local, "claudeBin");
    check(
        out,
        failures,
        is_executable(Path::new(&claude_bin)),
        format!("Claude Code executable{suffix}: {claude_bin}"),
    );
    let codex_bin = display_str(local, "codexBin");
    check(
        out,
        failures,
        is_executable(Path::new(&codex_bin)),
        format!("Codex executable{suffix}: {codex_bin}"),
    );
}

/// A string field with a preferred/legacy spelling pair — `undefined` when
/// neither is present, as the JS interpolation printed.
fn aliased_display(v: &Value, preferred: &str, legacy: &str) -> String {
    config::leader_aliased(v, preferred, legacy).unwrap_or_else(|| "undefined".to_string())
}

/// `node --version` → (major >= 22, "v22.x.y" | "not found").
fn node_version_probe() -> (bool, String) {
    let Ok(out) = std::process::Command::new("node").arg("--version").output() else {
        return (false, "not found".into());
    };
    if !out.status.success() {
        return (false, "not found".into());
    }
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let major = version
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    (major >= 22, version)
}

/// A string field for display — `undefined` when absent, as JS interpolates.
fn display_str(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("undefined")
        .to_string()
}

/// `(statSync(path).mode & 0o077) === 0`, false on any stat error.
fn private_mode(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o077 == 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}

fn is_directory(path: &str) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// `accessSync(path, X_OK)` + regular file, the same probe config.rs uses.
fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `join(remoteDir, file)` for the REMOTE (always-POSIX) side.
fn posix_join(dir: &str, file: &str) -> String {
    format!("{}/{file}", dir.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn write_config(dir: &Path, name: &str, v: &Value, mode: u32) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, serde_json::to_string(v).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
        p
    }

    fn installed_file(dir: &Path, name: &str) {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn coordinator_config(cwd: &Path) -> Value {
        json!({
            "token": "t",
            "chatId": 1,
            "defaultTarget": "gcp",
            "targets": {
                "gcp": {
                    "cwd": cwd.display().to_string(),
                    "claudeBin": "/bin/sh",
                    "codexBin": "/bin/sh",
                    "permissionMode": "default"
                },
                "mac": {}
            }
        })
    }

    fn run(role: &str, rt: &Path, cfg_dir: &Path) -> (i32, String) {
        let mut out: Vec<u8> = Vec::new();
        let code = doctor_checks(role, rt, cfg_dir, &mut out).expect("no throw");
        (code, String::from_utf8(out).unwrap())
    }

    /// `if (!config) return 1` — the summary line is deliberately absent.
    #[test]
    fn a_missing_config_fails_fast_without_a_summary() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let (code, text) = run("coordinator", rt.path(), cfg.path());
        assert_eq!(code, 1);
        assert!(text.contains("✗ Config exists: "), "{text}");
        assert!(text.contains("Node.js >=22 ("), "{text}");
        assert!(!text.contains("check(s) failed."), "{text}");
        assert!(!text.contains("Ready."), "{text}");
    }

    /// A config that exists but does not parse throws, like JSON.parse did.
    #[test]
    fn a_corrupt_config_is_an_error_not_a_check_line() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        std::fs::write(rt.path().join("config.json"), "{ nope").unwrap();
        let mut out: Vec<u8> = Vec::new();
        assert!(doctor_checks("coordinator", rt.path(), cfg.path(), &mut out).is_err());
    }

    #[test]
    fn a_healthy_coordinator_runtime_passes_the_file_checks() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        write_config(rt.path(), "config.json", &coordinator_config(rt.path()), 0o600);
        for f in ["stackhour", "claim.mjs", "return.mjs"] {
            installed_file(rt.path(), f);
        }
        let (_, text) = run("coordinator", rt.path(), cfg.path());
        assert!(text.contains("✓ Config validation\n"), "{text}");
        assert!(
            text.contains("✓ Config permissions exclude group/other access\n"),
            "{text}"
        );
        assert!(
            text.contains(&format!("✓ Working directory: {}\n", rt.path().display())),
            "{text}"
        );
        assert!(text.contains("✓ Claude Code executable: /bin/sh\n"), "{text}");
        assert!(text.contains("✓ Installed stackhour\n"), "{text}");
        assert!(text.contains("✓ Installed claim.mjs\n"), "{text}");
        assert!(text.contains("✓ Installed return.mjs\n"), "{text}");
        // The summary is always the last line, in one of its two exact forms.
        let last = text.trim_end().lines().last().unwrap();
        assert!(last == "✓ Ready." || last.ends_with("check(s) failed."), "{last}");
    }

    /// A group-readable config is the classic install mistake.
    #[cfg(unix)]
    #[test]
    fn loose_config_permissions_fail_the_doctor() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        write_config(rt.path(), "config.json", &coordinator_config(rt.path()), 0o644);
        let (code, text) = run("coordinator", rt.path(), cfg.path());
        assert_eq!(code, 1);
        assert!(
            text.contains("✗ Config permissions exclude group/other access\n"),
            "{text}"
        );
        assert!(text.contains("check(s) failed.\n"), "{text}");
    }

    /// Strict-validation problems land on the check line, semicolon-joined.
    #[test]
    fn validation_errors_are_printed_on_the_check_line() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let mut bad = coordinator_config(rt.path());
        bad.as_object_mut().unwrap().remove("token");
        write_config(rt.path(), "config.json", &bad, 0o600);
        let (code, text) = run("coordinator", rt.path(), cfg.path());
        assert_eq!(code, 1);
        assert!(
            text.contains("✗ Config validation: token is required\n"),
            "{text}"
        );
    }

    /// Missing keys print as `undefined`, the JS interpolation, not a panic.
    #[test]
    fn missing_paths_read_as_undefined_like_the_node_doctor() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let mut v = coordinator_config(rt.path());
        v["targets"]["gcp"].as_object_mut().unwrap().remove("cwd");
        write_config(rt.path(), "config.json", &v, 0o600);
        let (_, text) = run("coordinator", rt.path(), cfg.path());
        assert!(text.contains("✗ Working directory: undefined\n"), "{text}");
    }

    /// The NEW check: registry errors surface as ✗ lines after the parity
    /// checks and count as failures.
    #[test]
    fn registry_errors_surface_as_failed_checks() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        write_config(rt.path(), "config.json", &coordinator_config(rt.path()), 0o600);
        std::fs::create_dir_all(cfg.path().join("commands")).unwrap();
        std::fs::write(
            cfg.path().join("commands").join("bad.toml"),
            "definitely = not [ toml",
        )
        .unwrap();
        let (code, text) = run("coordinator", rt.path(), cfg.path());
        assert_eq!(code, 1);
        assert!(text.contains("✗ registry: "), "{text}");
        assert!(text.contains("bad.toml"), "{text}");

        // ...and only for the coordinator: the worker never reads the
        // registry, so a broken config dir must not fail its doctor.
        write_config(
            rt.path(),
            "worker-config.json",
            &json!({
                "gcpSsh": "u@h", "gcpKey": "/k", "remoteDir": "/d", "remoteNode": "/n",
                "claudeBin": "/bin/sh", "codexBin": "/bin/sh", "cwd": "/"
            }),
            0o600,
        );
        // (The worker doctor's ssh probe runs against u@h and fails fast in
        // BatchMode; we only assert the registry line is absent.)
        let (_, text) = run("worker", rt.path(), cfg.path());
        assert!(!text.contains("✗ registry: "), "{text}");
    }

    /// status/restart on the wrong platform is the exact Node error.
    /// DELIBERATE DIVERGENCE: `worker status` on Linux is no longer an error
    /// — it drives the systemd user unit — so the old "Worker service
    /// commands require macOS." assertion is gone.
    #[test]
    fn service_commands_are_platform_gated() {
        #[cfg(target_os = "macos")]
        assert_eq!(
            service_action("coordinator", "status").unwrap_err(),
            "Coordinator service commands require Linux."
        );
    }

    /// A decimal-ms heartbeat file, `age_secs` in the past.
    fn write_heartbeat(path: &Path, age_secs: u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        std::fs::write(path, format!("{}", now - age_secs * 1000)).unwrap();
    }

    /// Zero local targets is not a failure — it is the leader-only topology,
    /// and the doctor says so on its own ✓ line instead of ✗ing three
    /// `undefined` engine checks.
    #[test]
    fn a_leader_only_config_prints_the_leader_only_line() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let leader = json!({
            "token": "t", "chatId": 1, "defaultTarget": "pi",
            "targets": { "pi": { "label": "Pi" } }
        });
        write_config(rt.path(), "config.json", &leader, 0o600);
        for f in ["stackhour", "claim.mjs", "return.mjs"] {
            installed_file(rt.path(), f);
        }
        // A fresh targeted heartbeat: the (single) worker line reads online
        // in the familiar "<label> worker" wording.
        write_heartbeat(&rt.path().join("worker-heartbeat-pi"), 5);
        let (_, text) = run("coordinator", rt.path(), cfg.path());
        assert!(text.contains("✓ Config validation\n"), "{text}");
        assert!(
            text.contains("✓ leader-only coordinator (no local engines)\n"),
            "{text}"
        );
        assert!(!text.contains("Working directory"), "{text}");
        assert!(!text.contains("Claude Code executable"), "{text}");
        assert!(text.contains("✓ Pi worker: online\n"), "{text}");
    }

    /// Two local targets get two engine-check blocks, each line naming its
    /// target; the single-local form (above tests) keeps the Node wording.
    #[test]
    fn two_local_targets_get_two_engine_check_blocks() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let dir = rt.path().display().to_string();
        let v = json!({
            "token": "t", "chatId": 1, "defaultTarget": "hetzner",
            "targets": {
                "hetzner": { "type": "local", "cwd": dir, "claudeBin": "/bin/sh", "codexBin": "/bin/sh" },
                "attic": { "type": "local", "cwd": "/definitely/not/here", "claudeBin": "/bin/sh", "codexBin": "/bin/sh" },
            }
        });
        write_config(rt.path(), "config.json", &v, 0o600);
        let (code, text) = run("coordinator", rt.path(), cfg.path());
        assert_eq!(code, 1, "attic's cwd is missing");
        assert!(
            text.contains(&format!("✓ Working directory (hetzner): {dir}\n")),
            "{text}"
        );
        assert!(
            text.contains("✗ Working directory (attic): /definitely/not/here\n"),
            "{text}"
        );
        assert!(
            text.contains("✓ Claude Code executable (hetzner): /bin/sh\n"),
            "{text}"
        );
        assert!(text.contains("✓ Codex executable (attic): /bin/sh\n"), "{text}");
        // No worker targets, no worker lines.
        assert!(!text.contains("worker:"), "{text}");
    }

    /// Per-target worker liveness: each worker target reads its OWN
    /// heartbeat file, and with several workers each gets its own line.
    #[test]
    fn worker_liveness_lines_are_per_target() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let mut v = coordinator_config(rt.path());
        v["targets"]["pi"] = json!({});
        write_config(rt.path(), "config.json", &v, 0o600);
        write_heartbeat(&rt.path().join("worker-heartbeat-pi"), 5);
        // mac has no targeted heartbeat and no legacy fallback: offline.
        let (_, text) = run("coordinator", rt.path(), cfg.path());
        assert!(text.contains("✗ Worker mac: offline\n"), "{text}");
        assert!(text.contains("✓ Worker pi: online\n"), "{text}");

        // Exactly one worker: the familiar single-worker wording, satisfied
        // by the LEGACY shared heartbeat a no-arg `claim` still writes.
        let rt = tempfile::tempdir().unwrap();
        write_config(rt.path(), "config.json", &coordinator_config(rt.path()), 0o600);
        write_heartbeat(&rt.path().join("worker-heartbeat"), 5);
        let (_, text) = run("coordinator", rt.path(), cfg.path());
        assert!(text.contains("✓ mac worker: online\n"), "{text}");
    }

    /// A worker config written with the preferred leader* spellings and a
    /// claim target: the alias pair is accepted and the target is mentioned.
    #[test]
    fn a_worker_config_with_leader_spellings_and_a_target_reads_cleanly() {
        let rt = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let key = rt.path().join("id_test");
        std::fs::write(&key, "KEY").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let key = key.display().to_string();
        write_config(
            rt.path(),
            "worker-config.json",
            &json!({
                "leaderSsh": "u@h", "leaderKey": key, "remoteDir": "/d", "remoteNode": "/n",
                "target": "attic", "claudeBin": "/bin/sh", "codexBin": "/bin/sh", "cwd": "/"
            }),
            0o600,
        );
        installed_file(rt.path(), "stackhour");
        // (The ssh probe against u@h fails fast in BatchMode; the lines we
        // pin are the alias-driven ones.)
        let (_, text) = run("worker", rt.path(), cfg.path());
        assert!(text.contains("✓ Config validation\n"), "{text}");
        assert!(text.contains(&format!("✓ Private SSH key: {key}\n")), "{text}");
        assert!(
            text.contains("SSH and remote coordinator helpers: u@h\n"),
            "{text}"
        );
        assert!(text.contains("✓ Claim target: attic\n"), "{text}");
    }

    #[test]
    fn the_remote_probe_is_shell_quoted_and_posix_joined() {
        assert_eq!(posix_join("/srv/bridge/", "claim.mjs"), "/srv/bridge/claim.mjs");
        assert_eq!(posix_join("/srv/bridge", "return.mjs"), "/srv/bridge/return.mjs");
    }
}
