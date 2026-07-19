//! GET /api/detail — split into its own module (complexity budget).
//!
//! Case-sensitive dimension gate against GROUP_FIELDS -> exact 400 body;
//! value predicate ('' -> NULL, 'unknown' -> NULL-or-literal-'unknown');
//! GLOBAL credits are computed first and THEN filtered (order matters for
//! credit amounts); breakdowns = totals over the remaining dimensions minus
//! the selected one, top-20 each; recent capped at 50.

use crate::{json_response, ApiError, App};
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{Map, Value};
use stackhour_core::{js_round, js_round_f64, json_num, number_param};
use stackhour_store::summarize::{build_segments, totals_by, Credited, GROUP_FIELDS};
use stackhour_store::{compute_credits, reattribute::reattributed_range, Heartbeat};

/// Rows echoed in `recent`.
const RECENT_LIMIT: usize = 50;
/// Rows kept per breakdown field.
const BREAKDOWN_LIMIT: usize = 20;

/// The /api/detail route group (single handler + unit-tested helpers).
///
/// `method_not_allowed_fallback` matches the JS dispatcher exactly as
/// `ingest.rs` and `read_api.rs` do: src/server.js:346 guards this path with
/// `req.method === 'GET'`, so `PUT /api/detail` fell through the if-chain to
/// the catch-all `404 {"error":"not found"}` — never a 405 with an empty body.
pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/detail", get(detail))
        .method_not_allowed_fallback(|| async {
            crate::json_error(StatusCode::NOT_FOUND, "not found")
        })
}

/// A parsed query string with JS `URLSearchParams` lookup semantics.
///
/// Two details are load-bearing: an *absent* key is distinguishable from a
/// key present with an empty value (`?value=` selects SQL NULL, no `value=`
/// at all is a 400), and a repeated key resolves to its FIRST occurrence,
/// which is what `searchParams.get` returns.
struct Query(Vec<(String, String)>);

