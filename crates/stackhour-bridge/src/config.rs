//! Coordinator / worker config load and the setup-lib helpers.
//!
//! Typed views over a raw preserved `serde_json::Value` (unknown keys
//! survive rewrites). Runtime-LOOSE validation (exact throw strings) vs
//! installer-STRICT validators (exact error string lists). Unit-tested
//! against bridge-setup-lib.test.mjs expectations (tests-fixtures/).

use indexmap::IndexMap;
use serde_json::Value;
use stackhour_core::Result;
use std::path::{Path, PathBuf};

/// One coordinator `targets` entry.
#[derive(Debug, Clone)]
pub struct TargetCfg {
    pub label: String,
    /// `local` | `remote`.
    pub kind: String,
    pub cwd: Option<String>,
    pub claude_bin: Option<String>,
    pub codex_bin: Option<String>,
    pub extra_path: Option<String>,
    pub permission_mode: Option<String>,
    pub model: Option<String>,
    pub codex_model: Option<String>,
}

/// `<runtime_dir>/config.json` — the coordinator config.
#[derive(Debug, Clone)]
pub struct CoordinatorCfg {
    /// Raw file contents, key order + unknown keys preserved.
    pub raw: Value,
    /// Telegram bot token (JSON key `token`).
    pub token: String,
    pub chat_id: i64,
    pub default_target: String,
    pub max_media_bytes: u64,
    /// "" when transcription is disabled.
    pub eleven_labs_api_key: String,
    pub targets: IndexMap<String, TargetCfg>,
    /// NEW additive key: named agent preselected for the gcp target.
    pub default_agent: Option<String>,
}

/// `<runtime_dir>/worker-config.json` — the mac worker config.
#[derive(Debug, Clone)]
pub struct WorkerCfg {
    /// Raw file contents, key order + unknown keys preserved.
    pub raw: Value,
    pub gcp_ssh: String,
    pub gcp_key: Option<String>,
    pub remote_dir: String,
    pub remote_node: String,
    pub claude_bin: Option<String>,
    /// Defaulted when absent (runtime default parity).
    pub codex_bin: String,
    pub cwd: Option<String>,
    pub extra_path: Option<String>,
    pub permission_mode: Option<String>,
    pub model: Option<String>,
    pub codex_model: Option<String>,
}

/// The systemd unit name the coordinator installs as.
pub const SERVICE_NAME: &str = "stackhour-bridge.service";
/// The launchd label the mac worker installs as.
pub const LAUNCHD_LABEL: &str = "com.stackhour.bridge-worker";

/// `CONFIG.maxMediaBytes || 512 * 1024 * 1024` — JS `||`, so 0 falls back too.
const DEFAULT_MAX_MEDIA_BYTES: u64 = 512 * 1024 * 1024;

/// A JSON string field, or `None` when absent/null/not-a-string.
fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// A JSON string field that is present AND non-empty — JS truthiness, which
/// is what every `!config.key` check in setup-lib.mjs actually tests.
fn truthy_str(v: &Value, key: &str) -> Option<String> {
    str_field(v, key).filter(|s| !s.is_empty())
}

/// `Number.isSafeInteger` — an integer in ±(2^53 - 1).
fn safe_integer(v: Option<&Value>) -> Option<i64> {
    let n = v?.as_i64()?;
    // as_i64 already rejects fractions; the remaining job is the 2^53 clamp.
    (n.abs() <= 9_007_199_254_740_991).then_some(n)
}

