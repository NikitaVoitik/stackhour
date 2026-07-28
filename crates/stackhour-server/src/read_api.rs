//! Unauthenticated GET routes (except /api/detail, which lives in detail.rs;
//! /api/auth-check is the ONLY authed GET).
//!
//! /api/summary: number_param clamps, groupBy filtered against GROUP_FIELDS
//! with `['project']` default, tz clamp ±1440, credited totals + humanTotal;
//! agentTotal = total − humanTotal DERIVED, not re-rounded; raw-row
//! cost/token sums. /api/now: 150s window clamp, per-(actor, project,
//! source, machine) dedupe keeping max time. /api/timeline: segments.
//! /api/recent: newest-N page + context-window reattribution mapped back by
//! id. Plus /api/wakatime-days, GET /api/agent-status, /api/health,
//! /api/auth-check (responds machine|null).

use crate::auth::{authenticate, Principal};
use crate::{json_error, json_response, ApiError, App};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use indexmap::IndexMap;
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use stackhour_core::{js_round, js_round_f64, json_num, number_param, Error, Result, VERSION};
use stackhour_store::reattribute::{reattribute_file_saves, reattributed_range};
use stackhour_store::summarize::{build_segments, day_buckets, totals_by, Credited, GROUP_FIELDS};
use stackhour_store::{compute_credits, list_agent_status, recent_page, rows_in_range, Heartbeat};

/// The read-API route group.
///
/// `method_not_allowed_fallback` reproduces the JS dispatcher: a known path
/// reached with the wrong method fell through to the catch-all
/// `404 {"error":"not found"}`, never a 405. It is applied BEFORE merging the
/// `/api/agent-status` GET route on purpose — `ingest.rs` already installs a
/// custom fallback on that path's POST method router, and axum panics when
/// merging two method routers that BOTH carry a non-default fallback.
pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/auth-check", get(auth_check))
        .route("/api/summary", get(summary))
        .route("/api/now", get(now))
        .route("/api/timeline", get(timeline))
        .route("/api/recent", get(recent))
        .route("/api/wakatime-days", get(wakatime_days))
        .method_not_allowed_fallback(|| async { json_error(StatusCode::NOT_FOUND, "not found") })
        .merge(Router::new().route("/api/agent-status", get(agent_status)))
}

// ---------------------------------------------------------------------------
// query helpers
// ---------------------------------------------------------------------------

/// A parsed query string with JS `URLSearchParams` lookup semantics: an
/// absent key is distinguishable from an empty one, and a repeated key
/// resolves to its FIRST occurrence (what `searchParams.get` returns).
struct Query(Vec<(String, String)>);

