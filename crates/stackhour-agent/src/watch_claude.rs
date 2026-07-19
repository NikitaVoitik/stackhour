//! `claude` watcher: ~/.claude/projects/**/*.jsonl (depth-4 walk).
//!
//! Cheap size<=offset skip WITHOUT the truncation clamp (quirk kept — this
//! path differs from tail.rs's generic behaviour); 3600s past window with no
//! future bound; entrypoint -> source mapping; isHumanPrompt rules
//! (tool_result content excludes, isSidechain excludes); per-message-id
//! max-based token dedup with >=0-floored deltas and the 20000-entry
//! insertion-order cap; tokenFields attached to the FIRST tool_use file row
//! only, else the fallback app row; /edit|write/i tool-name -> is_write;
//! pruneOffsets.
//!
//! Port of `src/agent/watch-claude.js`.

use crate::tail;
use crate::{Gate, Watcher};
use serde_json::{json, Map, Value};
use stackhour_core::config::Config;
use stackhour_core::pricing::{cost_of, Usage};
use stackhour_core::Result;
use std::collections::HashSet;
use std::path::PathBuf;

/// Lines older than this are replays or catch-up, not activity.
const RECENT_WINDOW_S: f64 = 3600.0;

/// The four usage counters, in the order the JS object literal declares them
/// (which is the order the delta/max maps iterate).
const USAGE_KEYS: [(&str, &str); 4] = [
    ("input", "input_tokens"),
    ("cacheWrite", "cache_creation_input_tokens"),
    ("cacheRead", "cache_read_input_tokens"),
    ("output", "output_tokens"),
];

/// The Claude Code session-log watcher.
#[derive(Debug, Default)]
pub struct ClaudeWatcher {
    /// Overrides `~/.claude/projects` (tests, and any future config key).
    pub projects_dir: Option<PathBuf>,
}

/// `~/.claude/projects`.
fn default_projects_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claude")
        .join("projects")
}

/// `usage.<field> || 0` for the four counters.
fn read_usage(usage: &Value) -> [f64; 4] {
    let mut out = [0.0; 4];
    for (i, (_, field)) in USAGE_KEYS.iter().enumerate() {
        out[i] = usage.get(*field).and_then(Value::as_f64).unwrap_or(0.0);
    }
    out
}

/// The previously recorded per-key maximum for a message id.
fn read_previous(usage_by_id: &Map<String, Value>, id: &str) -> [f64; 4] {
    let mut out = [0.0; 4];
    let Some(prev) = usage_by_id.get(id) else {
        return out;
    };
    for (i, (key, _)) in USAGE_KEYS.iter().enumerate() {
        out[i] = prev.get(*key).and_then(Value::as_f64).unwrap_or(0.0);
    }
    out
}

/// A user line is a genuine human prompt only when it is not a tool_result
/// relay and not inside a subagent sidechain.
///
/// This is the boundary between "Nikita typed something" and "the model is
/// working", so both exclusions matter: a tool_result block is the harness
/// feeding output back in, and a sidechain is a subagent's own conversation.
fn is_human_prompt(line: &Value) -> bool {
    if line.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if line
        .get("isSidechain")
        .is_some_and(stackhour_core::jsnum::js_truthy)
    {
        return false;
    }
    let content = line.pointer("/message/content");
    match content {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => {
            let has_text = blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("text"));
            let has_tool_result = blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
            has_text && !has_tool_result
        }
        _ => false,
    }
}

impl Watcher for ClaudeWatcher {
    fn name(&self) -> &str {
        "claude"
    }

    fn gate(&self, cfg: &Config) -> Gate {
        if cfg.agent.watch.claude {
            Gate::Run
        } else {
            Gate::Skipped {
                enabled: false,
                available: false,
                reason: "disabled in config".to_string(),
            }
        }
    }

    fn input_marker(&self, state: &Value) -> Option<String> {
        // JSON.stringify(state.claudeOffsets || {}) — a byte-cheap
        // fingerprint of "did any transcript move?".
        Some(
            state
                .get("claudeOffsets")
                .filter(|v| v.is_object())
                .map_or_else(|| "{}".to_string(), Value::to_string),
        )
    }

