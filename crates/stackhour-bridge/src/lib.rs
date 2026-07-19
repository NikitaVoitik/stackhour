//! stackhour-bridge — Telegram coordinator + mac worker + claim/return +
//! tg-send + installer + bridge doctor. Blocking Rust: thread-per-child,
//! thread-per-timer.

use std::path::{Path, PathBuf};

pub mod commands;
pub mod config;
pub mod coordinator;
pub mod doctor;
pub mod engines;
pub mod installer;
pub mod jobs;
pub mod keyboard;
pub mod media;
pub mod registry_ctx;
pub mod render;
pub mod skills;
pub mod souls;
pub mod state;
pub mod telegram;
pub mod tgsend;
pub mod worker;

/// Bridge runtime-directory layout. Resolution honours
/// `STACKHOUR_BRIDGE_HOME` (parity with the JS runtime-dir resolution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgePaths {
    pub runtime_dir: PathBuf,
    pub jobs_dir: PathBuf,
    pub results_dir: PathBuf,
    pub media_dir: PathBuf,
    /// `<runtime_dir>/state.json`.
    pub state_path: PathBuf,
    /// `<runtime_dir>/config.json` (coordinator).
    pub config_path: PathBuf,
    /// `<runtime_dir>/worker-config.json` (worker).
    pub worker_config_path: PathBuf,
}

impl BridgePaths {
    /// Derive every path from a runtime dir.
    pub fn from_runtime_dir(runtime_dir: &Path) -> Self {
        let _ = runtime_dir;
        todo!()
    }

    /// Resolve the runtime dir from env (`STACKHOUR_BRIDGE_HOME`) / defaults.
    pub fn resolve(env: &impl Fn(&str) -> Option<String>, home: &Path) -> Self {
        let _ = (env, home);
        todo!()
    }
}

/// Append one line in the exact coordinator.log/worker.log format to `file`
/// AND stdout.
pub fn log_line(file: &Path, msg: &str) {
    let _ = (file, msg);
    todo!()
}
