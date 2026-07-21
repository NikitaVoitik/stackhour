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
    shell_quote, validate_coordinator_config, validate_worker_config, LAUNCHD_LABEL, SERVICE_NAME,
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
    check(out, &mut failures, node_ok, format!("Node.js >=22 ({node_version})"));

    let name = if role == "coordinator" { "config.json" } else { "worker-config.json" };
    let path = runtime_dir.join(name);
    let config: Option<Value> = match std::fs::read_to_string(&path) {
        Ok(text) => Some(serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?),
        Err(_) => None,
    };
    check(out, &mut failures, config.is_some(), format!("Config exists: {}", path.display()));
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
    // kept, so the two doctors read identically. (The Node doctor would
    // instead CRASH on a missing `targets` object; this one just fails the
    // checks.)
    let local: &Value = if role == "coordinator" {
        config.pointer("/targets/gcp").unwrap_or(&Value::Null)
    } else {
        &config
    };
    let cwd = display_str(local, "cwd");
    check(out, &mut failures, is_directory(&cwd), format!("Working directory: {cwd}"));
    let claude_bin = display_str(local, "claudeBin");
    check(
        out,
        &mut failures,
        is_executable(Path::new(&claude_bin)),
        format!("Claude Code executable: {claude_bin}"),
    );
    let codex_bin = display_str(local, "codexBin");
    check(
        out,
        &mut failures,
        is_executable(Path::new(&codex_bin)),
        format!("Codex executable: {codex_bin}"),
    );

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
        check(out, &mut failures, active, format!("User service active: {SERVICE_NAME}"));
    }
    if role == "worker" {
        let key = display_str(&config, "gcpKey");
        check(
            out,
            &mut failures,
            Path::new(&key).exists() && private_mode(Path::new(&key)),
            format!("Private SSH key: {key}"),
        );
        let gcp_ssh = display_str(&config, "gcpSsh");
        let remote_dir = display_str(&config, "remoteDir");
        let remote_check = format!(
            "test -x {} && test -f {} && test -f {}",
            shell_quote(&display_str(&config, "remoteNode")),
            shell_quote(&posix_join(&remote_dir, "claim.mjs")),
            shell_quote(&posix_join(&remote_dir, "return.mjs")),
        );
        let ssh_ok = std::process::Command::new("ssh")
            .args(["-i", &key, "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", &gcp_ssh, &remote_check])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        check(
            out,
            &mut failures,
            ssh_ok,
            format!("SSH and remote coordinator helpers: {gcp_ssh}"),
        );
        if cfg!(target_os = "macos") {
            let loaded = std::process::Command::new("launchctl")
                .args(["print", &format!("gui/{}/{LAUNCHD_LABEL}", uid())])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            check(out, &mut failures, loaded, format!("LaunchAgent loaded: {LAUNCHD_LABEL}"));
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
    } else {
        if !cfg!(target_os = "macos") {
            return Err("Worker service commands require macOS.".to_string());
        }
        let target = format!("gui/{}/{LAUNCHD_LABEL}", uid());
        if verb == "status" {
            run_cmd("launchctl", &["print", &target])
        } else {
            run_cmd("launchctl", &["kickstart", "-k", &target])
        }
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
        assert!(text.contains("✓ Config permissions exclude group/other access\n"), "{text}");
        assert!(text.contains(&format!("✓ Working directory: {}\n", rt.path().display())), "{text}");
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
        assert!(text.contains("✗ Config permissions exclude group/other access\n"), "{text}");
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
        assert!(text.contains("✗ Config validation: token is required\n"), "{text}");
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
        std::fs::write(cfg.path().join("commands").join("bad.toml"), "definitely = not [ toml").unwrap();
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
    #[test]
    fn service_commands_are_platform_gated() {
        #[cfg(target_os = "linux")]
        assert_eq!(
            service_action("worker", "status").unwrap_err(),
            "Worker service commands require macOS."
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            service_action("coordinator", "status").unwrap_err(),
            "Coordinator service commands require Linux."
        );
    }

    #[test]
    fn the_remote_probe_is_shell_quoted_and_posix_joined() {
        assert_eq!(posix_join("/srv/bridge/", "claim.mjs"), "/srv/bridge/claim.mjs");
        assert_eq!(posix_join("/srv/bridge", "return.mjs"), "/srv/bridge/return.mjs");
    }
}
