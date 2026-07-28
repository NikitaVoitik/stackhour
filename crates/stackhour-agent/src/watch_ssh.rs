//! `ssh` watcher (linux-only; gate reason: `requires Linux`).
//!
//! Numeric /dev/pts entries; atime idle gate; `ps -t pts/N -o pid=,stat=`
//! preferring the foreground '+' process, else the last; /proc/<pid>/cwd
//! readlink; one 'ssh'-source human row per active pty.
//!
//! Port of `src/agent/watch-ssh.js`.

use crate::{Gate, Watcher};
use serde_json::{json, Value};
use stackhour_core::config::Config;
use stackhour_core::Result;
use std::path::{Path, PathBuf};

/// The Linux ssh/pty watcher.
#[derive(Debug, Default)]
pub struct SshWatcher {
    /// Overrides `/dev/pts` (tests).
    pub pts_dir: Option<PathBuf>,
    /// Overrides the `ps` + `/proc/<pid>/cwd` lookup (tests). Receives the
    /// `pts/N` name and returns the foreground process's cwd.
    pub cwd_lookup: Option<fn(&str) -> Option<String>>,
}

/// Pick the foreground process on `pts` and read its working directory.
///
/// `ps -o stat=` marks the foreground process group with `+`. That is the
/// shell (or the command it is running), and therefore the thing whose cwd
/// says which project the keystrokes belong to. Falling back to the LAST
/// listed process — not the first — matches the JS and picks the most
/// recently started one when no `+` is present.
fn foreground_cwd(pts: &str) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-t", pts, "-o", "pid=,stat="])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let procs: Vec<(String, bool)> = text
        .trim()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.to_string();
            if pid.is_empty() {
                return None;
            }
            let stat = fields.next().unwrap_or("");
            Some((pid, stat.contains('+')))
        })
        .collect();
    let pick = procs.iter().find(|(_, fg)| *fg).or_else(|| procs.last())?;
    std::fs::read_link(format!("/proc/{}/cwd", pick.0))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Seconds since the pty was last READ from — i.e. since the last keystroke.
fn atime_seconds(path: &Path) -> Option<f64> {
    let md = std::fs::metadata(path).ok()?;
    let atime = std::os::unix::fs::MetadataExt::atime(&md) as f64;
    let nsec = std::os::unix::fs::MetadataExt::atime_nsec(&md) as f64;
    Some(atime + nsec / 1e9)
}