    fn run(&mut self, cfg: &Config, state: &mut Value, now: f64) -> Result<Vec<Value>> {
        let projects_dir = self.projects_dir.clone().unwrap_or_else(default_projects_dir);
        if !projects_dir.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        tail::walk_files(
            &projects_dir,
            0,
            &|name: &str| name.ends_with(".jsonl"),
            &mut files,
        );

        let mut rows = Vec::new();
        let live: HashSet<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();

        for file in &files {
            // Cheap skip. Note this deliberately does NOT apply tail.rs's
            // truncation clamp: a shrunken transcript is left alone here and
            // only re-based the next time it grows past the stale offset.
            let key = file.to_string_lossy().into_owned();
            let Ok(md) = std::fs::metadata(file) else {
                continue;
            };
            let recorded = crate::state::access::offsets_mut(state, "claudeOffsets")
                .get(&key)
                .and_then(Value::as_u64);
            if recorded.is_some_and(|prev| md.len() <= prev) {
                continue;
            }

            let lines = {
                let offsets = crate::state::access::offsets_mut(state, "claudeOffsets");
                tail::read_new_lines(file, offsets)
            };

            for line in lines {
                let Some(ts) = tail::parse_ts_seconds(line.get("timestamp")) else {
                    continue;
                };
                // Past-only window: a clock-skewed future timestamp is kept,
                // matching the JS (which has no upper bound here).
                if now - ts > RECENT_WINDOW_S {
                    continue;
                }
                let cwd = line
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown")
                    .to_string();
                let source = if line.get("entrypoint").and_then(Value::as_str) == Some("claude-desktop") {
                    "claude-desktop"
                } else {
                    "claude-code"
                };
                let base = json!({
                    "time": ts,
                    "source": source,
                    "project": stackhour_core::project::resolve_project(
                        &cwd,
                        &cfg.agent.project_aliases,
                        None,
                    ),
                    "category": "ai coding",
                    "branch": line.get("gitBranch").cloned().filter(|v| v.is_string()).unwrap_or(Value::Null),
                });

                // --- token accounting -------------------------------------
                // Claude rewrites an assistant line as it streams, so the same
                // message id is seen repeatedly with GROWING counters. We keep
                // the running max per id and charge only the positive delta;
                // without this every partial write would be billed again.
                let usage = line.pointer("/message/usage").filter(|v| v.is_object());
                let usage_id = usage.and(line.pointer("/message/id").map(stackhour_core::jsnum::js_display));
                let mut token_fields: Option<(f64, f64, f64)> = None;
                if let Some(usage) = usage {
                    let current = read_usage(usage);
                    let previous = usage_id
                        .as_deref()
                        .map(|id| read_previous(crate::state::access::usage_by_id_mut(state), id))
                        .unwrap_or([0.0; 4]);
                    let delta: Vec<f64> = current
                        .iter()
                        .zip(previous.iter())
                        .map(|(c, p)| (c - p).max(0.0))
                        .collect();
                    if let Some(id) = usage_id.as_deref() {
                        let maxed: Map<String, Value> = USAGE_KEYS
                            .iter()
                            .enumerate()
                            .map(|(i, (key, _))| ((*key).to_string(), json!(current[i].max(previous[i]))))
                            .collect();
                        let map = crate::state::access::usage_by_id_mut(state);
                        map.insert(id.to_string(), Value::Object(maxed));
                    }
                    if delta.iter().any(|d| *d > 0.0) {
                        let model = line
                            .pointer("/message/model")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        token_fields = Some((
                            delta[0] + delta[1] + delta[2],
                            delta[3],
                            cost_of(
                                model,
                                &Usage {
                                    input: delta[0],
                                    cache_write: delta[1],
                                    cache_read: delta[2],
                                    output: delta[3],
                                },
                                cfg.pricing.as_ref(),
                            ),
                        ));
                    }
                }
                let attach = |row: &mut Value, fields: Option<(f64, f64, f64)>| {
                    if let (Some((tin, tout, cost)), Some(obj)) = (fields, row.as_object_mut()) {
                        obj.insert("tokens_in".into(), json!(tin));
                        obj.insert("tokens_out".into(), json!(tout));
                        obj.insert("cost".into(), json!(cost));
                    }
                };

                if is_human_prompt(&line) {
                    let mut row = base.clone();
                    let obj = row.as_object_mut().expect("base is an object");
                    obj.insert("actor".into(), json!("human"));
                    obj.insert("entity".into(), json!(cwd));
                    obj.insert("entity_type".into(), json!("app"));
                    obj.insert("is_write".into(), json!(0));
                    rows.push(row);
                    continue;
                }

                // Everything else — assistant output, tool use, tool results,
                // sidechains — is the agent working, and accrues even when
                // nobody is at the keyboard.
                let mut emitted = false;
                if let Some(Value::Array(blocks)) = line.pointer("/message/content") {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                            continue;
                        }
                        let Some(fp) = block
                            .pointer("/input/file_path")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        else {
                            continue;
                        };
                        let tool = block.get("name").and_then(Value::as_str).unwrap_or("");
                        let lowered = tool.to_lowercase();
                        let mut row = base.clone();
                        {
                            let obj = row.as_object_mut().expect("base is an object");
                            obj.insert("actor".into(), json!("agent"));
                            obj.insert("entity".into(), json!(fp));
                            obj.insert("entity_type".into(), json!("file"));
                            obj.insert(
                                "is_write".into(),
                                json!(i32::from(lowered.contains("edit") || lowered.contains("write"))),
                            );
                        }
                        // Tokens ride on the FIRST file row only, so a turn
                        // that touched five files is not billed five times.
                        if !emitted {
                            attach(&mut row, token_fields);
                        }
                        rows.push(row);
                        emitted = true;
                    }
                }
                let kind = line.get("type").and_then(Value::as_str).unwrap_or("");
                if !emitted && (kind == "user" || kind == "assistant") {
                    let mut row = base;
                    {
                        let obj = row.as_object_mut().expect("base is an object");
                        obj.insert("actor".into(), json!("agent"));
                        obj.insert("entity".into(), json!(cwd));
                        obj.insert("entity_type".into(), json!("app"));
                        obj.insert("is_write".into(), json!(0));
                    }
                    attach(&mut row, token_fields);
                    rows.push(row);
                }
            }
        }

        crate::state::access::trim_usage_by_id(state);
        let offsets = crate::state::access::offsets_mut(state, "claudeOffsets");
        tail::prune_offsets(offsets, &live, tail::DEFAULT_PRUNE_MAX);
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cfg() -> Config {
        crate::test_config(json!({}))
    }

    fn write(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn append(p: &std::path::Path, body: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    /// `now` and a matching ISO timestamp for a line that is inside the
    /// recent window.
    fn recent() -> (f64, String) {
        let now = 1_800_000_000.0_f64;
        let iso = chrono::DateTime::from_timestamp(now as i64 - 5, 0)
            .unwrap()
            .to_rfc3339();
        (now, iso)
    }

    /// The headline case: new records are tailed, a human prompt and the
    /// agent's own work are distinguished, a tool_result relay is NOT a human
    /// prompt, tokens are charged once, and stale lines are dropped.
    #[test]
    fn tails_new_records_and_preserves_human_relay_edit_token_and_stale_semantics() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path().join("projects");
        let (now, ts) = recent();
        let file = write(&projects.join("proj"), "s.jsonl", "");

        let mut w = ClaudeWatcher {
            projects_dir: Some(projects.clone()),
        };
        let mut state = json!({});
        let cfg = cfg();
        // First sight: offset jumps to EOF, nothing emitted.
        assert!(w.run(&cfg, &mut state, now).unwrap().is_empty());

        let stale = chrono::DateTime::from_timestamp(now as i64 - 7200, 0)
            .unwrap()
            .to_rfc3339();
        append(
            &file,
            &format!(
                "{}\n{}\n{}\n{}\n{}\n",
                json!({"type":"user","timestamp":ts,"cwd":"/w/p","gitBranch":"main",
                       "message":{"content":"hello there"}}),
                json!({"type":"user","timestamp":ts,"cwd":"/w/p",
                       "message":{"content":[{"type":"tool_result","content":"ok"},{"type":"text","text":"x"}]}}),
                json!({"type":"assistant","timestamp":ts,"cwd":"/w/p",
                       "message":{"id":"m1","model":"claude-sonnet-4",
                                  "usage":{"input_tokens":100,"output_tokens":20},
                                  "content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/w/p/a.rs"}},
                                             {"type":"tool_use","name":"Read","input":{"file_path":"/w/p/b.rs"}}]}}),
                json!({"type":"assistant","timestamp":stale,"cwd":"/w/p",
                       "message":{"content":[{"type":"text","text":"too old"}]}}),
                json!({"type":"summary","timestamp":ts,"cwd":"/w/p"}),
            ),
        );

        let rows = w.run(&cfg, &mut state, now).unwrap();
        assert_eq!(rows.len(), 4, "got {rows:#?}");

        // 1: a genuine human prompt.
        assert_eq!(rows[0]["actor"], "human");
        assert_eq!(rows[0]["entity"], "/w/p");
        assert_eq!(rows[0]["source"], "claude-code");
        assert_eq!(rows[0]["branch"], "main");
        assert_eq!(rows[0]["category"], "ai coding");

        // 2: a tool_result relay is the agent, not a human prompt.
        assert_eq!(rows[1]["actor"], "agent");
        assert_eq!(rows[1]["entity_type"], "app");

        // 3+4: file rows, is_write from the tool name, tokens on the FIRST only.
        assert_eq!(rows[2]["entity"], "/w/p/a.rs");
        assert_eq!(rows[2]["entity_type"], "file");
        assert_eq!(rows[2]["is_write"], 1);
        assert_eq!(rows[2]["tokens_in"], 100.0);
        assert_eq!(rows[2]["tokens_out"], 20.0);
        assert!(rows[2]["cost"].as_f64().unwrap() > 0.0);
        assert_eq!(rows[3]["entity"], "/w/p/b.rs");
        assert_eq!(rows[3]["is_write"], 0);
        assert!(rows[3].get("tokens_in").is_none(), "double-billed a turn");

        // The stale line and the unknown `summary` type produced nothing.
    }

    /// Streaming rewrites the same message id with growing counters. Only the
    /// positive delta may be charged, or a long turn is billed many times.
    #[test]
    fn per_message_id_token_dedup_charges_only_the_growth() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path().join("projects");
        let (now, ts) = recent();
        let file = write(&projects.join("p"), "s.jsonl", "");
        let mut w = ClaudeWatcher {
            projects_dir: Some(projects),
        };
        let mut state = json!({});
        let cfg = cfg();
        w.run(&cfg, &mut state, now).unwrap();

        let line = |input: u64, output: u64| {
            json!({"type":"assistant","timestamp":ts,"cwd":"/w/p",
                   "message":{"id":"m1","model":"claude-sonnet-4",
                              "usage":{"input_tokens":input,"output_tokens":output},
                              "content":[{"type":"text","text":"…"}]}})
            .to_string()
        };

        append(&file, &format!("{}\n", line(100, 10)));
        let rows = w.run(&cfg, &mut state, now).unwrap();
        assert_eq!(rows[0]["tokens_in"], 100.0);
        assert_eq!(rows[0]["tokens_out"], 10.0);

        // Same id, grown: charge the delta only.
        append(&file, &format!("{}\n", line(100, 35)));
        let rows = w.run(&cfg, &mut state, now).unwrap();
        assert_eq!(rows[0]["tokens_in"], 0.0);
        assert_eq!(rows[0]["tokens_out"], 25.0);

        // Same id, a SHRUNKEN counter (a retry re-reporting less) must floor
        // at zero rather than credit tokens back.
        append(&file, &format!("{}\n", line(50, 5)));
        let rows = w.run(&cfg, &mut state, now).unwrap();
        assert!(
            rows[0].get("tokens_in").is_none(),
            "a shrinking counter must charge nothing, got {:#?}",
            rows[0]
        );
        assert_eq!(
            state["claudeUsageById"]["m1"]["output"], 35.0,
            "the running max must not regress"
        );
    }

    /// A sidechain user line is a subagent talking to itself, never a human
    /// prompt.
    #[test]
    fn sidechain_user_lines_are_agent_activity() {
        let line = json!({"type":"user","isSidechain":true,
                          "message":{"content":"pretend prompt"}});
        assert!(!is_human_prompt(&line));
        let line = json!({"type":"user","message":{"content":"real prompt"}});
        assert!(is_human_prompt(&line));
    }

    /// `entrypoint` selects the source label, which is how desktop-hosted
    /// sessions are told apart from the CLI.
    #[test]
    fn desktop_entrypoint_maps_to_its_own_source() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path().join("projects");
        let (now, ts) = recent();
        let file = write(&projects.join("p"), "s.jsonl", "");
        let mut w = ClaudeWatcher {
            projects_dir: Some(projects),
        };
        let mut state = json!({});
        let cfg = cfg();
        w.run(&cfg, &mut state, now).unwrap();
        append(
            &file,
            &format!(
                "{}\n",
                json!({"type":"user","timestamp":ts,"cwd":"/w/p",
                       "entrypoint":"claude-desktop","message":{"content":"hi"}})
            ),
        );
        let rows = w.run(&cfg, &mut state, now).unwrap();
        assert_eq!(rows[0]["source"], "claude-desktop");
    }

    /// The watcher must be gated off by config, and reported as such.
    #[test]
    fn gate_reflects_the_config_toggle() {
        let on = cfg();
        assert_eq!(ClaudeWatcher::default().gate(&on), Gate::Run);
        let off = crate::test_config(json!({ "agent": {"watch": {"claude": false}} }));
        assert_eq!(
            ClaudeWatcher::default().gate(&off),
            Gate::Skipped {
                enabled: false,
                available: false,
                reason: "disabled in config".to_string()
            }
        );
    }

    /// A missing `~/.claude/projects` is the normal state on a machine that
    /// does not run Claude Code: no error, no rows.
    #[test]
    fn a_missing_projects_dir_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let mut w = ClaudeWatcher {
            projects_dir: Some(tmp.path().join("nope")),
        };
        assert!(w.run(&cfg(), &mut json!({}), 1_800_000_000.0).unwrap().is_empty());
    }
}
