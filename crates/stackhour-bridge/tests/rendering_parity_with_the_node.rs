//! Differential rendering parity against the retired Node coordinator.
//!
//! SAFETY: every Telegram call here goes to the in-process mock in
//! `common/mock_bot_api.rs`. Nothing in this file may ever be pointed at the
//! real API, and no token is read.
//!
//! How the golden was produced (historical): a Node harness read the live
//! `coordinator.mjs` as TEXT — it never imported it, which would have started
//! a second long-poller — sliced out the transport wrappers and the
//! "rendering + tables" section, and evaluated that real source with only
//! `tg()` stubbed. The recorded payload sequence is
//! `tests-fixtures/render-parity/golden.json`, with its input corpus in
//! `cases.json`.
//!
//! Node has since been removed from the repository, so the golden is now a
//! FROZEN reference capture and cannot be regenerated. Treat it as the
//! specification: if this test fails, the Rust renderer changed behaviour, and
//! the golden is the evidence of what the behaviour used to be. Only edit the
//! golden alongside a deliberate, documented rendering change.

// Historical name kept so the git history of this parity work stays greppable.

#[path = "common/mock_bot_api.rs"]
mod mock;

use mock::{MockApi, Reply};
use serde_json::{json, Value};
use stackhour_bridge::local_lane::{deliver_final, LaneContext, LocalTarget};
use stackhour_bridge::telegram::{Tg, TgConfig};
use stackhour_core::registry::{EngineDef, Registry};
use std::sync::Mutex;

const CHAT: i64 = 4242;
const STATUS_ID: i64 = 77;

/// The keyboard the Node harness hands `deliverFinal`. Both sides must attach
/// the same object or the payload diff is meaningless.
fn keyboard() -> Value {
    json!({ "inline_keyboard": [[{ "text": "🆕 New session", "callback_data": "new" }]] })
}

struct Ctx {
    reg: Registry,
    logs: Mutex<Vec<String>>,
}

impl LaneContext for Ctx {
    fn engine(&self, name: &str) -> Option<EngineDef> {
        self.reg.engines.get(name).cloned()
    }
    fn target(&self, _n: &str, _e: &str) -> Option<LocalTarget> {
        None
    }
    fn prompt(&self, name: &str, vars: &[(&str, &str)]) -> String {
        self.reg.prompts.render(name, vars)
    }
    fn control_keyboard(&self) -> Value {
        keyboard()
    }
    fn session(&self, _t: &str, _e: &str, _a: Option<&str>) -> Option<String> {
        None
    }
    fn set_session(&self, _t: &str, _e: &str, _a: Option<&str>, _id: Option<String>) {}
    fn log(&self, line: &str) {
        self.logs.lock().expect("logs").push(line.to_string());
    }
    fn now_ms(&self) -> i64 {
        0
    }
    fn default_target(&self) -> String {
        "gcp".into()
    }
}

fn ctx() -> Ctx {
    Ctx {
        reg: stackhour_core::registry::load_with(
            std::path::Path::new("/nonexistent-stackhour-config"),
            stackhour_core::registry::EnvSource::fixed(&[]),
        ),
        logs: Mutex::new(Vec::new()),
    }
}

fn tg(api: &MockApi) -> Tg {
    let mut cfg = TgConfig::new("test-token", CHAT).with_api_root(api.base.clone());
    cfg.backoff_base_ms = 1;
    cfg.retry_after_slack_secs = 0;
    Tg::with_config(cfg)
}

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests-fixtures/render-parity")
        .join(name);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Drive `deliver_final` for one case and return the recorded payloads in the
/// same `{method, body}` shape the Node harness emits.
fn rust_payloads(text: &str) -> Vec<Value> {
    let api = MockApi::start();
    // A standard Bot API has no sendRichMessage: both attempts 404, matching
    // what the Node harness's stub returns.
    api.push_n(2, Reply::err(404, "Not Found: method not found"));
    api.set_default(Reply::ok(json!({ "message_id": 1 })));

    deliver_final(&tg(&api), &ctx(), text, Some(STATUS_ID));

    api.requests()
        .into_iter()
        .map(|r| json!({ "method": r.method, "body": r.body }))
        .collect()
}

#[test]
fn deliver_final_payloads_match_the_node_coordinator_for_every_case() {
    let cases = fixture("cases.json");
    let golden = fixture("golden.json");
    let cases = cases.as_array().expect("cases array");
    let golden = golden.as_array().expect("golden array");
    assert_eq!(
        cases.len(),
        golden.len(),
        "cases.json and golden.json disagree; they are frozen captures and must stay paired"
    );
    assert!(cases.len() >= 10, "corpus shrank unexpectedly");

    let mut failures: Vec<String> = Vec::new();
    for (case, want) in cases.iter().zip(golden.iter()) {
        let name = case["name"].as_str().expect("case name");
        assert_eq!(name, want["name"].as_str().expect("golden name"));
        let text = case["text"].as_str().expect("case text");

        let got = rust_payloads(text);
        let want_calls = want["calls"].as_array().expect("golden calls");

        if got.len() != want_calls.len() {
            failures.push(format!(
                "{name}: call count {} != node {} (rust: {:?}, node: {:?})",
                got.len(),
                want_calls.len(),
                got.iter().map(|c| c["method"].clone()).collect::<Vec<_>>(),
                want_calls.iter().map(|c| c["method"].clone()).collect::<Vec<_>>(),
            ));
            continue;
        }
        for (i, (g, w)) in got.iter().zip(want_calls.iter()).enumerate() {
            if g != w {
                failures.push(format!("{name}: payload #{i} differs\n  rust: {g}\n  node: {w}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The ASCII table rewrite is the highest-risk piece of the port (column
/// widths, the gutter, the per-row right-trim, the separator row), so it is
/// also asserted on its own, independent of the transport.
#[test]
fn ascii_table_conversion_matches_the_node_exactly() {
    let cases = fixture("cases.json");
    let golden = fixture("golden.json");
    let mut checked = 0usize;
    for (case, want) in cases
        .as_array()
        .expect("cases")
        .iter()
        .zip(golden.as_array().expect("golden").iter())
    {
        let text = case["text"].as_str().expect("text");
        let name = case["name"].as_str().expect("name");
        assert_eq!(
            stackhour_bridge::render::has_table(text),
            want["has_table"].as_bool().expect("has_table"),
            "{name}: hasTable disagrees"
        );
        if let Some(expected) = want["ascii_tables"].as_str() {
            assert_eq!(
                stackhour_bridge::render::rewrite_tables(text),
                expected,
                "{name}: asciiTables output differs"
            );
            checked += 1;
        }
    }
    assert!(checked >= 3, "expected several table cases, saw {checked}");
}