impl Query {
    fn parse(raw: Option<&str>) -> Self {
        Query(
            url::form_urlencoded::parse(raw.unwrap_or("").as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }

    /// `url.searchParams.get(name)` — `None` when the key never appears.
    fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// The `(dimension, value)` selector, or `None` for the 400.
///
/// JS: `!GROUP_FIELDS.includes(dimension) || value === null`. The membership
/// test is case-sensitive (`dimension=Project` is a 400) and `dimension` is
/// defaulted to `''` before the test, so a missing key and an empty key fail
/// identically. `value` only has to be *present* — `''` is legal and means
/// "the SQL NULL bucket".
fn selector(q: &Query) -> Option<(&str, &str)> {
    let dimension = q.get("dimension").unwrap_or("");
    let value = q.get("value")?;
    if !GROUP_FIELDS.contains(&dimension) {
        return None;
    }
    Some((dimension, value))
}

/// The row predicate:
///
/// * `value == ''` -> `row[dimension] == null`, i.e. only a NULL
///   `language`/`branch` (every other groupable column is `NOT NULL`);
/// * otherwise `String(row[dimension] ?? 'unknown') === value`, so the
///   literal `'unknown'` the dashboard sends for a null bucket matches BOTH
///   real NULLs and a column that genuinely holds the string `'unknown'`.
fn matches(row: &Heartbeat, dimension: &str, value: &str) -> bool {
    if value.is_empty() {
        matches!(row.field(dimension), None | Some(Value::Null))
    } else {
        row.field_label(dimension) == value
    }
}

/// The response body, computed from the ALREADY-credited global row set.
///
/// The argument order encodes the parity rule: `credited` must have been
/// produced from the full range (`compute_credits` over every row in
/// `[from, to]`), never from the filtered subset — a neighbouring heartbeat
/// in another project ends the selected stream's gap and therefore changes
/// the credit of a row that *is* selected.
///
/// `raw` and `credited` are filtered separately because they feed different
/// numbers: cost/tokens/recent come from the raw rows, the time totals and
/// the breakdowns/segments from the credited copies.
fn build_body(
    from: f64,
    to: f64,
    dimension: &str,
    value: &str,
    raw: &[Heartbeat],
    credited: &[Credited],
    join_gap: f64,
) -> Value {
    let mut selected_rows: Vec<&Heartbeat> = raw.iter().filter(|r| matches(r, dimension, value)).collect();
    let selected_credits: Vec<Credited> = credited
        .iter()
        .filter(|c| matches(&c.row, dimension, value))
        .cloned()
        .collect();

    let total = js_round(selected_credits.iter().map(|c| c.credit).sum::<f64>());
    let human_total = js_round(
        selected_credits
            .iter()
            .filter(|c| c.row.actor != "agent")
            .map(|c| c.credit)
            .sum::<f64>(),
    );

    let mut breakdowns = Map::new();
    for field in GROUP_FIELDS {
        if field == dimension {
            continue;
        }
        let mut rows = totals_by(&selected_credits, &[field]);
        rows.truncate(BREAKDOWN_LIMIT);
        breakdowns.insert(field.to_string(), Value::Array(rows));
    }

    let total_cost = selected_rows.iter().map(|r| r.cost).sum::<f64>();
    let total_tokens: f64 = selected_rows
        .iter()
        .map(|r| r.tokens_in as f64 + r.tokens_out as f64)
        .sum();

    // `sort((a, b) => b.time - a.time)` — stable, so equal timestamps keep
    // their SQL order — then `.slice(0, 50)`.
    selected_rows.sort_by(|a, b| b.time.total_cmp(&a.time));
    selected_rows.truncate(RECENT_LIMIT);

    let mut body = Map::new();
    body.insert("from".into(), Value::Number(json_num(from)));
    body.insert("to".into(), Value::Number(json_num(to)));
    body.insert("dimension".into(), Value::from(dimension));
    body.insert("value".into(), Value::from(value));
    body.insert("total".into(), Value::from(total));
    body.insert("humanTotal".into(), Value::from(human_total));
    // Derived, NOT an independently rounded agent sum (JS `total - humanTotal`).
    body.insert("agentTotal".into(), Value::from(total - human_total));
    body.insert("totalCost".into(), Value::Number(json_num(round2(total_cost))));
    body.insert("totalTokens".into(), Value::Number(json_num(total_tokens)));
    body.insert("breakdowns".into(), Value::Object(breakdowns));
    body.insert(
        "segments".into(),
        Value::Array(build_segments(&selected_credits, join_gap)),
    );
    body.insert(
        "recent".into(),
        Value::Array(selected_rows.iter().map(|r| r.to_value()).collect()),
    );
    Value::Object(body)
}

/// `Math.round(x * 100) / 100`.
fn round2(x: f64) -> f64 {
    js_round_f64(x * 100.0) / 100.0
}

/// `Date.now() / 1000` — the default for `to`.
fn now_seconds() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

async fn detail(
    State(app): State<App>,
    headers: axum::http::HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    // /api/detail leaks the most: per-heartbeat absolute file paths.
    if let Some(denied) = crate::read_api::read_guard(&app, &headers, raw.as_deref()) {
        return Ok(denied);
    }
    let q = Query::parse(raw.as_deref());
    let Some((dimension, value)) = selector(&q) else {
        return Ok(crate::json_error(
            StatusCode::BAD_REQUEST,
            "dimension and value are required",
        ));
    };
    let (dimension, value) = (dimension.to_string(), value.to_string());

    let days = number_param(q.get("days"), 7.0, Some(1.0), Some(366.0));
    let to = number_param(q.get("to"), now_seconds(), None, None);
    let from = number_param(q.get("from"), to - days * 86400.0, None, None);

    let summary = app.cfg().summary;
    let window = summary.reattribute_window_seconds;
    let rows = app
        .with_db(move |db| reattributed_range(db, from, to, window))
        .await?;
    let credited = compute_credits(
        rows.clone(),
        summary.cap_seconds,
        summary.last_event_credit_seconds,
    );
    let body = build_body(
        from,
        to,
        &dimension,
        &value,
        &rows,
        &credited,
        summary.join_gap_seconds,
    );
    Ok(json_response(StatusCode::OK, &body))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: f64 = 120.0;
    const LAST: f64 = 60.0;
    const JOIN_GAP: f64 = 300.0;

    fn hb(id: i64, time: f64, project: &str, entity: &str) -> Heartbeat {
        Heartbeat {
            id,
            time,
            machine: "laptop".into(),
            source: "editor-files".into(),
            project: project.into(),
            entity: entity.into(),
            entity_type: "file".into(),
            category: "coding".into(),
            language: Some("JavaScript".into()),
            branch: Some("main".into()),
            is_write: 1,
            actor: "human".into(),
            tokens_in: 0,
            tokens_out: 0,
            cost: 0.0,
            created_at: 0.0,
        }
    }

    fn credit(rows: &[Heartbeat]) -> Vec<Credited> {
        compute_credits(rows.to_vec(), CAP, LAST)
    }

    fn body_for(rows: &[Heartbeat], dimension: &str, value: &str) -> Value {
        build_body(0.0, 1000.0, dimension, value, rows, &credit(rows), JOIN_GAP)
    }

    #[test]
    fn query_distinguishes_absent_from_empty_and_keeps_the_first_duplicate() {
        let q = Query::parse(Some("dimension=project&value=&value=beta"));
        assert_eq!(q.get("dimension"), Some("project"));
        assert_eq!(q.get("value"), Some(""));
        assert_eq!(q.get("days"), None);
        // Percent-decoding matches URLSearchParams (including '+' as space).
        let q = Query::parse(Some("value=feature%2Fdetail&x=a+b"));
        assert_eq!(q.get("value"), Some("feature/detail"));
        assert_eq!(q.get("x"), Some("a b"));
    }

    /// The exact 400 matrix from test/dashboard-detail.test.mjs.
    #[test]
    fn selector_gate_is_case_sensitive_and_requires_a_present_value() {
        for bad in [
            "value=alpha",
            "dimension=project",
            "dimension=&value=alpha",
            "dimension=not-a-field&value=alpha",
            "dimension=Project&value=alpha",
        ] {
            assert!(selector(&Query::parse(Some(bad))).is_none(), "{bad}");
        }
        assert_eq!(
            selector(&Query::parse(Some("dimension=branch&value="))),
            Some(("branch", ""))
        );
        assert_eq!(
            selector(&Query::parse(Some("dimension=entity&value=/a.js"))),
            Some(("entity", "/a.js"))
        );
    }

    #[test]
    fn empty_value_selects_null_columns_and_unknown_selects_null_or_the_literal() {
        let mut null_branch = hb(1, 100.0, "alpha", "/a.js");
        null_branch.branch = None;
        null_branch.language = None;
        let mut literal = hb(2, 200.0, "alpha", "/b.js");
        literal.branch = Some("unknown".into());
        let named = hb(3, 300.0, "alpha", "/c.js"); // branch = 'main'

        assert!(matches(&null_branch, "branch", ""));
        assert!(!matches(&literal, "branch", ""));
        assert!(!matches(&named, "branch", ""));

        // 'unknown' is the dashboard's display label for the null bucket AND
        // a legal literal — it matches both.
        assert!(matches(&null_branch, "branch", "unknown"));
        assert!(matches(&literal, "branch", "unknown"));
        assert!(!matches(&named, "branch", "unknown"));

        // Non-null dimensions are exact, never prefix/substring.
        assert!(matches(&named, "project", "alpha"));
        assert!(!matches(&named, "project", "alph"));
        assert!(!matches(&named, "project", "alpha-longer"));
        // Empty value on a NOT NULL column selects nothing.
        assert!(!matches(&named, "project", ""));
    }

    /// The credit model runs GLOBALLY, then the predicate filters: the
    /// intervening beta heartbeat ends alpha's first credit at 10s. Filtering
    /// first would wrongly yield 140 (20 + 60 + 60).
    #[test]
    fn credits_are_global_then_filtered() {
        let mut rows = vec![
            hb(1, 100.0, "alpha", "/work/alpha/a.js"),
            hb(2, 110.0, "beta", "/work/beta/b.js"),
            hb(3, 120.0, "alpha", "/work/alpha/c.js"),
            hb(4, 130.0, "alpha", "/work/alpha/agent.js"),
        ];
        rows[0].tokens_in = 1;
        rows[0].tokens_out = 2;
        rows[0].cost = 0.014;
        rows[1].tokens_in = 50;
        rows[1].tokens_out = 50;
        rows[1].cost = 9.0;
        rows[2].tokens_in = 3;
        rows[2].tokens_out = 4;
        rows[2].cost = 0.016;
        rows[3].machine = "worker".into();
        rows[3].source = "codex-cli".into();
        rows[3].actor = "agent".into();
        rows[3].branch = Some("agent/topic".into());
        rows[3].tokens_in = 100;
        rows[3].tokens_out = 20;
        rows[3].cost = 0.456;

        let body = body_for(&rows, "project", "alpha");
        assert_eq!(body["total"], Value::from(130));
        assert_eq!(body["humanTotal"], Value::from(70));
        assert_eq!(body["agentTotal"], Value::from(60));
        assert_eq!(body["totalTokens"], Value::from(130));
        assert_eq!(body["totalCost"], Value::from(0.49));

        // One breakdown per GROUP_FIELD except the selected dimension, in
        // GROUP_FIELDS order.
        let keys: Vec<&str> = body["breakdowns"]
            .as_object()
            .expect("breakdowns object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["source", "machine", "category", "language", "entity", "actor", "branch"]
        );

        let actor = &body["breakdowns"]["actor"];
        assert_eq!(actor[0]["actor"], Value::from("human"));
        assert_eq!(actor[0]["seconds"], Value::from(70));
        assert_eq!(actor[0]["tokens"], Value::from(10));
        assert_eq!(actor[0]["cost"], Value::from(0.03));
        assert_eq!(actor[1]["actor"], Value::from("agent"));
        assert_eq!(actor[1]["seconds"], Value::from(60));
        assert_eq!(actor[1]["cost"], Value::from(0.46));

        let branch = &body["breakdowns"]["branch"];
        assert_eq!(branch[0]["branch"], Value::from("main"));
        assert_eq!(branch[0]["seconds"], Value::from(70));
        assert_eq!(branch[1]["branch"], Value::from("agent/topic"));

        let segments = body["segments"].as_array().expect("segments");
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0]["actor"], Value::from("human"));
        assert_eq!(segments[0]["start"], Value::from(100));
        assert_eq!(segments[0]["end"], Value::from(120));
        assert_eq!(segments[0]["seconds"], Value::from(70));
        assert_eq!(segments[1]["actor"], Value::from("agent"));
        assert_eq!(segments[1]["start"], Value::from(130));
        assert_eq!(segments[1]["end"], Value::from(130));
    }

    #[test]
    fn recent_is_newest_first_and_capped_at_fifty() {
        let rows: Vec<Heartbeat> = (0..55)
            .map(|i| hb(i + 1, 1000.0 + i as f64, "alpha", &format!("/a/{i}.js")))
            .collect();
        let body = body_for(&rows, "project", "alpha");
        let recent = body["recent"].as_array().expect("recent");
        assert_eq!(recent.len(), RECENT_LIMIT);
        let times: Vec<i64> = recent
            .iter()
            .map(|r| r["time"].as_i64().unwrap_or_default())
            .collect();
        assert_eq!(times, (0..50).map(|i| 1054 - i).collect::<Vec<_>>());
        assert!(recent.iter().all(|r| r["project"] == "alpha"));
        // Raw DB rows, not credited copies: id/created_at survive, no credit.
        assert!(recent[0].get("id").is_some());
        assert!(recent[0].get("created_at").is_some());
        assert!(recent[0].get("credit").is_none());
    }

    #[test]
    fn breakdowns_are_capped_at_twenty_rows() {
        // 25 distinct entities under one project.
        let rows: Vec<Heartbeat> = (0..25)
            .map(|i| hb(i + 1, 1000.0 + i as f64 * 10.0, "alpha", &format!("/a/{i}.js")))
            .collect();
        let body = body_for(&rows, "project", "alpha");
        assert_eq!(
            body["breakdowns"]["entity"]
                .as_array()
                .map(Vec::len)
                .unwrap_or_default(),
            BREAKDOWN_LIMIT
        );
    }

    /// A selection that matches nothing still returns the full envelope with
    /// zeroed numbers and one (empty) breakdown per remaining field.
    #[test]
    fn empty_selection_returns_zeroed_totals() {
        let rows = vec![hb(1, 100.0, "alpha", "/a.js")];
        let body = body_for(&rows, "project", "nope");
        assert_eq!(body["total"], Value::from(0));
        assert_eq!(body["humanTotal"], Value::from(0));
        assert_eq!(body["agentTotal"], Value::from(0));
        assert_eq!(body["totalCost"], Value::from(0));
        assert_eq!(body["totalTokens"], Value::from(0));
        assert_eq!(body["recent"], Value::Array(vec![]));
        assert_eq!(body["segments"], Value::Array(vec![]));
        assert_eq!(
            body["breakdowns"]
                .as_object()
                .map(|m| m.len())
                .unwrap_or_default(),
            GROUP_FIELDS.len() - 1
        );
    }

    /// Key order is observable (JSON.stringify of the object literal).
    #[test]
    fn body_key_order_matches_the_js_literal() {
        let rows = vec![hb(1, 100.0, "alpha", "/a.js")];
        let body = body_for(&rows, "project", "alpha");
        let keys: Vec<&str> = body
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "from",
                "to",
                "dimension",
                "value",
                "total",
                "humanTotal",
                "agentTotal",
                "totalCost",
                "totalTokens",
                "breakdowns",
                "segments",
                "recent"
            ]
        );
    }

    /// agentTotal is `total - humanTotal`, both independently rounded — it is
    /// NOT the rounded sum of the agent credits (which would be 1 here).
    #[test]
    fn agent_total_is_derived_from_two_roundings() {
        let mut human = hb(1, 100.0, "alpha", "/a.js");
        human.actor = "human".into();
        let mut agent = hb(2, 100.0, "alpha", "/b.js");
        agent.actor = "agent".into();
        agent.source = "codex-cli".into();
        let credited = vec![
            Credited {
                row: human,
                credit: 0.6,
            },
            Credited {
                row: agent,
                credit: 0.6,
            },
        ];
        let body = build_body(0.0, 10.0, "project", "alpha", &[], &credited, JOIN_GAP);
        assert_eq!(body["total"], Value::from(1)); // round(1.2)
        assert_eq!(body["humanTotal"], Value::from(1)); // round(0.6)
        assert_eq!(body["agentTotal"], Value::from(0)); // 1 - 1, not round(0.6)
    }

    #[test]
    fn round2_matches_math_round_half_up() {
        assert_eq!(round2(0.486), 0.49);
        assert_eq!(round2(0.005), 0.01);
        assert_eq!(round2(-0.005), -0.0);
        assert_eq!(round2(0.0), 0.0);
    }

    #[test]
    fn routes_registers_only_the_detail_path() {
        // Smoke: the router builds (a `todo!()` here would panic).
        let _: Router<App> = routes();
    }
}
