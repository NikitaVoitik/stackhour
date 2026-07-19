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
pub mod local_lane;
pub mod macqueue;
pub mod media;
pub mod migrate;
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
    /// Jobs the worker has claimed but not yet returned. `claim` renames into
    /// here; `return` removes the marker. Nothing ever reaps it — a job whose
    /// worker died stays here forever, exactly as in the JS.
    pub inprogress_dir: PathBuf,
    pub results_dir: PathBuf,
    pub media_dir: PathBuf,
    /// `<runtime_dir>/worker-heartbeat` — a bare decimal ms epoch, no
    /// trailing newline. Written by every `claim` poll, read by
    /// [`jobs::worker_alive`].
    pub heartbeat_path: PathBuf,
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
        BridgePaths {
            jobs_dir: runtime_dir.join("jobs"),
            inprogress_dir: runtime_dir.join("inprogress"),
            results_dir: runtime_dir.join("results"),
            media_dir: runtime_dir.join("media"),
            heartbeat_path: runtime_dir.join("worker-heartbeat"),
            state_path: runtime_dir.join("state.json"),
            config_path: runtime_dir.join("config.json"),
            worker_config_path: runtime_dir.join("worker-config.json"),
            runtime_dir: runtime_dir.to_path_buf(),
        }
    }

    /// Resolve the runtime dir from env (`STACKHOUR_BRIDGE_HOME`) / defaults.
    ///
    /// Mirrors cli.mjs: `--runtime-dir` (the caller's job, since it comes from
    /// argv) beats `$STACKHOUR_BRIDGE_HOME`, which beats
    /// `~/.local/share/stackhour/bridge`. An EMPTY env var is treated as
    /// unset — `STACKHOUR_BRIDGE_HOME=` must not resolve the runtime dir to
    /// `/`, which would scatter jobs/ and state.json across the filesystem
    /// root.
    pub fn resolve(env: &impl Fn(&str) -> Option<String>, home: &Path) -> Self {
        let runtime_dir = env("STACKHOUR_BRIDGE_HOME")
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("share").join("stackhour").join("bridge"));
        Self::from_runtime_dir(&runtime_dir)
    }

    /// `mkdir -p` the directories the runtime writes into.
    ///
    /// `media/` is 0700 because it holds Telegram attachments — other users
    /// on a shared box have no business reading them.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for dir in [&self.jobs_dir, &self.inprogress_dir, &self.results_dir] {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::create_dir_all(&self.media_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.media_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

/// Append one line in the exact coordinator.log/worker.log format to `file`
/// AND stdout.
///
/// `[<ISO-8601 ms UTC>] <msg>\n`, matching
/// `` `[${new Date().toISOString()}] ${a.join(' ')}\n` ``.
///
/// Both writes are best-effort and swallowed, exactly as the JS does: a full
/// disk or a closed stdout (the daemon is detached) must not take the bridge
/// down mid-conversation.
pub fn log_line(file: &Path, msg: &str) {
    let line = format!("[{}] {msg}\n", iso_now());
    if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
        use std::io::Write as _;
        let _ = fh.write_all(line.as_bytes());
    }
    print!("{line}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// `new Date().toISOString()` — always UTC, always exactly 3 fractional
/// digits, always a trailing `Z`.
fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn every_path_hangs_off_the_runtime_dir() {
        let p = BridgePaths::from_runtime_dir(Path::new("/rt"));
        assert_eq!(p.runtime_dir, Path::new("/rt"));
        assert_eq!(p.jobs_dir, Path::new("/rt/jobs"));
        assert_eq!(p.inprogress_dir, Path::new("/rt/inprogress"));
        assert_eq!(p.results_dir, Path::new("/rt/results"));
        assert_eq!(p.media_dir, Path::new("/rt/media"));
        // The heartbeat is a FILE in the runtime dir, not a subdirectory —
        // claim.mjs and the coordinator both hardcode that location.
        assert_eq!(p.heartbeat_path, Path::new("/rt/worker-heartbeat"));
        assert_eq!(p.state_path, Path::new("/rt/state.json"));
        assert_eq!(p.config_path, Path::new("/rt/config.json"));
        assert_eq!(p.worker_config_path, Path::new("/rt/worker-config.json"));
    }

    #[test]
    fn the_env_override_beats_the_default_location() {
        let p = BridgePaths::resolve(&env_of(&[("STACKHOUR_BRIDGE_HOME", "/custom")]), Path::new("/h"));
        assert_eq!(p.runtime_dir, Path::new("/custom"));

        let p = BridgePaths::resolve(&env_of(&[]), Path::new("/h"));
        assert_eq!(p.runtime_dir, Path::new("/h/.local/share/stackhour/bridge"));
    }

    /// `STACKHOUR_BRIDGE_HOME=` must not resolve the runtime dir to `/` and
    /// scatter jobs/, results/ and state.json across the filesystem root.
    #[test]
    fn an_empty_env_override_falls_back_to_the_default() {
        for blank in ["", "   "] {
            let p = BridgePaths::resolve(&env_of(&[("STACKHOUR_BRIDGE_HOME", blank)]), Path::new("/h"));
            assert_eq!(p.runtime_dir, Path::new("/h/.local/share/stackhour/bridge"));
        }
    }

    #[test]
    fn ensure_dirs_creates_the_tree_with_a_private_media_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let p = BridgePaths::from_runtime_dir(tmp.path());
        p.ensure_dirs().expect("mkdir");
        assert!(p.jobs_dir.is_dir() && p.results_dir.is_dir() && p.media_dir.is_dir());
        assert!(p.inprogress_dir.is_dir(), "claim renames into inprogress/");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p.media_dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "media/ holds user attachments");
        }
        // Idempotent: installing twice is normal.
        p.ensure_dirs().expect("second mkdir");
    }

    /// The log format is shared with the JS runtime and read by `bridge
    /// doctor`, so it is a contract: ISO-8601 UTC in brackets, then the
    /// message, then a newline.
    #[test]
    fn log_lines_are_timestamped_and_appended() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("coordinator.log");
        log_line(&log, "first");
        log_line(&log, "second thing");
        let body = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        for (line, msg) in lines.iter().zip(["first", "second thing"]) {
            let (ts, rest) = line.split_once("] ").expect("bracketed timestamp");
            assert_eq!(rest, msg);
            let ts = ts.strip_prefix('[').unwrap();
            assert!(ts.ends_with('Z'), "not UTC: {ts}");
            chrono::DateTime::parse_from_rfc3339(ts).expect("ISO-8601 timestamp");
        }
    }

    /// An unwritable log file must never take the bridge down mid-conversation.
    #[test]
    fn a_failing_log_write_is_swallowed() {
        log_line(Path::new("/nonexistent-dir-xyz/coordinator.log"), "still fine");
    }
}
