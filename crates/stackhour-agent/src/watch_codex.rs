//! `codex` watcher: ~/.codex/sessions/**/rollout-*.jsonl.
//!
//! codexMeta head-line protocol (read_first_json_line None swallowed,
//! retried while cwd/source are still unknown); sourceFromOriginator
//! mapping; turn_context cwd/model updates; token_count math (input as-is,
//! output+reasoning summed into tokens_out, cost computed with the cached
//! split subtracted from input); changes / patch.changes -> per-file
//! is_write rows WITHOUT token fields; event_msg user_message -> human row;
//! NO branch key on rows; pruneOffsets applied to BOTH the offsets and
//! codexMeta maps.
//!
//! Port of `src/agent/watch-codex.js`.

use crate::tail;
use crate::{Gate, Watcher};
use serde_json::{json, Map, Value};
use stackhour_core::config::Config;
use stackhour_core::pricing::{cost_of, Usage};
use stackhour_core::Result;
use std::collections::HashSet;
use std::path::PathBuf;

const RECENT_WINDOW_S: f64 = 3600.0;

/// The Codex rollout watcher.
#[derive(Debug, Default)]
pub struct CodexWatcher {
    /// Overrides `~/.codex/sessions`.
    pub sessions_dir: Option<PathBuf>,
}

fn default_sessions_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".codex")
        .join("sessions")
}

/// One store backs the CLI, the IDE extension and the desktop app; the
/// `originator` string is the only thing that tells them apart.
fn source_from_originator(originator: Option<&Value>) -> &'static str {
    let raw = originator.map_or(String::new(), stackhour_core::jsnum::js_display);
    let o = raw.to_lowercase();
    if o.contains("desktop") {
        "codex-desktop"
    } else if o.contains("vscode") || o.contains("ide") {
        "codex-ide"
    } else {
        "codex-cli"
    }
}

/// `line.payload || line` — rollout lines wrap their body, older ones don't.
fn payload(line: &Value) -> &Value {
    line.get("payload")
        .filter(|v| stackhour_core::jsnum::js_truthy(v))
        .unwrap_or(line)
}

