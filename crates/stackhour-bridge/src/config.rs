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
    /// Bot API root (JSON key `apiRoot`), for pointing the daemon at a LOCAL
    /// MOCK during parity testing. Absent in every real config, where the
    /// public `https://api.telegram.org` is used.
    pub api_root: Option<String>,
    /// Speech-to-text endpoint (JSON key `elevenLabsEndpoint`), the same kind
    /// of seam as [`Self::api_root`]: it exists so a parity run can point the
    /// voice lane at a LOCAL MOCK instead of api.elevenlabs.io. Absent in
    /// every real config, where [`crate::media::ELEVENLABS_ENDPOINT`] is used.
    pub eleven_labs_endpoint: Option<String>,
}

/// `<runtime_dir>/worker-config.json` — a pull-worker config.
#[derive(Debug, Clone)]
pub struct WorkerCfg {
    /// Raw file contents, key order + unknown keys preserved.
    pub raw: Value,
    /// SSH destination of the coordinator (leader) box. JSON `leaderSsh`,
    /// with the legacy `gcpSsh` spelling accepted as a fallback — the field
    /// predates arbitrary topologies, when the leader was always "the GCP
    /// box".
    pub leader_ssh: String,
    /// SSH key for the leader. JSON `leaderKey`, legacy `gcpKey` fallback.
    pub leader_key: Option<String>,
    pub remote_dir: String,
    pub claude_bin: Option<String>,
    /// Defaulted when absent (runtime default parity).
    pub codex_bin: String,
    pub cwd: Option<String>,
    pub extra_path: Option<String>,
    pub permission_mode: Option<String>,
    pub model: Option<String>,
    pub codex_model: Option<String>,
    /// The target name this worker claims jobs under (JSON `target`). Absent
    /// = legacy claim-anything mode: the worker takes the oldest job
    /// regardless of which target it was dispatched to — exactly what the
    /// live Node Mac worker does.
    pub target: Option<String>,
}

/// The systemd unit name the coordinator installs as.
pub const SERVICE_NAME: &str = "stackhour-bridge.service";
/// The systemd unit name a LINUX pull-worker installs as. NEW vs the Node
/// bridge, whose worker installer was macOS-only.
pub const WORKER_SERVICE_NAME: &str = "stackhour-bridge-worker.service";
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

/// Load + runtime-LOOSE validate the coordinator config.
///
/// DELIBERATE DIVERGENCE from coordinator.mjs, whose loose gate demanded
/// BOTH the `gcp` and `mac` targets by name (throw string: `config.json must
/// define token, an integer chatId, and gcp/mac targets.`). Targets are now
/// an arbitrary roster — any names, zero or more `type == "local"` entries
/// (zero = a leader-only coordinator that only dispatches to pull-workers) —
/// so the gate asks for token, a safe-integer chatId, and AT LEAST ONE
/// target. Everything else is defaulted rather than rejected, so a
/// half-filled config still boots — the installer's STRICT
/// [`validate_coordinator_config`] is where a human gets told off.
pub fn load_coordinator_cfg(path: &Path) -> Result<CoordinatorCfg> {
    let raw = read_raw(path)?;
    let token = truthy_str(&raw, "token");
    let chat_id = safe_integer(raw.get("chatId"));
    let has_targets = raw
        .get("targets")
        .and_then(Value::as_object)
        .is_some_and(|t| !t.is_empty());
    if token.is_none() || chat_id.is_none() || !has_targets {
        return Err(stackhour_core::Error::msg(
            "config.json must define token, an integer chatId, and at least one target.",
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
                    kind: target_kind(name, t),
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
        // an absent/blank defaultTarget falls back to "gcp" when a gcp
        // target exists (the legacy behaviour), else to the FIRST target in
        // config file order (serde_json preserves it). The installer's
        // STRICT validator still rejects an absent-but-wrong key outright.
        default_target: truthy_str(&raw, "defaultTarget").unwrap_or_else(|| {
            if targets.contains_key("gcp") {
                "gcp".into()
            } else {
                targets
                    .keys()
                    .next()
                    .cloned()
                    .expect("gated above: at least one target")
            }
        }),
        max_media_bytes: raw
            .get("maxMediaBytes")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_MEDIA_BYTES),
        eleven_labs_api_key: truthy_str(&raw, "elevenLabsApiKey").unwrap_or_default(),
        targets,
        default_agent: truthy_str(&raw, "defaultAgent"),
        api_root: truthy_str(&raw, "apiRoot").map(|r| r.trim_end_matches('/').to_string()),
        eleven_labs_endpoint: truthy_str(&raw, "elevenLabsEndpoint"),
        raw,
    })
}