impl Query {
    fn parse(raw: Option<&str>) -> Self {
        Query(
            url::form_urlencoded::parse(raw.unwrap_or("").as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// `Date.now() / 1000` — millisecond resolution, like the JS server clock.
fn now_seconds() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

/// `Math.round(x * 100) / 100`, with JS `Math.round` tie-breaking.
fn round2(x: f64) -> f64 {
    js_round_f64(x * 100.0) / 100.0
}

/// `(url.searchParams.get('groupBy') || 'project').split(',').filter(...)`,
/// with the `['project']` fallback applied when the filter empties the list.
///
/// Both defaults matter: a MISSING `groupBy` and a PRESENT-but-empty one are
/// identical (JS `||` treats `''` as falsy), and `groupBy=nope,alsoNope`
/// filters down to nothing and therefore also lands on `['project']`. The
/// membership test is case-sensitive.
fn group_by(raw: Option<&str>) -> Vec<&'static str> {
    let raw = raw.filter(|s| !s.is_empty()).unwrap_or("project");
    let picked: Vec<&'static str> = raw
        .split(',')
        // Yield the 'static GROUP_FIELDS entry, not the borrowed query slice.
        .filter_map(|part| GROUP_FIELDS.into_iter().find(|f| *f == part))
        .collect();
    if picked.is_empty() {
        vec!["project"]
    } else {
        picked
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// Reject an unauthenticated read when tokens ARE configured.
///
/// Node left every read route open, and this port faithfully copied that: on
/// the default `0.0.0.0:4040` bind, any host that could route to the port read
/// the whole activity corpus — `/api/recent` and `/api/detail` return absolute
/// source-file paths as `entity`, plus project names and branches, and
/// `/api/agent-status` returns the machine inventory. Setting `server.tokens`
/// did NOT close it: tokens gated writes only, so a fully tokenized deployment
/// was still wide open to read.
///
/// Deliberately conditional on [`crate::has_configured_tokens`]: a tokenless
/// local install (the documented single-machine setup) keeps working with no
/// credentials, exactly as before. Configuring a token is now the one action
/// that closes reads too, which is what a user configuring a token already
/// believes they are doing.
pub(crate) fn read_guard(app: &App, headers: &HeaderMap, raw: Option<&str>) -> Option<Response> {
    if !crate::has_configured_tokens(app.cfg()) {
        return None;
    }
    let server_cfg = app.cfg().raw.get("server").cloned().unwrap_or(Value::Null);
    match authenticate(headers, raw.unwrap_or(""), &server_cfg) {
        Some(_) => None,
        None => Some(json_error(StatusCode::UNAUTHORIZED, "unauthorized")),
    }
}

/// GET /api/health — no auth, `{"ok":true,"version":"0.1.0"}`.
async fn health() -> Response {
    json_response(StatusCode::OK, &json!({ "ok": true, "version": VERSION }))
}

/// GET /api/auth-check — the ONLY authenticated GET.
///
/// `machine` is the per-machine token's machine, and `null` for the open and
/// legacy-global principals (JS `principal.machine || null`).
async fn auth_check(State(app): State<App>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    let server_cfg = app.cfg().raw.get("server").cloned().unwrap_or(Value::Null);
    let Some(principal) = authenticate(&headers, raw.as_deref().unwrap_or(""), &server_cfg) else {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let machine = match principal {
        Principal::Machine(m) => Value::from(m),
        Principal::Open | Principal::Global => Value::Null,
    };
    json_response(
        StatusCode::OK,
        &json!({ "ok": true, "version": VERSION, "machine": machine }),
    )
}

/// GET /api/agent-status — no auth, ordered by machine ASC.
async fn agent_status(
    State(app): State<App>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    if let Some(denied) = read_guard(&app, &headers, raw.as_deref()) {
        return Ok(denied);
    }
    let now = now_seconds();
    let rows = app.with_db(move |db| list_agent_status(db, now)).await?;
    Ok(json_response(StatusCode::OK, &Value::Array(rows)))
}

/// GET /api/summary.
async fn summary(
    State(app): State<App>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    if let Some(denied) = read_guard(&app, &headers, raw.as_deref()) {
        return Ok(denied);
    }
    let q = Query::parse(raw.as_deref());
    let days = number_param(q.get("days"), 1.0, Some(1.0), Some(366.0));
    let to = number_param(q.get("to"), now_seconds(), None, None);
    let from = number_param(q.get("from"), to - days * 86400.0, None, None);
    let keys = group_by(q.get("groupBy"));
    let tz = number_param(q.get("tz"), 0.0, Some(-1440.0), Some(1440.0));

    let cfg = app.cfg().summary;
    let window = cfg.reattribute_window_seconds;
    let rows = app
        .with_db(move |db| reattributed_range(db, from, to, window))
        .await?;
    let credited = compute_credits(rows.clone(), cfg.cap_seconds, cfg.last_event_credit_seconds);

    Ok(json_response(
        StatusCode::OK,
        &summary_body(from, to, &rows, &credited, &keys, tz),
    ))
}

/// The /api/summary body, split out so the arithmetic is unit-testable
/// without a database.
fn summary_body(
    from: f64,
    to: f64,
    rows: &[Heartbeat],
    credited: &[Credited],
    keys: &[&str],
    tz: f64,
) -> Value {
    let total = js_round(credited.iter().map(|c| c.credit).sum::<f64>());
    let human_total = js_round(
        credited
            .iter()
            .filter(|c| c.row.actor != "agent")
            .map(|c| c.credit)
            .sum::<f64>(),
    );
    // Summed over the RAW reattributed rows, never the credited copies.
    let total_cost = round2(rows.iter().map(|r| r.cost).sum::<f64>());
    let total_tokens: f64 = rows
        .iter()
        .map(|r| r.tokens_in as f64 + r.tokens_out as f64)
        .sum();

    let mut body = Map::new();
    body.insert("from".into(), Value::Number(json_num(from)));
    body.insert("to".into(), Value::Number(json_num(to)));
    body.insert("total".into(), Value::from(total));
    body.insert("humanTotal".into(), Value::from(human_total));
    // Derived (JS `total - humanTotal`), NOT an independently rounded sum.
    body.insert("agentTotal".into(), Value::from(total - human_total));
    body.insert("totalCost".into(), Value::Number(json_num(total_cost)));
    body.insert("totalTokens".into(), Value::Number(json_num(total_tokens)));
    body.insert("totals".into(), Value::Array(totals_by(credited, keys)));
    // `tz` is clamped but never floored by numberParam, so a fractional
    // ?tz=30.5 is reachable; day_buckets takes whole minutes, so it truncates
    // toward zero here. No real client sends one (getTimezoneOffset is an
    // integer).
    body.insert(
        "days".into(),
        Value::Array(day_buckets(credited, keys, tz as i64)),
    );
    Value::Object(body)
}

/// GET /api/now — what is active right now.
async fn now(
    State(app): State<App>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    if let Some(denied) = read_guard(&app, &headers, raw.as_deref()) {
        return Ok(denied);
    }
    let q = Query::parse(raw.as_deref());
    let window_s = number_param(q.get("window"), 150.0, Some(1.0), Some(86400.0));
    // There is no `to` param: the upper bound is always the server clock.
    let to = now_seconds();

    let window = app.cfg().summary.reattribute_window_seconds;
    let rows = app
        .with_db(move |db| reattributed_range(db, to - window_s, to, window))
        .await?;
    Ok(json_response(StatusCode::OK, &Value::Array(now_rows(&rows))))
}

/// Dedupe by `(actor, project, source, machine)` keeping the max-time row,
/// then sort time DESC.
///
/// The comparison is strict (`r.time > prev.time`), so the FIRST row wins a
/// tie; the final sort is stable, so tied groups keep insertion order — both
/// match V8's `Array.prototype.sort` over the `Map` values.
fn now_rows(rows: &[Heartbeat]) -> Vec<Value> {
    let mut seen: IndexMap<(&str, &str, &str, &str), &Heartbeat> = IndexMap::new();
    for r in rows {
        let key = (
            r.actor.as_str(),
            r.project.as_str(),
            r.source.as_str(),
            r.machine.as_str(),
        );
        match seen.get(&key) {
            Some(prev) if r.time <= prev.time => {}
            _ => {
                seen.insert(key, r);
            }
        }
    }
    let mut picked: Vec<&Heartbeat> = seen.into_values().collect();
    picked.sort_by(|a, b| b.time.total_cmp(&a.time));
    picked
        .into_iter()
        .map(|r| {
            let mut m = Map::new();
            m.insert("actor".into(), Value::from(r.actor.clone()));
            m.insert("project".into(), Value::from(r.project.clone()));
            m.insert("source".into(), Value::from(r.source.clone()));
            m.insert("machine".into(), Value::from(r.machine.clone()));
            m.insert("time".into(), Value::Number(json_num(r.time)));
            m.insert("entity".into(), Value::from(r.entity.clone()));
            Value::Object(m)
        })
        .collect()
}

/// GET /api/timeline.
async fn timeline(
    State(app): State<App>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    if let Some(denied) = read_guard(&app, &headers, raw.as_deref()) {
        return Ok(denied);
    }
    let q = Query::parse(raw.as_deref());
    // min is one second (1/60 of an hour), max is 14 days.
    let hours = number_param(q.get("hours"), 24.0, Some(1.0 / 60.0), Some(24.0 * 14.0));
    let to = number_param(q.get("to"), now_seconds(), None, None);
    // Unlike /api/summary there is no `from` param — it is always derived.
    let from = to - hours * 3600.0;

    let cfg = app.cfg().summary;
    let window = cfg.reattribute_window_seconds;
    let rows = app
        .with_db(move |db| reattributed_range(db, from, to, window))
        .await?;
    let credited = compute_credits(rows, cfg.cap_seconds, cfg.last_event_credit_seconds);

    let mut body = Map::new();
    body.insert("from".into(), Value::Number(json_num(from)));
    body.insert("to".into(), Value::Number(json_num(to)));
    body.insert(
        "segments".into(),
        Value::Array(build_segments(&credited, cfg.join_gap_seconds)),
    );
    Ok(json_response(StatusCode::OK, &Value::Object(body)))
}

/// GET /api/recent — the newest N raw rows, reattributed against a context
/// window and mapped back by id.
async fn recent(
    State(app): State<App>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    if let Some(denied) = read_guard(&app, &headers, raw.as_deref()) {
        return Ok(denied);
    }
    let q = Query::parse(raw.as_deref());
    // `integer: true` in the JS is a floor applied AFTER the clamp.
    let limit = number_param(q.get("limit"), 50.0, Some(1.0), Some(500.0)).floor() as i64;

    let window = app.cfg().summary.reattribute_window_seconds;
    let rows = app
        .with_db(move |db| recent_with_context(db, limit, window))
        .await?;
    Ok(json_response(
        StatusCode::OK,
        &Value::Array(rows.iter().map(Heartbeat::to_value).collect()),
    ))
}

/// Fetch the newest `limit` rows, then reattribute them using a widened
/// context set and substitute each row by id.
///
/// The context query is bounded by the SELECTED page's own min/max time (not
/// the whole table), which is exactly why attribution here can differ from a
/// full-range /api/summary query near the page edges.
fn recent_with_context(db: &Connection, limit: i64, window: f64) -> Result<Vec<Heartbeat>> {
    let selected = recent_page(db, limit)?;
    if selected.is_empty() {
        return Ok(selected);
    }
    let (mut min_time, mut max_time) = (f64::INFINITY, f64::NEG_INFINITY);
    for r in &selected {
        min_time = min_time.min(r.time);
        max_time = max_time.max(r.time);
    }
    let context = rows_in_range(db, min_time - window, max_time + window)?;
    let by_id: IndexMap<i64, Heartbeat> = reattribute_file_saves(context, window)
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    // `byId.get(r.id) || r` — the original page order (time DESC) is kept.
    Ok(selected
        .into_iter()
        .map(|r| by_id.get(&r.id).cloned().unwrap_or(r))
        .collect())
}

/// GET /api/wakatime-days — raw rows, `ORDER BY date`.
async fn wakatime_days(
    State(app): State<App>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    if let Some(denied) = read_guard(&app, &headers, raw.as_deref()) {
        return Ok(denied);
    }
    let rows = app.with_db(select_wakatime_days).await?;
    Ok(json_response(StatusCode::OK, &Value::Array(rows)))
}

/// `SELECT * FROM wakatime_days ORDER BY date` with the DDL key order
/// (`date, project, seconds`) that node:sqlite's row objects carry.
///
/// The secondary `project` sort is not in the JS, whose ordering within one
/// date was left to SQLite; spelling it out keeps the Rust port deterministic
/// and matches the `(date, project)` UNIQUE index SQLite scans anyway.
fn select_wakatime_days(db: &mut Connection) -> Result<Vec<Value>> {
    let sql = "SELECT date, project, seconds FROM wakatime_days ORDER BY date, project";
    let mut stmt = db.prepare(sql).map_err(sql_err)?;
    let rows = stmt
        .query_map([], |row| {
            let date: String = row.get(0)?;
            let project: String = row.get(1)?;
            let seconds: f64 = row.get(2)?;
            let mut m = Map::new();
            m.insert("date".into(), Value::from(date));
            m.insert("project".into(), Value::from(project));
            m.insert("seconds".into(), Value::Number(json_num(seconds)));
            Ok(Value::Object(m))
        })
        .map_err(sql_err)?
        .collect::<std::result::Result<Vec<Value>, _>>()
        .map_err(sql_err)?;
    Ok(rows)
}

fn sql_err(e: rusqlite::Error) -> Error {
    Error::msg(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: f64 = 120.0;
    const LAST: f64 = 60.0;

    fn hb(id: i64, time: f64, project: &str, actor: &str) -> Heartbeat {
        Heartbeat {
            id,
            time,
            machine: "laptop".into(),
            source: "editor-files".into(),
            project: project.into(),
            entity: format!("/work/{project}/f{id}.js"),
            entity_type: "file".into(),
            category: "coding".into(),
            language: Some("JavaScript".into()),
            branch: Some("main".into()),
            is_write: 0,
            actor: actor.into(),
            tokens_in: 0,
            tokens_out: 0,
            cost: 0.0,
            created_at: 0.0,
        }
    }

    // ---- query parsing ----------------------------------------------------

    #[test]
    fn query_keeps_the_first_duplicate_and_decodes() {
        let q = Query::parse(Some("days=7&days=30&groupBy=source%2Cmachine&x=a+b"));
        assert_eq!(q.get("days"), Some("7"));
        assert_eq!(q.get("groupBy"), Some("source,machine"));
        assert_eq!(q.get("x"), Some("a b"));
        assert_eq!(q.get("tz"), None);
    }

    #[test]
    fn group_by_filters_against_group_fields_with_a_project_default() {
        assert_eq!(group_by(None), vec!["project"]);
        assert_eq!(group_by(Some("")), vec!["project"]);
        assert_eq!(group_by(Some("nope,alsoNope")), vec!["project"]);
        // Case-sensitive membership, JS `Array.includes`.
        assert_eq!(group_by(Some("Project")), vec!["project"]);
        assert_eq!(group_by(Some("source")), vec!["source"]);
        // Order follows the QUERY, not GROUP_FIELDS, and junk is dropped in
        // place rather than collapsing the whole list.
        assert_eq!(group_by(Some("actor,junk,project")), vec!["actor", "project"]);
        // Duplicates are NOT deduped by the JS filter either.
        assert_eq!(group_by(Some("source,source")), vec!["source", "source"]);
    }

    // ---- clamps -----------------------------------------------------------

    #[test]
    fn number_param_clamps_match_the_js_call_sites() {
        // days: 1..366, fractional allowed (no integer flag).
        assert_eq!(number_param(Some("0"), 1.0, Some(1.0), Some(366.0)), 1.0);
        assert_eq!(number_param(Some("999"), 1.0, Some(1.0), Some(366.0)), 366.0);
        assert_eq!(number_param(Some("1.5"), 1.0, Some(1.0), Some(366.0)), 1.5);
        // A non-numeric value silently falls back to the default.
        assert_eq!(number_param(Some("soon"), 1.0, Some(1.0), Some(366.0)), 1.0);
        // tz: ±1440.
        assert_eq!(
            number_param(Some("-9999"), 0.0, Some(-1440.0), Some(1440.0)),
            -1440.0
        );
        assert_eq!(
            number_param(Some("9999"), 0.0, Some(-1440.0), Some(1440.0)),
            1440.0
        );
        // window: 1..86400.
        assert_eq!(number_param(None, 150.0, Some(1.0), Some(86400.0)), 150.0);
        assert_eq!(number_param(Some("0"), 150.0, Some(1.0), Some(86400.0)), 1.0);
        assert_eq!(
            number_param(Some("99999"), 150.0, Some(1.0), Some(86400.0)),
            86400.0
        );
        // hours: 1/60 .. 336.
        assert_eq!(
            number_param(Some("0"), 24.0, Some(1.0 / 60.0), Some(336.0)),
            1.0 / 60.0
        );
        assert_eq!(
            number_param(Some("1000"), 24.0, Some(1.0 / 60.0), Some(336.0)),
            336.0
        );
        // limit: 1..500, then floored.
        assert_eq!(
            number_param(Some("30.9"), 50.0, Some(1.0), Some(500.0)).floor(),
            30.0
        );
        assert_eq!(
            number_param(Some("9999"), 50.0, Some(1.0), Some(500.0)).floor(),
            500.0
        );
        // `to`/`from` are unclamped and accept negatives.
        assert_eq!(number_param(Some("-5"), 0.0, None, None), -5.0);
    }

    // ---- /api/summary -----------------------------------------------------

    fn summary_for(rows: &[Heartbeat], keys: &[&str], tz: f64) -> Value {
        let credited = compute_credits(rows.to_vec(), CAP, LAST);
        summary_body(0.0, 1000.0, rows, &credited, keys, tz)
    }

    #[test]
    fn summary_body_key_order_matches_the_js_literal() {
        let body = summary_for(&[hb(1, 100.0, "alpha", "human")], &["project"], 0.0);
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
                "total",
                "humanTotal",
                "agentTotal",
                "totalCost",
                "totalTokens",
                "totals",
                "days"
            ]
        );
    }

    #[test]
    fn summary_totals_split_human_and_agent_time() {
        let mut rows = vec![
            hb(1, 100.0, "alpha", "human"),
            hb(2, 120.0, "alpha", "human"),
            hb(3, 130.0, "alpha", "agent"),
        ];
        rows[2].source = "claude-code".into();
        rows[0].cost = 0.014;
        rows[0].tokens_in = 3;
        rows[2].cost = 0.456;
        rows[2].tokens_in = 100;
        rows[2].tokens_out = 20;

        let body = summary_for(&rows, &["project"], 0.0);
        // human stream: 20 + 60; agent stream: 60.
        assert_eq!(body["total"], Value::from(140));
        assert_eq!(body["humanTotal"], Value::from(80));
        assert_eq!(body["agentTotal"], Value::from(60));
        assert_eq!(body["totalCost"], Value::from(0.47));
        assert_eq!(body["totalTokens"], Value::from(123));
        assert_eq!(body["totals"][0]["project"], Value::from("alpha"));
        assert_eq!(body["totals"][0]["seconds"], Value::from(140));
    }

    /// agentTotal is `total - humanTotal` with BOTH sides independently
    /// rounded — never `round(sum of agent credits)`, which would be 1 here.
    #[test]
    fn summary_agent_total_is_derived_not_rerounded() {
        let human = hb(1, 100.0, "alpha", "human");
        let mut agent = hb(2, 100.0, "alpha", "agent");
        agent.source = "claude-code".into();
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
        let body = summary_body(0.0, 10.0, &[], &credited, &["project"], 0.0);
        assert_eq!(body["total"], Value::from(1)); // round(1.2)
        assert_eq!(body["humanTotal"], Value::from(1)); // round(0.6)
        assert_eq!(body["agentTotal"], Value::from(0)); // 1 - 1, not round(0.6)
    }

    /// Cost/token totals come from the RAW rows, so they are unaffected by
    /// the credit model (and are still reported when total time is 0).
    #[test]
    fn summary_cost_and_tokens_come_from_raw_rows() {
        let mut row = hb(1, 100.0, "alpha", "human");
        row.cost = 1.005;
        row.tokens_in = 7;
        row.tokens_out = 5;
        let body = summary_body(0.0, 10.0, &[row], &[], &["project"], 0.0);
        assert_eq!(body["total"], Value::from(0));
        // NOT 1.01: `1.005 * 100` is 100.49999999999999 in IEEE754, so
        // `Math.round(...) / 100` is 1 in Node too. Verified with
        //   node -e "console.log(Math.round(1.005*100)/100)"  ->  1
        assert_eq!(body["totalCost"], Value::from(1));
        assert_eq!(body["totalTokens"], Value::from(12));
        assert_eq!(body["totals"], Value::Array(vec![]));
    }

    /// The same path, with a cost pair that legitimately reaches 1.01, so the
    /// "totals come from raw rows" contract is still pinned against a value
    /// that is not a rounding artefact.
    #[test]
    fn summary_cost_sums_raw_rows_before_rounding() {
        let mut a = hb(1, 100.0, "alpha", "human");
        a.cost = 1.005;
        a.tokens_in = 7;
        a.tokens_out = 5;
        let mut b = hb(2, 200.0, "alpha", "human");
        b.cost = 0.004;
        b.tokens_in = 1;
        b.tokens_out = 0;
        // 1.009 rounds to 1.01; rounding each row first would give 1 + 0 = 1.
        let body = summary_body(0.0, 10.0, &[a, b], &[], &["project"], 0.0);
        assert_eq!(body["totalCost"], Value::from(1.01));
        assert_eq!(body["totalTokens"], Value::from(13));
    }

    /// `from`/`to` echo verbatim, including fractional and negative values.
    #[test]
    fn summary_echoes_the_range_bounds_unrounded() {
        let body = summary_body(-1.5, 1000.25, &[], &[], &["project"], 0.0);
        assert_eq!(body["from"], Value::from(-1.5));
        assert_eq!(body["to"], Value::from(1000.25));
    }

    /// The tz shift is `r.time - tz * 60` seconds, then the UTC date — the
    /// JS `getTimezoneOffset` convention (positive = behind UTC).
    #[test]
    fn summary_days_use_the_timezone_shift() {
        // 1970-01-02T00:30:00Z.
        let row = hb(1, 86400.0 + 1800.0, "alpha", "human");
        let utc = summary_for(std::slice::from_ref(&row), &["project"], 0.0);
        assert_eq!(utc["days"][0]["date"], Value::from("1970-01-02"));
        // 60 minutes behind UTC pushes 00:30Z back into the previous day.
        let behind = summary_for(&[row], &["project"], 60.0);
        assert_eq!(behind["days"][0]["date"], Value::from("1970-01-01"));
    }

    #[test]
    fn summary_group_keys_reach_totals_and_days() {
        let mut rows = vec![hb(1, 100.0, "alpha", "human"), hb(2, 200.0, "beta", "human")];
        rows[1].machine = "desktop".into();
        let body = summary_for(&rows, &["project", "machine"], 0.0);
        let first = &body["totals"][0];
        assert!(first.get("project").is_some());
        assert!(first.get("machine").is_some());
        let day = &body["days"][0];
        assert!(day.get("project").is_some());
        assert!(day.get("machine").is_some());
    }

    // ---- /api/now ---------------------------------------------------------

    #[test]
    fn now_dedupes_per_actor_project_source_machine_keeping_max_time() {
        let rows = vec![
            hb(1, 100.0, "alpha", "human"),
            hb(2, 300.0, "alpha", "human"),
            hb(3, 200.0, "alpha", "human"),
        ];
        let out = now_rows(&rows);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["time"], Value::from(300));
        assert_eq!(out[0]["entity"], Value::from("/work/alpha/f2.js"));
    }

    /// The dedupe key includes `actor`, so the same project+source+machine
    /// legitimately appears twice (one human line, one agent line).
    #[test]
    fn now_keeps_human_and_agent_lines_apart_and_sorts_time_desc() {
        let agent = hb(2, 250.0, "alpha", "agent"); // same source on purpose
        let rows = vec![
            hb(1, 400.0, "alpha", "human"),
            agent,
            hb(3, 100.0, "beta", "human"),
        ];
        let out = now_rows(&rows);
        assert_eq!(out.len(), 3);
        let times: Vec<i64> = out
            .iter()
            .map(|r| r["time"].as_i64().unwrap_or_default())
            .collect();
        assert_eq!(times, vec![400, 250, 100]);
        assert_eq!(out[1]["actor"], Value::from("agent"));
    }

    /// `r.time > prev.time` is strict, so the FIRST row wins a tie.
    #[test]
    fn now_tie_keeps_the_first_row_seen() {
        let mut second = hb(2, 100.0, "alpha", "human");
        second.entity = "/late.js".into();
        let out = now_rows(&[hb(1, 100.0, "alpha", "human"), second]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["entity"], Value::from("/work/alpha/f1.js"));
    }

    #[test]
    fn now_row_key_order_matches_the_js_literal() {
        let out = now_rows(&[hb(1, 100.0, "alpha", "human")]);
        let keys: Vec<&str> = out[0]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["actor", "project", "source", "machine", "time", "entity"]
        );
        // Raw DB columns are NOT echoed: this is a projection, not a row.
        assert!(out[0].get("id").is_none());
        assert!(out[0].get("cost").is_none());
    }

    #[test]
    fn now_on_an_empty_range_is_an_empty_array() {
        assert_eq!(now_rows(&[]), Vec::<Value>::new());
    }

    // ---- /api/recent ------------------------------------------------------

    fn seeded_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = stackhour_store::open_db(&dir.path().join("stackhour.db")).expect("open db");
        (dir, db)
    }

    fn insert(db: &mut Connection, rows: Vec<Value>) {
        stackhour_store::insert_heartbeats(db, &rows).expect("insert");
    }

    fn raw(time: f64, source: &str, actor: &str, entity: &str, is_write: i64) -> Value {
        json!({
            "time": time,
            "machine": "laptop",
            "source": source,
            "project": "alpha",
            "entity": entity,
            "entity_type": "file",
            "category": "coding",
            "is_write": is_write,
            "actor": actor,
        })
    }

    #[test]
    fn recent_returns_an_empty_array_for_an_empty_table() {
        let (_dir, db) = seeded_db();
        assert!(recent_with_context(&db, 50, 120.0).expect("recent").is_empty());
    }

    #[test]
    fn recent_is_newest_first_and_honours_the_limit() {
        let (_dir, mut db) = seeded_db();
        let rows: Vec<Value> = (0..5)
            .map(|i| raw(1000.0 + i as f64, "webstorm", "human", &format!("/a{i}.js"), 0))
            .collect();
        insert(&mut db, rows);
        let page = recent_with_context(&db, 3, 120.0).expect("recent");
        let times: Vec<f64> = page.iter().map(|r| r.time).collect();
        assert_eq!(times, vec![1004.0, 1003.0, 1002.0]);
    }

    /// The context set reaches OUTSIDE the page: the agent write at t=900 is
    /// not itself in the newest-1 page, yet it flips that page's
    /// `editor-files` save to actor=agent AND rewrites its source.
    #[test]
    fn recent_reattributes_from_context_rows_outside_the_page() {
        let (_dir, mut db) = seeded_db();
        insert(
            &mut db,
            vec![
                raw(900.0, "claude-code", "agent", "/work/alpha/a.js", 1),
                raw(950.0, "editor-files", "human", "/work/alpha/a.js", 1),
            ],
        );
        let page = recent_with_context(&db, 1, 120.0).expect("recent");
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].time, 950.0);
        assert_eq!(page[0].actor, "agent");
        assert_eq!(page[0].source, "claude-code");
    }