/// Non-empty string field, else `None` (JS `payload.cwd || meta.cwd`).
fn truthy_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Watcher for CodexWatcher {
    fn name(&self) -> &str {
        "codex"
    }

    fn gate(&self, cfg: &Config) -> Gate {
        if cfg.agent.watch.codex {
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
        Some(
            state
                .get("codexOffsets")
                .filter(|v| v.is_object())
                .map_or_else(|| "{}".to_string(), Value::to_string),
        )
    }

    fn run(&mut self, cfg: &Config, state: &mut Value, now: f64) -> Result<Vec<Value>> {
        let sessions_dir = self
            .sessions_dir
            .clone()
            .unwrap_or_else(default_sessions_dir);
        if !sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        tail::walk_files(
            &sessions_dir,
            0,
            &|name: &str| name.starts_with("rollout-") && name.ends_with(".jsonl"),
            &mut files,
        );
        let live: HashSet<String> = files
            .iter()
            .map(|f| f.to_string_lossy().into_owned())
            .collect();

        let mut rows = Vec::new();
        for file in &files {
            let key = file.to_string_lossy().into_owned();
            let Ok(md) = std::fs::metadata(file) else {
                continue;
            };
            let recorded = crate::state::access::offsets_mut(state, "codexOffsets")
                .get(&key)
                .and_then(Value::as_u64);
            let first_sight = recorded.is_none();
            if let Some(prev) = recorded {
                if md.len() <= prev {
                    continue;
                }
            }

            // Working copy of this file's learned metadata; written back at
            // the end of the file so the borrow checker and the JS `meta`
            // alias agree.
            let mut meta: Map<String, Value> = crate::state::access::codex_meta_mut(state)
                .get(&key)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();

            // Read the head for session metadata, RETRYING on later ticks when
            // first sight caught only a partial first line. Without the retry
            // a session opened mid-write is attributed to `unknown` forever.
            let need_meta = first_sight || !meta.contains_key("cwd") || !meta.contains_key("source");
            if need_meta && md.len() > 0 {
                if let Some(first) = tail::read_first_json_line(file, tail::MAX_HEAD_LINE) {
                    let body = payload(&first).clone();
                    let is_session_meta =
                        first.get("type").and_then(Value::as_str) == Some("session_meta");
                    if is_session_meta || truthy_str(&body, "cwd").is_some() {
                        if let Some(cwd) = truthy_str(&body, "cwd") {
                            meta.insert("cwd".into(), json!(cwd));
                        }
                        meta.insert(
                            "source".into(),
                            json!(source_from_originator(body.get("originator"))),
                        );
                    }
                }
            }

            let lines = {
                let offsets = crate::state::access::offsets_mut(state, "codexOffsets");
                tail::read_new_lines(file, offsets)
            };

            for line in lines {
                let body = payload(&line).clone();
                match line.get("type").and_then(Value::as_str) {
                    Some("session_meta") => {
                        if let Some(cwd) = truthy_str(&body, "cwd") {
                            meta.insert("cwd".into(), json!(cwd));
                        }
                        meta.insert(
                            "source".into(),
                            json!(source_from_originator(body.get("originator"))),
                        );
                        continue;
                    }
                    Some("turn_context") => {
                        if let Some(cwd) = truthy_str(&body, "cwd") {
                            meta.insert("cwd".into(), json!(cwd));
                        }
                        if let Some(model) = truthy_str(&body, "model") {
                            meta.insert("model".into(), json!(model));
                        }
                        continue;
                    }
                    _ => {}
                }

                let Some(ts) = tail::parse_ts_seconds(line.get("timestamp")) else {
                    continue;
                };
                if now - ts > RECENT_WINDOW_S {
                    continue;
                }
                let kind = line.get("type").and_then(Value::as_str).unwrap_or("");
                if kind != "response_item" && kind != "event_msg" {
                    continue;
                }

                let cwd = meta
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown")
                    .to_string();
                let base = json!({
                    "time": ts,
                    "source": meta.get("source").and_then(Value::as_str).unwrap_or("codex-cli"),
                    "project": stackhour_core::project::resolve_project(
                        &cwd,
                        &cfg.agent.project_aliases,
                        None,
                    ),
                    "category": "ai coding",
                });

                // Per-turn usage rides on token_count events. `last_token_usage`
                // is the turn; the delta form is the fallback for older Codex.
                let token_fields = (body.get("type").and_then(Value::as_str)
                    == Some("token_count"))
                .then(|| {
                    body.pointer("/info/last_token_usage")
                        .filter(|v| v.is_object())
                        .or_else(|| {
                            body.pointer("/info/total_token_usage_delta")
                                .filter(|v| v.is_object())
                        })
                })
                .flatten()
                .map(|tu| {
                    let n = |k: &str| tu.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                    let output = n("output_tokens") + n("reasoning_output_tokens");
                    let cached = n("cached_input_tokens");
                    (
                        n("input_tokens"),
                        output,
                        cost_of(
                            meta.get("model").and_then(Value::as_str).unwrap_or("gpt-5"),
                            &Usage {
                                // Cached input is billed at the cache-read
                                // rate, so it must come OUT of `input`.
                                input: (n("input_tokens") - cached).max(0.0),
                                cache_read: cached,
                                cache_write: 0.0,
                                output,
                            },
                            cfg.pricing.as_ref(),
                        ),
                    )
                });

                let is_human_prompt = kind == "event_msg"
                    && body.get("type").and_then(Value::as_str) == Some("user_message");

                // A patch event names the files it touched; prefer those over
                // a single opaque app row.
                let changes = body
                    .get("changes")
                    .or_else(|| body.pointer("/patch/changes"))
                    .and_then(Value::as_object);
                if let Some(changes) = changes {
                    for fp in changes.keys() {
                        let mut row = base.clone();
                        let obj = row.as_object_mut().expect("base is an object");
                        obj.insert("actor".into(), json!("agent"));
                        obj.insert("entity".into(), json!(fp));
                        obj.insert("entity_type".into(), json!("file"));
                        obj.insert("is_write".into(), json!(1));
                        rows.push(row);
                    }
                } else {
                    let mut row = base;
                    let obj = row.as_object_mut().expect("base is an object");
                    obj.insert(
                        "actor".into(),
                        json!(if is_human_prompt { "human" } else { "agent" }),
                    );
                    obj.insert("entity".into(), json!(cwd));
                    obj.insert("entity_type".into(), json!("app"));
                    obj.insert("is_write".into(), json!(0));
                    if let Some((tin, tout, cost)) = token_fields {
                        obj.insert("tokens_in".into(), json!(tin));
                        obj.insert("tokens_out".into(), json!(tout));
                        obj.insert("cost".into(), json!(cost));
                    }
                    rows.push(row);
                }
            }

            crate::state::access::codex_meta_mut(state).insert(key, Value::Object(meta));
        }

        tail::prune_offsets(
            crate::state::access::offsets_mut(state, "codexOffsets"),
            &live,
            tail::DEFAULT_PRUNE_MAX,
        );
        tail::prune_offsets(
            crate::state::access::codex_meta_mut(state),
            &live,
            tail::DEFAULT_PRUNE_MAX,
        );
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn recent() -> (f64, String) {
        let now = 1_800_000_000.0_f64;
        let iso = chrono::DateTime::from_timestamp(now as i64 - 5, 0)
            .unwrap()
            .to_rfc3339();
        (now, iso)
    }

    fn append(p: &std::path::Path, body: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn originator_maps_to_the_three_sources() {
        assert_eq!(
            source_from_originator(Some(&json!("Codex Desktop"))),
            "codex-desktop"
        );
        assert_eq!(source_from_originator(Some(&json!("vscode"))), "codex-ide");
        assert_eq!(source_from_originator(Some(&json!("some-IDE"))), "codex-ide");
        assert_eq!(
            source_from_originator(Some(&json!("codex_cli_rs"))),
            "codex-cli"
        );
        assert_eq!(source_from_originator(None), "codex-cli");
    }

    /// The headline case: metadata is learned from the head line, prompts and
    /// agent work are classified, token_count events are priced with cached
    /// input moved to the cache-read rate, and a patch becomes file rows.
    #[test]
    fn learns_metadata_then_classifies_prompts_token_events_and_patch_files() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions").join("2026").join("07");
        std::fs::create_dir_all(&sessions).unwrap();
        let file = sessions.join("rollout-1.jsonl");
        let (now, ts) = recent();
        append(
            &file,
            &format!(
                "{}\n",
                json!({"type":"session_meta","timestamp":ts,
                       "payload":{"cwd":"/w/proj","originator":"Codex Desktop"}})
            ),
        );

        let mut w = CodexWatcher {
            sessions_dir: Some(tmp.path().join("sessions")),
        };
        let mut state = json!({});
        let cfg = crate::test_config(json!({}));
        // First sight: head is read for metadata, but nothing is emitted.
        assert!(w.run(&cfg, &mut state, now).unwrap().is_empty());
        let key = file.to_string_lossy().into_owned();
        assert_eq!(state["codexMeta"][&key]["cwd"], "/w/proj");
        assert_eq!(state["codexMeta"][&key]["source"], "codex-desktop");

        append(
            &file,
            &format!(
                "{}\n{}\n{}\n{}\n{}\n",
                json!({"type":"turn_context","timestamp":ts,"payload":{"model":"gpt-5"}}),
                json!({"type":"event_msg","timestamp":ts,"payload":{"type":"user_message"}}),
                json!({"type":"response_item","timestamp":ts,"payload":{"type":"message"}}),
                json!({"type":"event_msg","timestamp":ts,"payload":{"type":"token_count",
                       "info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":800,
                                                   "output_tokens":50,"reasoning_output_tokens":25}}}}),
                json!({"type":"response_item","timestamp":ts,"payload":{"type":"patch_apply",
                       "changes":{"/w/proj/a.rs":{},"/w/proj/b.rs":{}}}}),
            ),
        );

        let rows = w.run(&cfg, &mut state, now).unwrap();
        assert_eq!(rows.len(), 5, "got {rows:#?}");

        assert_eq!(rows[0]["actor"], "human");
        assert_eq!(rows[0]["source"], "codex-desktop");
        assert_eq!(rows[0]["entity"], "/w/proj");
        assert_eq!(rows[1]["actor"], "agent");

        assert_eq!(rows[2]["tokens_in"], 1000.0);
        assert_eq!(rows[2]["tokens_out"], 75.0);
        // 200 uncached input + 800 cached + 75 output, priced apart.
        let expected = cost_of(
            "gpt-5",
            &Usage {
                input: 200.0,
                cache_read: 800.0,
                cache_write: 0.0,
                output: 75.0,
            },
            None,
        );
        assert_eq!(rows[2]["cost"].as_f64().unwrap(), expected);

        let mut files: Vec<&str> = rows[3..]
            .iter()
            .map(|r| r["entity"].as_str().unwrap())
            .collect();
        files.sort_unstable();
        assert_eq!(files, ["/w/proj/a.rs", "/w/proj/b.rs"]);
        assert_eq!(rows[3]["is_write"], 1);
        assert_eq!(rows[3]["entity_type"], "file");
        assert!(
            rows[3].get("tokens_in").is_none(),
            "patch file rows never carry tokens"
        );
    }

    /// Regression: a session whose head line was only half-written on first
    /// sight must be retried, not attributed to `unknown` for its lifetime.
    #[test]
    fn session_metadata_is_retried_when_first_sight_saw_a_partial_head() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let file = sessions.join("rollout-1.jsonl");
        let (now, ts) = recent();
        // A truncated head line: no newline, unparseable.
        append(&file, "{\"type\":\"session_meta\",\"payl");

        let mut w = CodexWatcher {
            sessions_dir: Some(sessions),
        };
        let mut state = json!({});
        let cfg = crate::test_config(json!({}));
        w.run(&cfg, &mut state, now).unwrap();
        let key = file.to_string_lossy().into_owned();
        assert!(
            state["codexMeta"][&key].get("cwd").is_none(),
            "nothing should have been learned from a partial head"
        );

        // The writer finishes the line and appends a real event.
        append(
            &file,
            &format!(
                "oad\":{{\"cwd\":\"/w/late\",\"originator\":\"codex_cli_rs\"}}}}\n{}\n",
                json!({"type":"response_item","timestamp":ts,"payload":{"type":"message"}})
            ),
        );
        let rows = w.run(&cfg, &mut state, now).unwrap();
        assert_eq!(state["codexMeta"][&key]["cwd"], "/w/late");
        assert_eq!(rows[0]["entity"], "/w/late", "got {rows:#?}");
        assert_eq!(rows[0]["source"], "codex-cli");
    }

    #[test]
    fn gate_reflects_the_config_toggle() {
        assert_eq!(
            CodexWatcher::default().gate(&crate::test_config(json!({}))),
            Gate::Run
        );
        assert_eq!(
            CodexWatcher::default().gate(&crate::test_config(
                json!({"agent": {"watch": {"codex": false}}})
            )),
            Gate::Skipped {
                enabled: false,
                available: false,
                reason: "disabled in config".to_string()
            }
        );
    }

    #[test]
    fn a_missing_sessions_dir_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let mut w = CodexWatcher {
            sessions_dir: Some(tmp.path().join("nope")),
        };
        assert!(w
            .run(&crate::test_config(json!({})), &mut json!({}), 1.0)
            .unwrap()
            .is_empty());
    }
}