/// A worker-config key that grew a `leader*` spelling: the preferred key is
/// read first, then the legacy `gcp*` fallback. JSON back-compat is
/// mandatory — every existing worker-config.json keeps loading unchanged.
pub(crate) fn leader_aliased(v: &Value, preferred: &str, legacy: &str) -> Option<String> {
    truthy_str(v, preferred).or_else(|| truthy_str(v, legacy))
}

/// The effective kind of a raw `targets` entry: its JSON `type` when truthy,
/// else the legacy name-keyed default — `gcp` is local, anything else is
/// remote (the same rule the loader and the strict validator apply).
pub(crate) fn target_kind(name: &str, t: &Value) -> String {
    truthy_str(t, "type").unwrap_or_else(|| {
        if name == "gcp" {
            "local".into()
        } else {
            "remote".into()
        }
    })
}

/// Load + runtime-LOOSE validate the worker config (5-key check, defaults).
///
/// worker.mjs gates on exactly five keys — gcpKey, gcpSsh, remoteDir,
/// claudeBin, cwd — and defaults remoteNode to `node` and codexBin to
/// `codex`. Note it does NOT require codexBin or remoteNode, so a
/// claude-only worker boots fine. `leaderSsh`/`leaderKey` are accepted as
/// preferred spellings of `gcpSsh`/`gcpKey` (the leader is no longer
/// necessarily a GCP box); the PARITY throw string keeps the legacy names.
pub fn load_worker_cfg(path: &Path) -> Result<WorkerCfg> {
    let raw = read_raw(path)?;
    let (leader_key, leader_ssh, remote_dir, claude_bin, cwd) = (
        leader_aliased(&raw, "leaderKey", "gcpKey"),
        leader_aliased(&raw, "leaderSsh", "gcpSsh"),
        truthy_str(&raw, "remoteDir"),
        truthy_str(&raw, "claudeBin"),
        truthy_str(&raw, "cwd"),
    );
    if leader_key.is_none()
        || leader_ssh.is_none()
        || remote_dir.is_none()
        || claude_bin.is_none()
        || cwd.is_none()
    {
        return Err(stackhour_core::Error::msg(
            "worker-config.json must define gcpKey, gcpSsh, remoteDir, claudeBin, and cwd.",
        ));
    }
    Ok(WorkerCfg {
        leader_ssh: leader_ssh.expect("gated above"),
        leader_key,
        remote_dir: remote_dir.expect("gated above"),
        claude_bin,
        codex_bin: truthy_str(&raw, "codexBin").unwrap_or_else(|| "codex".into()),
        cwd,
        extra_path: truthy_str(&raw, "extraPath"),
        permission_mode: truthy_str(&raw, "permissionMode"),
        model: truthy_str(&raw, "model"),
        codex_model: truthy_str(&raw, "codexModel"),
        target: truthy_str(&raw, "target"),
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
    // DELIBERATE DIVERGENCE from validateCoordinatorConfig, which pinned
    // `defaultTarget must be gcp or mac`, required both of those targets by
    // name, and only inspected targets.gcp's local keys. The roster is now
    // arbitrary: at least one target of any name, defaultTarget (when
    // present) must name one of them, and EVERY `type == "local"` target
    // needs the keys a local run requires. Zero local targets is a valid
    // leader-only coordinator.
    let targets = v.get("targets").and_then(Value::as_object);
    match targets {
        Some(t) if !t.is_empty() => {}
        _ => errors.push("targets must define at least one target".into()),
    }
    if let Some(want) = v.get("defaultTarget").and_then(Value::as_str) {
        let known = targets.is_some_and(|t| t.contains_key(want));
        if !known {
            errors.push(format!("defaultTarget '{want}' is not a configured target"));
        }
    }
    for (name, t) in targets.into_iter().flatten() {
        // The JSON key is `type`, with the legacy name-keyed default: gcp is
        // local, anything else is remote (see [`target_kind`]).
        if target_kind(name, t) != "local" {
            continue;
        }
        for key in ["cwd", "claudeBin", "codexBin"] {
            if truthy_str(t, key).is_none() {
                errors.push(format!("targets.{name}.{key} is required"));
            }
        }
        // `local.permissionMode || 'default'` — absent is legal, wrong is not.
        let mode = truthy_str(t, "permissionMode").unwrap_or_else(|| "default".into());
        if mode != "default" && mode != "bypassPermissions" {
            errors.push(format!("targets.{name}.permissionMode is invalid"));
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
    // The two leader keys accept either spelling; the error names both.
    for (preferred, legacy) in [("leaderSsh", "gcpSsh"), ("leaderKey", "gcpKey")] {
        if leader_aliased(v, preferred, legacy).is_none() {
            errors.push(format!("{preferred} (or {legacy}) is required"));
        }
    }
    // `remoteNode` was required while claim/return were Node shims on the
    // leader. The worker now execs the leader's `stackhour` binary directly,
    // so the key is obsolete: an existing config may still carry it (unknown
    // keys are preserved, never rejected), but it is no longer read.
    for key in ["remoteDir", "claudeBin", "codexBin", "cwd"] {
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

/// Render the coordinator's systemd unit text (double-quote escaping,
/// newline rejection).
///
/// `exec` is the stackhour binary, not a Node shim — the unit runs
/// `<exec> bridge coordinator`.
///
/// DIVERGENCE (deliberate): a newline in any interpolated value is rejected
/// rather than written out. setup-lib.mjs interpolates unescaped, so a
/// newline there would inject arbitrary directives into the unit file — an
/// escalation, since the unit is what systemd executes.
pub fn render_systemd_unit(exec: &str, dir: &str, home: &str, path: &str) -> Result<String> {
    render_systemd_unit_for("coordinator", exec, dir, home, path)
}

/// Render a LINUX pull-worker's systemd unit text: identical machinery to
/// the coordinator unit, execing `<exec> bridge worker`. NEW vs the Node
/// bridge, whose worker only ever ran under launchd.
pub fn render_worker_systemd_unit(exec: &str, dir: &str, home: &str, path: &str) -> Result<String> {
    render_systemd_unit_for("worker", exec, dir, home, path)
}

/// The shared unit body behind both renderers; `role` is the `bridge <role>`
/// daemon verb and lands in the Description line too.
fn render_systemd_unit_for(role: &str, exec: &str, dir: &str, home: &str, path: &str) -> Result<String> {
    for (name, value) in [("exec", exec), ("dir", dir), ("home", home), ("path", path)] {
        if value.contains('\n') || value.contains('\r') {
            return Err(stackhour_core::Error::msg(format!(
                "{name} must not contain a newline"
            )));
        }
    }
    Ok(format!(
        "[Unit]\n\
         Description=Stackhour Telegram bridge {role} for Claude Code and Codex\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={dir_q}\n\
         ExecStart={exec_q} bridge {role}\n\
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

    /// DELIBERATE DIVERGENCE from the JS test (>= 4 coordinator problems):
    /// the roster is arbitrary now, so an empty coordinator config is
    /// missing exactly three things — token, chatId and any target at all —
    /// and the worker's two leader keys name both accepted spellings.
    #[test]
    fn empty_configs_report_every_missing_key() {
        let c = validate_coordinator_config(&json!({}));
        assert_eq!(
            c,
            [
                "token is required",
                "chatId must be an integer",
                "targets must define at least one target",
            ]
        );
        let w = validate_worker_config(&json!({}));
        // Six, not the historical seven: `remoteNode` is no longer required.
        assert!(w.len() >= 6, "{w:?}");
        assert!(
            !w.iter().any(|e| e.contains("remoteNode")),
            "remoteNode is a retired key and must not be demanded: {w:?}"
        );
        assert_eq!(w[0], "leaderSsh (or gcpSsh) is required");
        assert_eq!(w[1], "leaderKey (or gcpKey) is required");
        assert_eq!(w.last().unwrap(), "cwd is required");
    }

    /// The strict gate over the generalized roster: any names, every local
    /// target audited under its own key, defaultTarget checked against the
    /// roster, and zero local targets (leader-only) fully valid.
    #[test]
    fn the_strict_gate_audits_every_local_target_by_name() {
        // A leader-only config: arbitrary names, no local target at all.
        let leader_only = json!({
            "token": "t", "chatId": 7, "defaultTarget": "pi",
            "targets": { "pi": {}, "attic": {} },
        });
        assert!(validate_coordinator_config(&leader_only).is_empty());

        // Every `type == "local"` target needs the local-run keys, and the
        // error names the actual target.
        let two_locals = json!({
            "token": "t", "chatId": 7, "defaultTarget": "hetzner",
            "targets": {
                "hetzner": { "type": "local" },
                "pi": {},
                "attic": { "type": "local", "cwd": "/w", "claudeBin": "/c",
                           "codexBin": "/x", "permissionMode": "yolo" },
            },
        });
        assert_eq!(
            validate_coordinator_config(&two_locals),
            [
                "targets.hetzner.cwd is required",
                "targets.hetzner.claudeBin is required",
                "targets.hetzner.codexBin is required",
                "targets.attic.permissionMode is invalid",
            ]
        );

        // defaultTarget must name a configured target — when present.
        let bad_default = json!({
            "token": "t", "chatId": 7, "defaultTarget": "moon",
            "targets": { "pi": {} },
        });
        assert_eq!(
            validate_coordinator_config(&bad_default),
            ["defaultTarget 'moon' is not a configured target"]
        );
        let absent_default = json!({ "token": "t", "chatId": 7, "targets": { "pi": {} } });
        assert!(
            validate_coordinator_config(&absent_default).is_empty(),
            "an absent defaultTarget is legal — the loader falls back"
        );
    }

    /// A bare `gcp` target still defaults to `type: local` in the STRICT
    /// gate too, so the legacy fixture keeps demanding its local keys.
    #[test]
    fn the_strict_gate_keeps_the_name_keyed_type_default() {
        let c = validate_coordinator_config(&json!({
            "token": "t", "chatId": 7, "defaultTarget": "gcp",
            "targets": { "gcp": {} },
        }));
        assert_eq!(
            c,
            [
                "targets.gcp.cwd is required",
                "targets.gcp.claudeBin is required",
                "targets.gcp.codexBin is required",
            ]
        );
    }

    #[test]
    fn a_non_object_config_is_a_single_error() {
        assert_eq!(
            validate_coordinator_config(&json!("nope")),
            ["config must be an object"]
        );
        assert_eq!(validate_worker_config(&json!(7)), ["config must be an object"]);
    }

    /// `permissionMode` is optional but must be one of the two legal values.
    #[test]
    fn an_invalid_permission_mode_is_rejected_but_an_absent_one_is_not() {
        let mut c = coordinator_fixture();
        c["targets"]["gcp"]["permissionMode"] = json!("yolo");
        assert!(
            validate_coordinator_config(&c).contains(&"targets.gcp.permissionMode is invalid".to_string())
        );

        let mut c = coordinator_fixture();
        c["targets"]["gcp"]
            .as_object_mut()
            .unwrap()
            .remove("permissionMode");
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

    /// Both endpoint seams are absent from every real config, and absent must
    /// mean "the public endpoint" — a blank string is not a valid override
    /// either, or a stray `"elevenLabsEndpoint": ""` would silently point
    /// transcription at nowhere.
    #[test]
    fn the_endpoint_seams_default_to_the_public_services() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(tmp.path(), "config.json", &coordinator_fixture());
        let cfg = load_coordinator_cfg(&p).unwrap();
        assert_eq!(cfg.api_root, None);
        assert_eq!(cfg.eleven_labs_endpoint, None);
        assert_eq!(
            crate::media::ElevenLabs::from_cfg(&cfg).endpoint,
            crate::media::ELEVENLABS_ENDPOINT
        );

        let mut v = coordinator_fixture();
        v["elevenLabsEndpoint"] = json!("");
        let p = write(tmp.path(), "blank.json", &v);
        assert_eq!(load_coordinator_cfg(&p).unwrap().eleven_labs_endpoint, None);

        v["elevenLabsEndpoint"] = json!("http://127.0.0.1:9/v1/speech-to-text");
        let p = write(tmp.path(), "mock.json", &v);
        let cfg = load_coordinator_cfg(&p).unwrap();
        assert_eq!(
            crate::media::ElevenLabs::from_cfg(&cfg).endpoint,
            "http://127.0.0.1:9/v1/speech-to-text"
        );
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
        assert_eq!(
            keys,
            ["token", "chatId", "defaultTarget", "targets", "somethingNew"]
        );
    }

    /// DELIBERATE DIVERGENCE from the Node throw string (`…and gcp/mac
    /// targets.`): the loose gate now accepts any roster with at least one
    /// target, and its message says so.
    #[test]
    fn the_loose_coordinator_gate_accepts_any_roster_of_at_least_one_target() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in [
            json!({ "chatId": 1, "targets": { "gcp": {}, "mac": {} } }),
            json!({ "token": "t", "targets": { "gcp": {}, "mac": {} } }),
            json!({ "token": "t", "chatId": 1.5, "targets": { "gcp": {}, "mac": {} } }),
            json!({ "token": "t", "chatId": 1 }),
            json!({ "token": "t", "chatId": 1, "targets": {} }),
            json!({ "token": "t", "chatId": 1, "targets": "nope" }),
        ] {
            let p = write(tmp.path(), "config.json", &bad);
            let err = load_coordinator_cfg(&p).expect_err("must reject");
            assert_eq!(
                err.message(),
                "config.json must define token, an integer chatId, and at least one target."
            );
        }

        // One target of ANY name now boots — the Node gate demanded gcp+mac.
        let p = write(
            tmp.path(),
            "config.json",
            &json!({ "token": "t", "chatId": 1, "targets": { "pi": {} } }),
        );
        let cfg = load_coordinator_cfg(&p).expect("a one-target roster boots");
        assert_eq!(cfg.targets.len(), 1);
        assert_eq!(cfg.targets["pi"].kind, "remote", "non-gcp names default remote");
    }

    /// A config with no defaultTarget must not leave the active target unset:
    /// "gcp" when a gcp target exists, else the first target in file order.
    #[test]
    fn an_absent_default_target_falls_back_to_gcp() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = coordinator_fixture();
        v.as_object_mut().unwrap().remove("defaultTarget");
        let p = write(tmp.path(), "config.json", &v);
        assert_eq!(load_coordinator_cfg(&p).unwrap().default_target, "gcp");

        // No gcp target: the FIRST target in config file order wins.
        let p = write(
            tmp.path(),
            "config.json",
            &json!({ "token": "t", "chatId": 1, "targets": { "pi": {}, "attic": {} } }),
        );
        assert_eq!(load_coordinator_cfg(&p).unwrap().default_target, "pi");
    }

    /// (d) The worker config's leaderSsh/leaderKey aliases and the new
    /// `target` field, round-tripped through the loader.
    #[test]
    fn the_worker_loader_reads_the_leader_aliases_and_the_target_field() {
        let tmp = tempfile::tempdir().unwrap();

        // Preferred spellings + a target: the new-style config.
        let v = json!({
            "leaderSsh": "user@leader.example",
            "leaderKey": "/tmp/leader-key",
            "remoteDir": "/srv/bridge",
            "claudeBin": "/bin/claude",
            "cwd": "/tmp/work",
            "target": "attic",
        });
        let p = write(tmp.path(), "worker-config.json", &v);
        let cfg = load_worker_cfg(&p).expect("leader spellings load");
        assert_eq!(cfg.leader_ssh, "user@leader.example");
        assert_eq!(cfg.leader_key.as_deref(), Some("/tmp/leader-key"));
        assert_eq!(cfg.target.as_deref(), Some("attic"));
        // The raw value keeps the spelling the user wrote.
        assert!(cfg.raw.get("leaderSsh").is_some() && cfg.raw.get("gcpSsh").is_none());

        // Legacy spellings, no target: the live Mac worker's config.
        let cfg = load_worker_cfg(&write(tmp.path(), "w2.json", &worker_fixture())).unwrap();
        assert_eq!(cfg.leader_ssh, "user@example.com");
        assert_eq!(cfg.leader_key.as_deref(), Some("/tmp/key"));
        assert_eq!(cfg.target, None, "absent target = legacy claim-anything mode");

        // Both spellings present: the preferred one wins.
        let mut v = worker_fixture();
        v["leaderSsh"] = json!("user@new-leader");
        let cfg = load_worker_cfg(&write(tmp.path(), "w3.json", &v)).unwrap();
        assert_eq!(cfg.leader_ssh, "user@new-leader");

        // The strict validator accepts either spelling.
        let mut v = worker_fixture();
        let obj = v.as_object_mut().unwrap();
        let ssh = obj.remove("gcpSsh").unwrap();
        obj.insert("leaderSsh".into(), ssh);
        assert!(validate_worker_config(&v).is_empty());
    }

    /// `remoteNode` is a retired key: a config that still carries it loads
    /// fine (unknown keys are preserved in `raw`, never rejected) and one that
    /// has dropped it loads just the same, because nothing reads it any more.
    #[test]
    fn the_worker_loader_defaults_codex_bin_and_ignores_remote_node() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = worker_fixture();
        v.as_object_mut().unwrap().remove("remoteNode");
        v.as_object_mut().unwrap().remove("codexBin");
        let p = write(tmp.path(), "worker-config.json", &v);
        let cfg = load_worker_cfg(&p).unwrap();
        assert_eq!(cfg.codex_bin, "codex");
        assert_eq!(cfg.leader_ssh, "user@example.com");

        // The legacy spelling still parses; it is simply inert.
        let legacy = worker_fixture();
        assert!(legacy.get("remoteNode").is_some(), "fixture keeps the old key");
        let p = write(tmp.path(), "legacy-worker-config.json", &legacy);
        let cfg = load_worker_cfg(&p).unwrap();
        assert_eq!(
            cfg.raw.get("remoteNode").and_then(Value::as_str),
            Some("/usr/bin/node")
        );
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
            let err = load_worker_cfg(&p).expect_err("must reject a missing key");
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
        assert!(
            unit.contains(r#"ExecStart="/usr/local/bin/stackhour" bridge coordinator"#),
            "{unit}"
        );
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains(r#"Environment="HOME=/home/me""#));
        assert!(unit.contains(r#"Environment="PATH=/usr/bin:/bin""#));
        // No placeholder ever reaches a real unit file.
        for bad in ["CHANGE_ME", "__HOME__", "User=", "coordinator.mjs", "node"] {
            assert!(!unit.contains(bad), "{bad} leaked into the unit:\n{unit}");
        }
    }

    /// The Linux worker unit shares the coordinator's machinery verbatim,
    /// differing only in the daemon verb (and the Description that names it).
    #[test]
    fn the_worker_systemd_unit_execs_the_worker_verb() {
        let unit = render_worker_systemd_unit(
            "/usr/local/bin/stackhour",
            "/home/me/.local/share/stackhour/bridge",
            "/home/me",
            "/usr/bin:/bin",
        )
        .unwrap();
        assert!(
            unit.contains(r#"ExecStart="/usr/local/bin/stackhour" bridge worker"#),
            "{unit}"
        );
        assert!(unit.contains("bridge worker for Claude Code and Codex"), "{unit}");
        assert!(!unit.contains("bridge coordinator"), "{unit}");
        // The same newline-injection gate guards this renderer too.
        assert!(render_worker_systemd_unit("/bin/x\nExecStartPre=/bin/evil", "/d", "/h", "/p").is_err());
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
        let plist = render_launch_agent(
            "/opt/node&tools/stackhour",
            "/Users/me/A & B",
            "/Users/me",
            "/usr/bin:/bin",
        );
        assert!(plist.contains(LAUNCHD_LABEL));
        assert!(plist.contains("node&amp;tools"), "{plist}");
        assert!(plist.contains("A &amp; B"), "{plist}");
        assert!(plist.contains("<string>bridge</string>"));
        assert!(plist.contains("<string>worker</string>"));
        assert!(
            plist.contains("/Users/me/A &amp; B/worker.launchd.out.log"),
            "{plist}"
        );
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