/// Read + parse a bridge config file, preserving key order and unknown keys.
fn read_raw(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

/// Load + runtime-LOOSE validate the coordinator config (exact throw strings).
///
/// The loose gate is coordinator.mjs's, verbatim: token, a safe-integer
/// chatId, and BOTH targets. Everything else is defaulted rather than
/// rejected, so a half-filled config still boots — the installer's STRICT
/// [`validate_coordinator_config`] is where a human gets told off.
pub fn load_coordinator_cfg(path: &Path) -> Result<CoordinatorCfg> {
    let raw = read_raw(path)?;
    let token = truthy_str(&raw, "token");
    let chat_id = safe_integer(raw.get("chatId"));
    let targets_of = |name: &str| raw.get("targets").and_then(|t| t.get(name));
    if token.is_none()
        || chat_id.is_none()
        || targets_of("gcp").is_none()
        || targets_of("mac").is_none()
    {
        return Err(stackhour_core::Error::msg(
            "config.json must define token, an integer chatId, and gcp/mac targets.",
        ));
    }

    let mut targets = IndexMap::new();
    if let Some(Value::Object(map)) = raw.get("targets") {
        for (name, t) in map {
            targets.insert(
                name.clone(),
                TargetCfg {
                    // `targets[name]?.label || name` — the Node label() fallback.
                    label: truthy_str(t, "label").unwrap_or_else(|| name.clone()),
                    // The JSON key is `type`; `kind` is the Rust-side name.
                    kind: truthy_str(t, "type").unwrap_or_else(|| {
                        if name == "gcp" { "local".into() } else { "remote".into() }
                    }),
                    cwd: truthy_str(t, "cwd"),
                    claude_bin: truthy_str(t, "claudeBin"),
                    codex_bin: truthy_str(t, "codexBin"),
                    extra_path: truthy_str(t, "extraPath"),
                    permission_mode: truthy_str(t, "permissionMode"),
                    model: truthy_str(t, "model"),
                    codex_model: truthy_str(t, "codexModel"),
                },
            );
        }
    }

    Ok(CoordinatorCfg {
        token: token.expect("gated above"),
        chat_id: chat_id.expect("gated above"),
        // DIVERGENCE (deliberate): coordinator.mjs does `s.active ||=
        // CONFIG.defaultTarget` with no validation, so a config missing
        // defaultTarget leaves `active` undefined and every later
        // `targets[active]` lookup silently misses. That is plainly a bug, so
        // an absent/blank defaultTarget falls back to "gcp" here. The
        // installer's STRICT validator still rejects the key outright.
        default_target: truthy_str(&raw, "defaultTarget").unwrap_or_else(|| "gcp".into()),
        max_media_bytes: raw
            .get("maxMediaBytes")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_MEDIA_BYTES),
        eleven_labs_api_key: truthy_str(&raw, "elevenLabsApiKey").unwrap_or_default(),
        targets,
        default_agent: truthy_str(&raw, "defaultAgent"),
        raw,
    })
}

/// Load + runtime-LOOSE validate the worker config (5-key check, defaults).
///
/// worker.mjs gates on exactly five keys — gcpKey, gcpSsh, remoteDir,
/// claudeBin, cwd — and defaults remoteNode to `node` and codexBin to
/// `codex`. Note it does NOT require codexBin or remoteNode, so a
/// claude-only worker boots fine.
pub fn load_worker_cfg(path: &Path) -> Result<WorkerCfg> {
    let raw = read_raw(path)?;
    let (gcp_key, gcp_ssh, remote_dir, claude_bin, cwd) = (
        truthy_str(&raw, "gcpKey"),
        truthy_str(&raw, "gcpSsh"),
        truthy_str(&raw, "remoteDir"),
        truthy_str(&raw, "claudeBin"),
        truthy_str(&raw, "cwd"),
    );
    if gcp_key.is_none()
        || gcp_ssh.is_none()
        || remote_dir.is_none()
        || claude_bin.is_none()
        || cwd.is_none()
    {
        return Err(stackhour_core::Error::msg(
            "worker-config.json must define gcpKey, gcpSsh, remoteDir, claudeBin, and cwd.",
        ));
    }
    Ok(WorkerCfg {
        gcp_ssh: gcp_ssh.expect("gated above"),
        gcp_key,
        remote_dir: remote_dir.expect("gated above"),
        remote_node: truthy_str(&raw, "remoteNode").unwrap_or_else(|| "node".into()),
        claude_bin,
        codex_bin: truthy_str(&raw, "codexBin").unwrap_or_else(|| "codex".into()),
        cwd,
        extra_path: truthy_str(&raw, "extraPath"),
        permission_mode: truthy_str(&raw, "permissionMode"),
        model: truthy_str(&raw, "model"),
        codex_model: truthy_str(&raw, "codexModel"),
        raw,
    })
}

