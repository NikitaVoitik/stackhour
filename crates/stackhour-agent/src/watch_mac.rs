//! `macApps` watcher (darwin-only; gate reason: `requires macOS`).
//!
//! ioreg HID idle seconds via /bin/sh; ONE osascript invocation for the
//! frontmost app name + window title; cfg.agent.apps lookup (string
//! shorthand already normalized at config load); projectFromTitle
//! (configured pattern, else DEFAULT_TITLE_PATTERNS: WebStorm en/em-dash,
//! Zed em-dash). User patterns are compiled with the regex crate —
//! JS-regex-subset caveat documented: incompatible patterns are treated as
//! no-match, never crash. Emits a single 'human' app row.
//!
//! Port of `src/agent/watch-mac.js`.

use crate::{Gate, Watcher};
use serde_json::{json, Value};
use stackhour_core::config::Config;
use stackhour_core::Result;

/// Subprocess-command injection seam so the logic is unit-testable on Linux:
/// (program, args) -> stdout.
pub type CmdRunner = fn(program: &str, args: &[&str]) -> std::io::Result<String>;

/// `DEFAULT_TITLE_PATTERNS` — app name -> title regexes, first match wins.
///
/// These are the shipped extractors for the two editors whose window titles
/// carry the project name. A user `projectFromTitle` REPLACES them rather
/// than adding to them, matching the JS ternary.
const DEFAULT_TITLE_PATTERNS: &[(&str, &[&str])] = &[
    // JetBrains: "project – path/to/file" (en dash).
    ("WebStorm", &[r"^([^–—]+?)\s+[–—]"]),
    // Zed: "filename — project" (em dash).
    ("Zed", &[r"\s+[—]\s+([^—]+)$"]),
];

/// The macOS frontmost-app watcher.
#[derive(Debug, Default)]
pub struct MacWatcher {
    /// None = real subprocess execution.
    pub runner: Option<CmdRunner>,
}

/// The AppleScript that yields "<app>\n<window title>" in one round trip.
const FRONTMOST_SCRIPT: &str = r#"
    tell application "System Events"
      set p to first application process whose frontmost is true
      set appName to name of p
      set winTitle to ""
      try
        set winTitle to name of front window of p
      end try
      return appName & linefeed & winTitle
    end tell"#;

const IDLE_SCRIPT: &str =
    "ioreg -c IOHIDSystem | awk '/HIDIdleTime/ {print int($NF/1000000000); exit}'";

