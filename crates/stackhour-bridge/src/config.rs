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

/// Load + runtime-LOOSE validate the coordinator config (exact throw strings).
pub fn load_coordinator_cfg(path: &Path) -> Result<CoordinatorCfg> {
    let _ = path;
    todo!()
}

/// Load + runtime-LOOSE validate the worker config (5-key check, defaults).
pub fn load_worker_cfg(path: &Path) -> Result<WorkerCfg> {
    let _ = path;
    todo!()
}

/// Installer-STRICT coordinator validation: every problem as an exact string.
pub fn validate_coordinator_config(v: &Value) -> Vec<String> {
    let _ = v;
    todo!()
}

/// Installer-STRICT worker validation: every problem as an exact string.
pub fn validate_worker_config(v: &Value) -> Vec<String> {
    let _ = v;
    todo!()
}

/// PATH assembly with dedupe, order preserved.
pub fn merged_path(parts: &[&str]) -> String {
    let _ = parts;
    todo!()
}

/// POSIX single-quote shell quoting.
pub fn shell_quote(s: &str) -> String {
    let _ = s;
    todo!()
}

/// Render the systemd unit text (double-quote escaping, newline rejection).
pub fn render_systemd_unit(exec: &str, dir: &str, home: &str, path: &str) -> Result<String> {
    let _ = (exec, dir, home, path);
    todo!()
}

/// Render the launchd plist XML (XML escaping).
pub fn render_launch_agent(exec: &str, dir: &str, home: &str, path: &str) -> String {
    let _ = (exec, dir, home, path);
    todo!()
}

/// `find_executable`: PATH search for a bare binary name.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    let _ = name;
    todo!()
}