impl Watcher for SshWatcher {
    fn name(&self) -> &'static str {
        "ssh"
    }

    fn gate(&self, cfg: &Config) -> Gate {
        let enabled = cfg.agent.watch.ssh;
        let available = enabled && cfg!(target_os = "linux");
        if available {
            Gate::Run
        } else {
            Gate::Skipped {
                enabled,
                available,
                reason: if enabled {
                    "requires Linux".to_string()
                } else {
                    "disabled in config".to_string()
                },
            }
        }
    }

    fn input_marker(&self, _state: &Value) -> Option<String> {
        None
    }

    fn run(&mut self, cfg: &Config, _state: &mut Value, now: f64) -> Result<Vec<Value>> {
        let pts_dir = self.pts_dir.clone().unwrap_or_else(|| PathBuf::from("/dev/pts"));
        let Ok(entries) = std::fs::read_dir(&pts_dir) else {
            // No /dev/pts (a container without devpts) is not an error.
            return Ok(Vec::new());
        };

        // Sorted so the row order is stable across ticks and filesystems;
        // readdir order is not.
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            // `ptmx` and any other non-numeric entry is not a pty.
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .collect();
        names.sort_by_key(|n| n.parse::<u64>().unwrap_or(u64::MAX));

        let lookup = self.cwd_lookup.unwrap_or(foreground_cwd);
        let mut rows = Vec::new();
        for name in names {
            let Some(atime) = atime_seconds(&pts_dir.join(&name)) else {
                continue;
            };
            // A pty's atime advances when the shell READS input, which is the
            // same signal `w` uses for idle. This is what makes "typing over
            // SSH" count even when nothing is saved.
            if now - atime >= cfg.agent.idle_seconds {
                continue;
            }
            let cwd = lookup(&format!("pts/{name}")).unwrap_or_else(|| "unknown".to_string());
            rows.push(json!({
                "time": now,
                "source": "ssh",
                "project": stackhour_core::project::resolve_project(
                    &cwd,
                    &cfg.agent.project_aliases,
                    None,
                ),
                "entity": cwd,
                "entity_type": "app",
                "category": "coding",
                "actor": "human",
                "is_write": 0,
            }));
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Set a file's atime to `secs` since the epoch.
    fn set_atime(path: &Path, secs: i64) {
        filetime::set_file_atime(path, filetime::FileTime::from_unix_time(secs, 0))
            .unwrap_or_else(|error| panic!("cannot set atime for {path:?}: {error}"));
    }

    #[allow(clippy::unnecessary_wraps)] // Matches the injected cwd lookup signature.
    fn fake_cwd(pts: &str) -> Option<String> {
        Some(format!("/work/{}", pts.replace('/', "-")))
    }

    #[test]
    fn gate_distinguishes_disabled_from_unsupported_platform() {
        let off = crate::test_config(json!({"agent": {"watch": {"ssh": false}}}));
        assert_eq!(
            SshWatcher::default().gate(&off),
            Gate::Skipped {
                enabled: false,
                available: false,
                reason: "disabled in config".to_string()
            }
        );
        let on = crate::test_config(json!({"agent": {"watch": {"ssh": true}}}));
        let gate = SshWatcher::default().gate(&on);
        if cfg!(target_os = "linux") {
            assert_eq!(gate, Gate::Run);
        } else {
            assert_eq!(
                gate,
                Gate::Skipped {
                    enabled: true,
                    available: false,
                    reason: "requires Linux".to_string()
                }
            );
        }
    }

    /// Only recently-read ptys count, only numeric entries are ptys, and the
    /// cwd of each one's foreground process becomes the project.
    #[test]
    fn only_recently_active_numeric_ptys_produce_rows() {
        let tmp = TempDir::new().unwrap();
        let now = 1_800_000_000_i64;
        for name in ["0", "1", "ptmx", "not-a-pty"] {
            std::fs::write(tmp.path().join(name), "").unwrap();
        }
        // pts/0 was read 5s ago (someone is typing); pts/1 an hour ago.
        set_atime(&tmp.path().join("0"), now - 5);
        set_atime(&tmp.path().join("1"), now - 3600);
        // The non-numeric entries would be "active" if they were considered.
        set_atime(&tmp.path().join("ptmx"), now);
        set_atime(&tmp.path().join("not-a-pty"), now);

        let cfg = crate::test_config(json!({"agent": {"idleSeconds": 300}}));
        let mut w = SshWatcher {
            pts_dir: Some(tmp.path().to_path_buf()),
            cwd_lookup: Some(fake_cwd),
        };
        let rows = w.run(&cfg, &mut json!({}), now as f64).unwrap();
        assert_eq!(rows.len(), 1, "got {rows:#?}");
        assert_eq!(rows[0]["entity"], "/work/pts-0");
        assert_eq!(rows[0]["source"], "ssh");
        assert_eq!(rows[0]["actor"], "human");
        assert_eq!(rows[0]["category"], "coding");
        assert_eq!(rows[0]["is_write"], 0);
    }

    /// A pty whose foreground process cannot be resolved still counts as
    /// presence — the time is real, only the attribution is unknown.
    #[test]
    fn an_unresolvable_cwd_falls_back_to_unknown() {
        fn none(_pts: &str) -> Option<String> {
            None
        }
        let tmp = TempDir::new().unwrap();
        let now = 1_800_000_000_i64;
        std::fs::write(tmp.path().join("3"), "").unwrap();
        set_atime(&tmp.path().join("3"), now);

        let cfg = crate::test_config(json!({"agent": {"idleSeconds": 300}}));
        let mut w = SshWatcher {
            pts_dir: Some(tmp.path().to_path_buf()),
            cwd_lookup: Some(none),
        };
        let rows = w.run(&cfg, &mut json!({}), now as f64).unwrap();
        assert_eq!(rows[0]["entity"], "unknown");
    }

    /// A machine with no devpts mounted is not an error.
    #[test]
    fn a_missing_pts_dir_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let mut w = SshWatcher {
            pts_dir: Some(tmp.path().join("nope")),
            cwd_lookup: Some(fake_cwd),
        };
        assert!(w
            .run(&crate::test_config(json!({})), &mut json!({}), 1.0)
            .unwrap()
            .is_empty());
    }
}