fn real_runner(program: &str, args: &[&str]) -> std::io::Result<String> {
    let out = std::process::Command::new(program).args(args).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Extract the project name from a window title.
///
/// A configured `projectFromTitle` REPLACES the built-in patterns for that
/// app. Patterns are JS regex source compiled by the `regex` crate; the two
/// dialects overlap for everything realistic here, but a pattern using a
/// JS-only construct (lookahead, backreference) fails to compile — that is
/// treated as "no match" rather than an error, so one bad config line cannot
/// take the whole watcher down.
pub fn project_from_title(
    app: &str,
    title: &str,
    app_cfg: &stackhour_core::config::AppCfg,
) -> Option<String> {
    if title.is_empty() {
        return None;
    }
    let owned;
    let patterns: &[&str] = match &app_cfg.project_from_title {
        Some(p) => {
            owned = [p.as_str()];
            &owned
        }
        None => DEFAULT_TITLE_PATTERNS
            .iter()
            .find(|(name, _)| *name == app)
            .map_or(&[][..], |(_, pats)| *pats),
    };
    for pattern in patterns {
        let Ok(re) = regex::Regex::new(pattern) else {
            continue;
        };
        if let Some(m) = re.captures(title).and_then(|c| c.get(1)) {
            let trimmed = m.as_str().trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

impl Watcher for MacWatcher {
    fn name(&self) -> &str {
        "macApps"
    }

    fn gate(&self, cfg: &Config) -> Gate {
        let enabled = cfg.agent.watch.mac_apps;
        let available = enabled && cfg!(target_os = "macos");
        if available {
            Gate::Run
        } else {
            Gate::Skipped {
                enabled,
                available,
                reason: if enabled {
                    "requires macOS".to_string()
                } else {
                    "disabled in config".to_string()
                },
            }
        }
    }

    fn input_marker(&self, _state: &Value) -> Option<String> {
        // Presence is sampled fresh every tick; there is no persisted input
        // to fingerprint.
        None
    }

    fn run(&mut self, cfg: &Config, _state: &mut Value, now: f64) -> Result<Vec<Value>> {
        let run = self.runner.unwrap_or(real_runner);

        // Human-presence gate FIRST: no keystrokes recently means nobody is
        // here, whatever window happens to be frontmost.
        let idle_out = run("/bin/sh", &["-c", IDLE_SCRIPT])
            .map_err(|e| stackhour_core::Error::msg(format!("ioreg failed: {e}")))?;
        // JS: Number(stdout.trim() || 0) — an empty read is zero idle.
        let trimmed = idle_out.trim();
        let idle = if trimmed.is_empty() {
            0.0
        } else {
            stackhour_core::jsnum::js_number(&Value::String(trimmed.to_string()))
        };
        if idle >= cfg.agent.idle_seconds {
            return Ok(Vec::new());
        }

        let front = run("osascript", &["-e", FRONTMOST_SCRIPT])
            .map_err(|e| stackhour_core::Error::msg(format!("osascript failed: {e}")))?;
        let mut parts = front.split('\n');
        let app = parts.next().unwrap_or("").trim().to_string();
        let title = parts.collect::<Vec<_>>().join("\n").trim().to_string();

        // Only apps the user has mapped produce rows: an unmapped app is not
        // "unknown work", it is not work we track at all.
        let Some(mapped) = cfg.agent.apps.get(&app) else {
            return Ok(Vec::new());
        };
        let project = project_from_title(&app, &title, mapped);
        let location = project.unwrap_or_else(|| mapped.source.clone());

        Ok(vec![json!({
            "time": now,
            "source": mapped.source,
            "project": stackhour_core::project::resolve_project(
                &location,
                &cfg.agent.project_aliases,
                Some(&location),
            ),
            "entity": if title.is_empty() { app.clone() } else { title },
            "entity_type": "app",
            "category": mapped.category,
            "actor": "human",
            "is_write": 0,
        })])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stackhour_core::config::AppCfg;

    fn app_cfg(pattern: Option<&str>) -> AppCfg {
        AppCfg {
            source: "webstorm".into(),
            category: "coding".into(),
            project_from_title: pattern.map(str::to_string),
        }
    }

    /// The shipped title extractors are a user-visible contract: changing
    /// them silently re-attributes everyone's editor time.
    #[test]
    fn default_title_patterns_stay_stable() {
        assert_eq!(
            project_from_title("WebStorm", "stackhour – src/agent/index.js", &app_cfg(None)),
            Some("stackhour".to_string())
        );
        assert_eq!(
            project_from_title("Zed", "lib.rs — stackhour", &app_cfg(None)),
            Some("stackhour".to_string())
        );
        // An app with no shipped pattern extracts nothing.
        assert_eq!(
            project_from_title("Safari", "some page - a site", &app_cfg(None)),
            None
        );
        // An empty title never matches.
        assert_eq!(project_from_title("Zed", "", &app_cfg(None)), None);
    }

    /// A configured pattern REPLACES the built-in one for that app.
    #[test]
    fn a_configured_pattern_overrides_the_default() {
        let cfg = app_cfg(Some(r"\[(\w+)\]"));
        assert_eq!(
            project_from_title("WebStorm", "[myproj] – file.js", &cfg),
            Some("myproj".to_string())
        );
        // The default WebStorm pattern would have matched here; it must not
        // be consulted at all once an override exists.
        assert_eq!(project_from_title("WebStorm", "plain – title", &cfg), None);
    }

    /// A pattern the regex crate cannot compile (a JS-only construct) is a
    /// no-match, never a panic or an error that kills the watcher.
    #[test]
    fn an_incompatible_pattern_is_a_no_match_not_a_crash() {
        let cfg = app_cfg(Some(r"(?=lookahead)(\w+)"));
        assert_eq!(project_from_title("WebStorm", "anything", &cfg), None);
    }

    /// Gating: off in config vs unavailable on this platform are different
    /// reasons, and doctor renders both.
    #[test]
    fn gate_distinguishes_disabled_from_unsupported_platform() {
        let off = crate::test_config(json!({"agent": {"watch": {"macApps": false}}}));
        assert_eq!(
            MacWatcher::default().gate(&off),
            Gate::Skipped {
                enabled: false,
                available: false,
                reason: "disabled in config".to_string()
            }
        );

        let on = crate::test_config(json!({"agent": {"watch": {"macApps": true}}}));
        let gate = MacWatcher::default().gate(&on);
        if cfg!(target_os = "macos") {
            assert_eq!(gate, Gate::Run);
        } else {
            assert_eq!(
                gate,
                Gate::Skipped {
                    enabled: true,
                    available: false,
                    reason: "requires macOS".to_string()
                }
            );
        }
    }

    /// An idle machine emits nothing: this watcher is the HUMAN presence
    /// signal, so a frontmost window with nobody typing is not activity.
    #[test]
    fn an_idle_machine_emits_nothing() {
        fn idle_runner(program: &str, _args: &[&str]) -> std::io::Result<String> {
            assert_eq!(program, "/bin/sh", "osascript must not even be called");
            Ok("900\n".to_string())
        }
        let cfg = crate::test_config(json!({"agent": {"idleSeconds": 300}}));
        let mut w = MacWatcher {
            runner: Some(idle_runner),
        };
        assert!(w.run(&cfg, &mut json!({}), 100.0).unwrap().is_empty());
    }

    /// The active path: an active machine on a mapped app emits one human row
    /// carrying the title-derived project.
    #[test]
    fn an_active_mapped_app_emits_one_human_row() {
        fn runner(program: &str, _args: &[&str]) -> std::io::Result<String> {
            Ok(match program {
                "/bin/sh" => "3\n".to_string(),
                _ => "Zed\nlib.rs — stackhour\n".to_string(),
            })
        }
        let cfg = crate::test_config(json!({
            "agent": {"idleSeconds": 300, "apps": {"Zed": {"source": "zed", "category": "coding"}}}
        }));
        let mut w = MacWatcher {
            runner: Some(runner),
        };
        let rows = w.run(&cfg, &mut json!({}), 1234.0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["actor"], "human");
        assert_eq!(rows[0]["source"], "zed");
        assert_eq!(rows[0]["project"], "stackhour");
        assert_eq!(rows[0]["entity"], "lib.rs — stackhour");
        assert_eq!(rows[0]["entity_type"], "app");
        assert_eq!(rows[0]["time"], 1234.0);
    }

    /// An app the user has not mapped is not tracked at all.
    #[test]
    fn an_unmapped_frontmost_app_emits_nothing() {
        fn runner(program: &str, _args: &[&str]) -> std::io::Result<String> {
            Ok(match program {
                "/bin/sh" => "0\n".to_string(),
                _ => "Slack\nsome channel\n".to_string(),
            })
        }
        let cfg = crate::test_config(json!({"agent": {"idleSeconds": 300}}));
        let mut w = MacWatcher {
            runner: Some(runner),
        };
        assert!(w.run(&cfg, &mut json!({}), 1.0).unwrap().is_empty());
    }
}