/// Installer-STRICT coordinator validation: every problem as an exact string.
///
/// Order and wording are the contract (`validateCoordinatorConfig`); the
/// installer prints the list verbatim.
pub fn validate_coordinator_config(v: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    if !v.is_object() {
        return vec!["config must be an object".into()];
    }
    if truthy_str(v, "token").is_none() {
        errors.push("token is required".into());
    }
    if safe_integer(v.get("chatId")).is_none() {
        errors.push("chatId must be an integer".into());
    }
    if !matches!(v.get("defaultTarget").and_then(Value::as_str), Some("gcp" | "mac")) {
        errors.push("defaultTarget must be gcp or mac".into());
    }
    for name in ["gcp", "mac"] {
        if v.get("targets").and_then(|t| t.get(name)).is_none() {
            errors.push(format!("targets.{name} is required"));
        }
    }
    if let Some(local) = v.get("targets").and_then(|t| t.get("gcp")) {
        for (key, label) in [("cwd", "cwd"), ("claudeBin", "claudeBin"), ("codexBin", "codexBin")] {
            if truthy_str(local, key).is_none() {
                errors.push(format!("targets.gcp.{label} is required"));
            }
        }
        // `local.permissionMode || 'default'` — absent is legal, wrong is not.
        let mode = truthy_str(local, "permissionMode").unwrap_or_else(|| "default".into());
        if mode != "default" && mode != "bypassPermissions" {
            errors.push("targets.gcp.permissionMode is invalid".into());
        }
    }
    errors
}

/// Installer-STRICT worker validation: every problem as an exact string.
pub fn validate_worker_config(v: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    if !v.is_object() {
        return vec!["config must be an object".into()];
    }
    for key in ["gcpSsh", "gcpKey", "remoteDir", "remoteNode", "claudeBin", "codexBin", "cwd"] {
        if truthy_str(v, key).is_none() {
            errors.push(format!("{key} is required"));
        }
    }
    let mode = truthy_str(v, "permissionMode").unwrap_or_else(|| "default".into());
    if mode != "default" && mode != "bypassPermissions" {
        errors.push("permissionMode is invalid".into());
    }
    errors
}

/// PATH assembly with dedupe, order preserved.
///
/// Each part may itself be a `:`-joined PATH; empty segments are dropped and
/// the FIRST occurrence of a directory wins, so earlier arguments keep their
/// precedence.
pub fn merged_path(parts: &[&str]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<&str> = Vec::new();
    for value in parts {
        for part in value.split(':') {
            if !part.is_empty() && seen.insert(part) {
                out.push(part);
            }
        }
    }
    out.join(":")
}

/// POSIX single-quote shell quoting.
///
/// Wrap in single quotes and close/escape/reopen around each embedded quote
/// (`'` -> `'"'"'`), which is safe for every byte including newlines.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// systemd's own quoting: wrap in double quotes, escaping `\` then `"`.
fn systemd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Render the systemd unit text (double-quote escaping, newline rejection).
///
/// `exec` is the stackhour binary, not a Node shim — the unit runs
/// `<exec> bridge coordinator`.
///
/// DIVERGENCE (deliberate): a newline in any interpolated value is rejected
/// rather than written out. setup-lib.mjs interpolates unescaped, so a
/// newline there would inject arbitrary directives into the unit file — an
/// escalation, since the unit is what systemd executes.
pub fn render_systemd_unit(exec: &str, dir: &str, home: &str, path: &str) -> Result<String> {
    for (name, value) in [("exec", exec), ("dir", dir), ("home", home), ("path", path)] {
        if value.contains('\n') || value.contains('\r') {
            return Err(stackhour_core::Error::msg(format!(
                "{name} must not contain a newline"
            )));
        }
    }
    Ok(format!(
        "[Unit]\n\
         Description=Stackhour Telegram bridge coordinator for Claude Code and Codex\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={dir_q}\n\
         ExecStart={exec_q} bridge coordinator\n\
         Restart=always\n\
         RestartSec=5\n\
         UMask=0077\n\
         Environment={home_q}\n\
         Environment={path_q}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        dir_q = systemd_quote(dir),
        exec_q = systemd_quote(exec),
        home_q = systemd_quote(&format!("HOME={home}")),
        path_q = systemd_quote(&format!("PATH={path}")),
    ))
}

/// 5-entity XML escape (`xmlEscape`) for plist text nodes.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Render the launchd plist XML (XML escaping).
///
/// As with the systemd unit, `exec` is the stackhour binary: the agent runs
/// `<exec> bridge worker`.
pub fn render_launch_agent(exec: &str, dir: &str, home: &str, path: &str) -> String {
    let x = xml_escape;
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exec}</string>
    <string>bridge</string>
    <string>worker</string>
  </array>
  <key>WorkingDirectory</key>
  <string>{dir}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home}</string>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>{out}</string>
  <key>StandardErrorPath</key>
  <string>{err}</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        exec = x(exec),
        dir = x(dir),
        home = x(home),
        path = x(path),
        out = x(&format!("{}/worker.launchd.out.log", dir.trim_end_matches('/'))),
        err = x(&format!("{}/worker.launchd.err.log", dir.trim_end_matches('/'))),
    )
}