    /// Outside the reattribution window nothing is touched, and rows that
    /// were never candidates pass through untouched.
    #[test]
    fn recent_leaves_rows_outside_the_window_alone() {
        let (_dir, mut db) = seeded_db();
        insert(
            &mut db,
            vec![
                raw(500.0, "claude-code", "agent", "/work/alpha/a.js", 1),
                raw(950.0, "editor-files", "human", "/work/alpha/a.js", 1),
                raw(960.0, "webstorm", "human", "/work/alpha/a.js", 0),
            ],
        );
        let page = recent_with_context(&db, 3, 120.0).expect("recent");
        assert_eq!(page[0].source, "webstorm");
        assert_eq!(page[0].actor, "human");
        assert_eq!(page[1].source, "editor-files");
        assert_eq!(page[1].actor, "human");
        // Raw rows keep their DB identity (id/created_at survive).
        assert!(page.iter().all(|r| r.id > 0));
    }

    // ---- /api/wakatime-days ----------------------------------------------

    #[test]
    fn wakatime_days_are_ordered_by_date_with_ddl_key_order() {
        let (_dir, mut db) = seeded_db();
        stackhour_store::upsert_wakatime_day(&db, "2024-01-02", "beta", 60.0).expect("upsert");
        stackhour_store::upsert_wakatime_day(&db, "2024-01-01", "alpha", 30.5).expect("upsert");
        let rows = select_wakatime_days(&mut db).expect("select");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["date"], Value::from("2024-01-01"));
        assert_eq!(rows[0]["seconds"], Value::from(30.5));
        assert_eq!(rows[1]["date"], Value::from("2024-01-02"));
        // Integral REALs print as JS integers, not 60.0.
        assert_eq!(rows[1]["seconds"], Value::from(60));
        let keys: Vec<&str> = rows[0]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["date", "project", "seconds"]);
    }

    #[test]
    fn wakatime_days_on_an_empty_table_is_an_empty_array() {
        let (_dir, mut db) = seeded_db();
        assert_eq!(select_wakatime_days(&mut db).expect("select").len(), 0);
    }

    // ---- misc -------------------------------------------------------------

    #[test]
    fn round2_matches_math_round_half_up() {
        assert_eq!(round2(0.486), 0.49);
        assert_eq!(round2(0.005), 0.01);
        assert_eq!(round2(0.0), 0.0);
    }

    #[test]
    fn routes_builds() {
        let _: Router<App> = routes();
    }

    /// Read APIs must close when tokens are configured, and stay open when
    /// they are not.
    ///
    /// Before this, `server.tokens` gated WRITES only: on the default
    /// `0.0.0.0:4040` bind, any LAN peer could read `/api/recent` and
    /// `/api/detail` (absolute source-file paths as `entity`, project names,
    /// branches) and `/api/agent-status` (the machine inventory) with no
    /// credentials, even on a fully tokenized deployment.
    mod read_gate {
        use super::*;
        use axum::body::Body;
        use axum::http::Request;
        use stackhour_core::config::load_config;
        use tower::ServiceExt as _;

        /// Every read route that returns activity data or machine inventory.
        const GATED: &[&str] = &[
            "/api/summary?days=1",
            "/api/now",
            "/api/timeline",
            "/api/recent",
            "/api/wakatime-days",
            "/api/agent-status",
        ];

        fn harness(user_cfg: Value) -> (tempfile::TempDir, Router) {
            let dir = tempfile::tempdir().expect("tempdir");
            let db_path = dir.path().join("stackhour.db");
            let cfg_path = dir.path().join("config.json");
            let mut raw = user_cfg;
            raw["server"]["db"] = json!(db_path.to_string_lossy());
            std::fs::write(&cfg_path, raw.to_string()).expect("write config");
            let cfg = load_config(&cfg_path).expect("load config");
            let db = stackhour_store::open_db(&db_path).expect("open db");
            let app = crate::make_app(cfg, db, None);
            let router = routes().merge(crate::detail::routes()).with_state(app);
            (dir, router)
        }

        async fn status(router: &Router, uri: &str) -> StatusCode {
            router
                .clone()
                .oneshot(Request::get(uri).body(Body::empty()).expect("request"))
                .await
                .expect("response")
                .status()
        }

        #[tokio::test]
        async fn configured_tokens_close_every_read_route() {
            let (_d, router) = harness(json!({ "server": { "tokens": { "mac": "s3cret" } } }));
            for uri in GATED {
                assert_eq!(
                    status(&router, uri).await,
                    StatusCode::UNAUTHORIZED,
                    "{uri} was readable without a token"
                );
                // …and readable WITH one.
                let sep = if uri.contains('?') { '&' } else { '?' };
                assert_eq!(
                    status(&router, &format!("{uri}{sep}api_key=s3cret")).await,
                    StatusCode::OK,
                    "{uri} rejected a valid token"
                );
            }
            // /api/detail leaks per-heartbeat absolute file paths.
            assert_eq!(
                status(&router, "/api/detail?dimension=project&value=x").await,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                status(&router, "/api/detail?dimension=project&value=x&api_key=s3cret").await,
                StatusCode::OK
            );
            // /api/health stays open: it carries no data and is what probes hit.
            assert_eq!(status(&router, "/api/health").await, StatusCode::OK);
        }

        /// Backward compatibility: the documented tokenless single-machine
        /// install keeps working with no credentials at all.
        #[tokio::test]
        async fn a_tokenless_server_stays_open() {
            let (_d, router) = harness(json!({ "server": {} }));
            for uri in GATED {
                assert_eq!(
                    status(&router, uri).await,
                    StatusCode::OK,
                    "{uri} broke a tokenless install"
                );
            }
            assert_eq!(
                status(&router, "/api/detail?dimension=project&value=x").await,
                StatusCode::OK
            );
        }

        /// A wrong token is not a way in.
        #[tokio::test]
        async fn a_wrong_token_is_still_unauthorized() {
            let (_d, router) = harness(json!({ "server": { "token": "right" } }));
            assert_eq!(
                status(&router, "/api/recent?api_key=wrong").await,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(status(&router, "/api/recent?api_key=right").await, StatusCode::OK);
        }
    }
}