/// `find_executable`: PATH search for a bare binary name.
///
/// An absolute `name` is probed directly (no PATH walk), matching
/// `findExecutable`. A candidate must be executable AND a regular file — a
/// directory named `claude` on the PATH must not win.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let candidates: Vec<PathBuf> = if Path::new(name).is_absolute() {
        vec![PathBuf::from(name)]
    } else {
        std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join(name))
            .collect()
    };
    candidates.into_iter().find(|c| is_executable_file(c))
}

/// `accessSync(candidate, X_OK)` + `statSync(candidate).isFile()`.
fn is_executable_file(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The fixtures from test/bridge-setup-lib.test.mjs, verbatim.
    fn coordinator_fixture() -> Value {
        json!({
            "token": "test-token",
            "chatId": -100123,
            "defaultTarget": "gcp",
            "targets": {
                "gcp": {
                    "cwd": "/tmp/work",
                    "claudeBin": "/bin/claude",
                    "codexBin": "/bin/codex",
                    "permissionMode": "default"
                },
                "mac": { "permissionMode": "default" }
            }
        })
    }

    fn worker_fixture() -> Value {
        json!({
            "gcpSsh": "user@example.com",
            "gcpKey": "/tmp/key",
            "remoteDir": "/home/user/.local/share/stackhour/bridge",
            "remoteNode": "/usr/bin/node",
            "claudeBin": "/bin/claude",
            "codexBin": "/bin/codex",
            "cwd": "/tmp/work",
            "permissionMode": "default"
        })
    }

    #[test]
    fn valid_configs_produce_no_errors() {
        assert!(validate_coordinator_config(&coordinator_fixture()).is_empty());
        assert!(validate_worker_config(&worker_fixture()).is_empty());
    }

    /// The JS test asserts >= 4 and >= 7 problems for empty objects.
    #[test]
    fn empty_configs_report_every_missing_key() {
        let c = validate_coordinator_config(&json!({}));
        assert!(c.len() >= 4, "{c:?}");
        assert_eq!(
            c,
            [
                "token is required",
                "chatId must be an integer",
                "defaultTarget must be gcp or mac",
                "targets.gcp is required",
                "targets.mac is required",
            ]
        );
        let w = validate_worker_config(&json!({}));
        assert!(w.len() >= 7, "{w:?}");
        assert_eq!(w[0], "gcpSsh is required");
        assert_eq!(w.last().unwrap(), "cwd is required");
    }

    #[test]
    fn a_non_object_config_is_a_single_error() {
        assert_eq!(validate_coordinator_config(&json!("nope")), ["config must be an object"]);
        assert_eq!(validate_worker_config(&json!(7)), ["config must be an object"]);
    }

    /// `permissionMode` is optional but must be one of the two legal values.
    #[test]
    fn an_invalid_permission_mode_is_rejected_but_an_absent_one_is_not() {
        let mut c = coordinator_fixture();
        c["targets"]["gcp"]["permissionMode"] = json!("yolo");
        assert!(validate_coordinator_config(&c).contains(&"targets.gcp.permissionMode is invalid".to_string()));

        let mut c = coordinator_fixture();
        c["targets"]["gcp"].as_object_mut().unwrap().remove("permissionMode");
        assert!(validate_coordinator_config(&c).is_empty(), "absent means default");
    }

    /// chatId is a large negative supergroup id — it must survive as an
    /// integer, and a fractional or 2^53-overflowing value must not.
    #[test]
    fn chat_id_must_be_a_safe_integer() {
        let mut c = coordinator_fixture();
        c["chatId"] = json!(-1_001_234_567_890_i64);
        assert!(validate_coordinator_config(&c).is_empty());

        for bad in [json!(1.5), json!("123"), json!(9_007_199_254_740_993_i64)] {
            let mut c = coordinator_fixture();
            c["chatId"] = bad.clone();
            assert!(
                validate_coordinator_config(&c).contains(&"chatId must be an integer".to_string()),
                "accepted {bad}"
            );
        }
    }

    fn write(dir: &Path, name: &str, v: &Value) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, serde_json::to_string(v).unwrap()).unwrap();
        p
    }

    #[test]
    fn the_coordinator_loader_applies_runtime_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(tmp.path(), "config.json", &coordinator_fixture());
        let cfg = load_coordinator_cfg(&p).expect("loads");
        assert_eq!(cfg.token, "test-token");
        assert_eq!(cfg.chat_id, -100123);
        assert_eq!(cfg.default_target, "gcp");
        // `CONFIG.maxMediaBytes || 512 * 1024 * 1024`.
        assert_eq!(cfg.max_media_bytes, 512 * 1024 * 1024);
        // "" when transcription is not configured, never a None-shaped hole.
        assert_eq!(cfg.eleven_labs_api_key, "");
        // `targets[name]?.label || name`.
        assert_eq!(cfg.targets["gcp"].label, "gcp");
        assert_eq!(cfg.targets["gcp"].kind, "local");
        assert_eq!(cfg.targets["mac"].kind, "remote");
        assert_eq!(cfg.targets["gcp"].claude_bin.as_deref(), Some("/bin/claude"));
    }

    /// Unknown keys and key ORDER must survive a load, because migrate/install
    /// rewrite the file from `raw`.
    #[test]
    fn the_raw_value_preserves_unknown_keys_and_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = coordinator_fixture();
        v["somethingNew"] = json!({ "keep": true });
        let p = write(tmp.path(), "config.json", &v);
        let cfg = load_coordinator_cfg(&p).unwrap();
        assert_eq!(cfg.raw["somethingNew"]["keep"], json!(true));
        let keys: Vec<&str> = cfg.raw.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["token", "chatId", "defaultTarget", "targets", "somethingNew"]);
    }

    /// The loose gate is coordinator.mjs's, and its message is the contract.
    #[test]
    fn the_loose_coordinator_gate_matches_the_node_throw_string() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in [
            json!({ "chatId": 1, "targets": { "gcp": {}, "mac": {} } }),
            json!({ "token": "t", "targets": { "gcp": {}, "mac": {} } }),
            json!({ "token": "t", "chatId": 1, "targets": { "gcp": {} } }),
            json!({ "token": "t", "chatId": 1.5, "targets": { "gcp": {}, "mac": {} } }),
        ] {
            let p = write(tmp.path(), "config.json", &bad);
            let err = load_coordinator_cfg(&p).expect_err("must reject");
            assert_eq!(
                err.message(),
                "config.json must define token, an integer chatId, and gcp/mac targets."
            );
        }
    }

    /// A config with no defaultTarget must not leave the active target unset.
    #[test]
    fn an_absent_default_target_falls_back_to_gcp() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = coordinator_fixture();
        v.as_object_mut().unwrap().remove("defaultTarget");
        let p = write(tmp.path(), "config.json", &v);
        assert_eq!(load_coordinator_cfg(&p).unwrap().default_target, "gcp");
    }

    #[test]
    fn the_worker_loader_defaults_remote_node_and_codex_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = worker_fixture();
        v.as_object_mut().unwrap().remove("remoteNode");
        v.as_object_mut().unwrap().remove("codexBin");
        let p = write(tmp.path(), "worker-config.json", &v);
        let cfg = load_worker_cfg(&p).unwrap();
        assert_eq!(cfg.remote_node, "node");
        assert_eq!(cfg.codex_bin, "codex");
        assert_eq!(cfg.gcp_ssh, "user@example.com");
    }

    /// worker.mjs gates on exactly these five keys — and NOT on codexBin or
    /// remoteNode, so a claude-only worker must still boot.
    #[test]
    fn the_loose_worker_gate_checks_exactly_five_keys() {
        let tmp = tempfile::tempdir().unwrap();
        for key in ["gcpKey", "gcpSsh", "remoteDir", "claudeBin", "cwd"] {
            let mut v = worker_fixture();
            v.as_object_mut().unwrap().remove(key);
            let p = write(tmp.path(), "worker-config.json", &v);
            let err = load_worker_cfg(&p).expect_err("must reject a missing {key}");
            assert_eq!(
                err.message(),
                "worker-config.json must define gcpKey, gcpSsh, remoteDir, claudeBin, and cwd."
            );
        }
    }

    #[test]
    fn a_missing_or_corrupt_config_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_coordinator_cfg(&tmp.path().join("absent.json")).is_err());
        let p = tmp.path().join("bad.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(load_coordinator_cfg(&p).is_err());
    }

    #[test]
    fn merged_path_dedupes_and_keeps_first_occurrence_order() {
        assert_eq!(merged_path(&["/a:/b", "/b:/c"]), "/a:/b:/c");
        assert_eq!(merged_path(&["", "/a", ""]), "/a");
        assert_eq!(merged_path(&["/a::/b"]), "/a:/b", "empty segments drop");
        assert_eq!(merged_path(&[]), "");
    }

    #[test]
    fn shell_quote_survives_embedded_single_quotes() {
        assert_eq!(shell_quote("a'b"), r#"'a'"'"'b'"#);
        assert_eq!(shell_quote("plain"), "'plain'");
        // The whole point: no shell metacharacter escapes the quoting.
        assert_eq!(shell_quote("; rm -rf /"), "'; rm -rf /'");
    }

    #[test]
    fn the_systemd_unit_execs_the_stackhour_binary() {
        let unit = render_systemd_unit(
            "/usr/local/bin/stackhour",
            "/home/me/.local/share/stackhour/bridge",
            "/home/me",
            "/usr/bin:/bin",
        )
        .unwrap();
        assert!(unit.contains(r#"ExecStart="/usr/local/bin/stackhour" bridge coordinator"#), "{unit}");
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains(r#"Environment="HOME=/home/me""#));
        assert!(unit.contains(r#"Environment="PATH=/usr/bin:/bin""#));
        // No placeholder ever reaches a real unit file.
        for bad in ["CHANGE_ME", "__HOME__", "User=", "coordinator.mjs", "node"] {
            assert!(!unit.contains(bad), "{bad} leaked into the unit:\n{unit}");
        }
    }

    #[test]
    fn systemd_quoting_escapes_backslashes_and_quotes() {
        let unit = render_systemd_unit(r#"/opt/a"b\c/stackhour"#, "/d", "/h", "/p").unwrap();
        assert!(unit.contains(r#"ExecStart="/opt/a\"b\\c/stackhour""#), "{unit}");
    }

    /// A newline would inject arbitrary systemd directives into the unit.
    #[test]
    fn a_newline_in_any_field_is_rejected() {
        assert!(render_systemd_unit("/bin/x\nExecStartPre=/bin/evil", "/d", "/h", "/p").is_err());
        assert!(render_systemd_unit("/bin/x", "/d\n", "/h", "/p").is_err());
        assert!(render_systemd_unit("/bin/x", "/d", "/h\n", "/p").is_err());
        assert!(render_systemd_unit("/bin/x", "/d", "/h", "/p\n").is_err());
        assert!(render_systemd_unit("/bin/x", "/d", "/h", "/p\r").is_err());
    }

    #[test]
    fn the_launch_agent_escapes_xml_and_execs_the_binary() {
        let plist = render_launch_agent("/opt/node&tools/stackhour", "/Users/me/A & B", "/Users/me", "/usr/bin:/bin");
        assert!(plist.contains(LAUNCHD_LABEL));
        assert!(plist.contains("node&amp;tools"), "{plist}");
        assert!(plist.contains("A &amp; B"), "{plist}");
        assert!(plist.contains("<string>bridge</string>"));
        assert!(plist.contains("<string>worker</string>"));
        assert!(plist.contains("/Users/me/A &amp; B/worker.launchd.out.log"), "{plist}");
        for bad in ["__HOME__", "__NODE__", "worker.mjs"] {
            assert!(!plist.contains(bad), "{bad} leaked:\n{plist}");
        }
    }

    #[test]
    fn xml_escape_covers_all_five_entities() {
        assert_eq!(xml_escape(r#"&<>"'"#), "&amp;&lt;&gt;&quot;&apos;");
    }

    #[test]
    fn find_executable_probes_absolute_paths_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("thing");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(find_executable(bin.to_str().unwrap()), Some(bin.clone()));
        assert_eq!(find_executable(""), None);
        assert_eq!(find_executable("/definitely/not/here/xyz"), None);

        // A non-executable file on the PATH must not win.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(find_executable(bin.to_str().unwrap()), None);
        }
    }

    /// A directory named like the binary must never be returned as one.
    #[test]
    fn find_executable_rejects_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("claude");
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(find_executable(dir.to_str().unwrap()), None);
    }
}
