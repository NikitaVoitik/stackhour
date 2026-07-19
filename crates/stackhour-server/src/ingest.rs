//! Authenticated POST routes: /api/ingest, /api/agent-status, and the
//! WakaTime-compatible heartbeat paths.
//!
//! /api/ingest: non-array -> 400; a machine-scope violation fails the WHOLE
//! batch atomically with 403 and ZERO inserts; response
//! `{inserted, received}`. WakaTime paths: single-or-array wrap, x-machine-
//! name header, HTTP 202 echoing `[[rawBody, 201], …]` pairs of the RAW
//! entity/time (undefined keys omitted). Body limit 5 MiB -> 413 `body too
//! large` with the stream drained; bad JSON -> 400 `invalid JSON`.

use crate::auth::{allows_machine, authenticate, Principal};
use crate::{json_error, json_response, ApiError, App, BODY_LIMIT};
use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use regex::Regex;
use serde_json::{json, Map, Value};
use stackhour_core::jsnum::{js_display, js_number, js_truthy, json_num};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// The four WakaTime-protocol ingest paths, exactly as src/server.js lists
/// them (both the `/api/v1`-prefixed and bare forms, plain and `.bulk`).
const WAKATIME_PATHS: [&str; 4] = [
    "/api/v1/users/current/heartbeats",
    "/api/v1/users/current/heartbeats.bulk",
    "/users/current/heartbeats",
    "/users/current/heartbeats.bulk",
];

/// The ingest route group.
pub fn routes() -> Router<App> {
    let mut router = Router::new()
        .route("/api/ingest", post(ingest_handler))
        // GET /api/agent-status lives in read_api.rs; axum merges the two
        // method routers for this path because the methods are disjoint.
        .route("/api/agent-status", post(agent_status_handler));
    for path in WAKATIME_PATHS {
        router = router.route(path, post(wakatime_handler));
    }
    // JS dispatch fell through to the catch-all 404 for a wrong method on a
    // known path, rather than axum's default 405.
    router.method_not_allowed_fallback(|| async { json_error(StatusCode::NOT_FOUND, "not found") })
}

/// `Date.now() / 1000` — millisecond resolution, like the JS server clock.
fn now_seconds() -> f64 {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    ms as f64 / 1000.0
}

/// `readBody` + `JSON.parse`: 413 `body too large` past the 5 MiB cap, 400
/// `invalid JSON` on a parse failure.
///
/// Node rejected mid-stream while continuing to drain; axum's `to_bytes`
/// stops accumulating at the same `> limit` boundary and hyper handles the
/// connection. A transport error while reading also surfaces as 413: it is
/// indistinguishable through this API, and in that case the peer is already
/// gone so the status is unobservable.
async fn read_json(body: Body) -> std::result::Result<Value, Response> {
    let bytes = axum::body::to_bytes(body, BODY_LIMIT)
        .await
        .map_err(|_| json_error(StatusCode::PAYLOAD_TOO_LARGE, "body too large"))?;
    serde_json::from_slice(&bytes).map_err(|_| json_error(StatusCode::BAD_REQUEST, "invalid JSON"))
}

/// `authenticate(...)`, or the 401 body so handlers can `?` on it.
///
/// The `Err` variant is a ready-to-send `Response`, which clippy flags as a
/// large error type. Boxing it would only add an allocation on a path that
/// returns the value to the caller immediately.
#[allow(clippy::result_large_err)]
fn principal_or_401(
    headers: &HeaderMap,
    query: &Option<String>,
    app: &App,
) -> std::result::Result<Principal, Response> {
    let server_cfg = app.cfg().raw.get("server").cloned().unwrap_or_else(|| json!({}));
    let query = query.as_deref().unwrap_or("");
    authenticate(headers, query, &server_cfg)
        .ok_or_else(|| json_error(StatusCode::UNAUTHORIZED, "unauthorized"))
}

/// `token is restricted to machine <machine>` — only reachable for a
/// machine-scoped principal, since every other kind allows every machine.
fn scope_denied(principal: &Principal) -> Response {
    let machine = match principal {
        Principal::Machine(m) => m.as_str(),
        // Unreachable: allows_machine() is unconditionally true for these.
        Principal::Open | Principal::Global => "",
    };
    json_error(
        StatusCode::FORBIDDEN,
        &format!("token is restricted to machine {machine}"),
    )
}

/// JS `String(row?.machine || 'unknown')` — a non-object row, or a missing /
/// falsy machine, yields `'unknown'`.
fn row_machine(row: &Value) -> String {
    match row.get("machine") {
        Some(v) if js_truthy(v) => js_display(v),
        _ => "unknown".to_string(),
    }
}

/// `req.headers['x-machine-name'] || 'unknown'`.
fn machine_header(headers: &HeaderMap) -> String {
    headers
        .get("x-machine-name")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

// --------------------------------------------------------------- /api/ingest

async fn ingest_handler(
    State(app): State<App>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Body,
) -> Response {
    // Auth runs BEFORE the body is read: an unauthorized oversized POST is a
    // 401, never a 413.
    let principal = match principal_or_401(&headers, &query, &app) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let parsed = match read_json(body).await {
        Ok(v) => v,
        Err(res) => return res,
    };
    let Value::Array(rows) = parsed else {
        return json_error(StatusCode::BAD_REQUEST, "expected array");
    };
    // Whole-batch atomicity: the scope check precedes every insert, so a
    // single offending row leaves the database untouched.
    if rows
        .iter()
        .any(|row| !allows_machine(&principal, Some(&row_machine(row))))
    {
        return scope_denied(&principal);
    }

    let received = rows.len();
    match app
        .with_db(move |db| stackhour_store::db::insert_heartbeats(db, &rows))
        .await
    {
        Ok(inserted) => json_response(
            StatusCode::OK,
            &json!({ "inserted": inserted, "received": received }),
        ),
        // A SQL failure rolls the batch back and propagates to the catch-all.
        Err(e) => ApiError(e).into_response(),
    }
}

// --------------------------------------------------------- /api/agent-status

async fn agent_status_handler(
    State(app): State<App>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Body,
) -> Response {
    let principal = match principal_or_401(&headers, &query, &app) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let status = match read_json(body).await {
        Ok(v) => v,
        Err(res) => return res,
    };
    // `!status || typeof status !== 'object' || Array.isArray(status)`
    if !status.is_object() {
        return json_error(StatusCode::BAD_REQUEST, "expected object");
    }
    if !allows_machine(&principal, Some(&row_machine(&status))) {
        return scope_denied(&principal);
    }

    let received_at = now_seconds();
    match app
        .with_db(move |db| stackhour_store::db::upsert_agent_status(db, &status, received_at))
        .await
    {
        Ok(ack) => json_response(
            StatusCode::OK,
            &json!({
                "machine": ack.machine,
                "serverTime": json_num(ack.server_time),
                "clockSkewSeconds": json_num(ack.clock_skew_seconds),
            }),
        ),
        // JS wraps the whole upsert in try/catch and answers 400 with the
        // message — validation AND SQL errors alike.
        Err(e) => json_error(StatusCode::BAD_REQUEST, e.message()),
    }
}

// ------------------------------------------------------------ WakaTime paths

async fn wakatime_handler(
    State(app): State<App>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Body,
) -> Response {
    let principal = match principal_or_401(&headers, &query, &app) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let parsed = match read_json(body).await {
        Ok(v) => v,
        Err(res) => return res,
    };
    // A bare object is wrapped into a one-element batch.
    let items: Vec<Value> = match parsed {
        Value::Array(items) => items,
        other => vec![other],
    };
    let machine = machine_header(&headers);
    if !allows_machine(&principal, Some(&machine)) {
        return scope_denied(&principal);
    }
    // JS reads `h.plugin` unguarded, so a null item throws a TypeError that
    // the catch-all turns into a 500 with V8's message. Reproduced rather
    // than silently accepted, because the status code is observable.
    if items.iter().any(Value::is_null) {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot read properties of null (reading 'plugin')",
        );
    }

    let rows: Vec<Value> = items.iter().map(|h| from_wakatime(h, &headers)).collect();
    // The echoed pairs use the RAW request values, not the mapped rows.
    let responses: Vec<Value> = items.iter().map(echo_pair).collect();

    if let Err(e) = app
        .with_db(move |db| stackhour_store::db::insert_heartbeats(db, &rows))
        .await
    {
        return ApiError(e).into_response();
    }
    json_response(StatusCode::ACCEPTED, &json!({ "responses": responses }))
}

/// One `[{ data: { id, entity, time } }, 201]` pair.
///
/// `entity` / `time` echo the RAW input verbatim; a key absent from the
/// request is `undefined` in JS and therefore OMITTED by JSON.stringify —
/// not emitted as null.
fn echo_pair(h: &Value) -> Value {
    let mut data = Map::new();
    data.insert("id".to_string(), Value::Null);
    if let Some(entity) = h.get("entity") {
        data.insert("entity".to_string(), entity.clone());
    }
    if let Some(time) = h.get("time") {
        data.insert("time".to_string(), time.clone());
    }
    json!([{ "data": Value::Object(data) }, 201])
}

/// `/\bai\b/i` with JS semantics: `\b` there is an ASCII word boundary
/// (`[A-Za-z0-9_]`), so Unicode mode is disabled to match it exactly.
fn ai_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // A compile-time-constant pattern; the fallback only exists so this
        // stays panic-free.
        Regex::new(r"(?i-u:\bai\b)").unwrap_or_else(|_| {
            Regex::new("ai").unwrap_or_else(|_| {
                // Unreachable; a literal always compiles.
                Regex::new("$^").unwrap_or_else(|_| unreachable!("literal regex must compile"))
            })
        })
    })
}

/// JS `h[key] || fallback`, keeping the RAW truthy value (an editor plugin
/// sending a number keeps that number; coercion happens at insert time).
fn or_value(h: &Value, key: &str, fallback: Value) -> Value {
    match h.get(key) {
        Some(v) if js_truthy(v) => v.clone(),
        _ => fallback,
    }
}

/// Map one WakaTime heartbeat + request headers to a stackhour row
/// (User-Agent-first-token source, `/\bai\b/i` actor classification).
/// Unit-tested standalone.
pub fn from_wakatime(h: &Value, headers: &HeaderMap) -> Value {
    // String(h.plugin || <user-agent> || 'wakatime-plugin')
    let plugin = match h.get("plugin") {
        Some(v) if js_truthy(v) => js_display(v),
        _ => {
            let ua = headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if ua.is_empty() {
                "wakatime-plugin".to_string()
            } else {
                ua.to_string()
            }
        }
    };
    // "webstorm/2024.1 webstorm-wakatime/15.0.2" -> "webstorm"
    let first_token = plugin.split(' ').next().unwrap_or("");
    let first_token = if first_token.is_empty() {
        "wakatime-plugin"
    } else {
        first_token
    };
    let source = first_token.split('/').next().unwrap_or("").to_lowercase();

    // Number(h.time): an absent key is undefined -> NaN. JSON has no NaN, so
    // it becomes null — which insert_heartbeats skips exactly like the
    // non-finite number was skipped in JS.
    let time = match h.get("time") {
        Some(v) => js_number(v),
        None => f64::NAN,
    };
    let time = if time.is_finite() {
        Value::Number(json_num(time))
    } else {
        Value::Null
    };

    // String(h.category || '') is what the actor regex runs against.
    let category_text = match h.get("category") {
        Some(v) if js_truthy(v) => js_display(v),
        _ => String::new(),
    };
    let actor = if ai_regex().is_match(&category_text) {
        "agent"
    } else {
        "human"
    };

    let project = match h.get("project") {
        Some(v) if js_truthy(v) => v.clone(),
        _ => or_value(h, "alternate_project", json!("unknown")),
    };

    // Key order mirrors the JS object literal (serde_json preserves it).
    json!({
        "time": time,
        "machine": machine_header(headers),
        "source": source,
        "project": project,
        "entity": or_value(h, "entity", json!("unknown")),
        // strict `h.type === 'app'`
        "entity_type": if h.get("type") == Some(&json!("app")) { "app" } else { "file" },
        "category": or_value(h, "category", json!("coding")),
        "language": or_value(h, "language", Value::Null),
        "branch": or_value(h, "branch", Value::Null),
        "is_write": i64::from(h.get("is_write").map(js_truthy).unwrap_or(false)),
        "actor": actor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let name: axum::http::HeaderName = k.parse().expect("valid header name");
            h.insert(name, v.parse().expect("valid header value"));
        }
        h
    }

    // ---- from_wakatime ---------------------------------------------------

    #[test]
    fn maps_a_typical_plugin_heartbeat() {
        let h = json!({
            "time": 1700000000.5,
            "entity": "/src/app.js",
            "type": "file",
            "project": "stackhour",
            "language": "JavaScript",
            "branch": "main",
            "is_write": true,
            "plugin": "webstorm/2024.1 webstorm-wakatime/15.0.2",
        });
        let row = from_wakatime(&h, &headers(&[("x-machine-name", "mac")]));
        assert_eq!(row["source"], json!("webstorm"));
        assert_eq!(row["machine"], json!("mac"));
        assert_eq!(row["time"], json!(1700000000.5));
        assert_eq!(row["project"], json!("stackhour"));
        assert_eq!(row["entity"], json!("/src/app.js"));
        assert_eq!(row["entity_type"], json!("file"));
        assert_eq!(row["category"], json!("coding"));
        assert_eq!(row["language"], json!("JavaScript"));
        assert_eq!(row["branch"], json!("main"));
        assert_eq!(row["is_write"], json!(1));
        assert_eq!(row["actor"], json!("human"));
    }

    #[test]
    fn source_falls_back_to_user_agent_then_to_a_literal() {
        // No plugin field -> User-Agent's first token, before the slash.
        let row = from_wakatime(
            &json!({ "time": 1.0 }),
            &headers(&[("user-agent", "vscode/1.90 vscode-wakatime/24.1.0")]),
        );
        assert_eq!(row["source"], json!("vscode"));

        // Neither plugin nor User-Agent.
        let row = from_wakatime(&json!({ "time": 1.0 }), &HeaderMap::new());
        assert_eq!(row["source"], json!("wakatime-plugin"));

        // An empty User-Agent is falsy in JS, so the literal wins.
        let row = from_wakatime(&json!({ "time": 1.0 }), &headers(&[("user-agent", "")]));
        assert_eq!(row["source"], json!("wakatime-plugin"));
    }

    #[test]
    fn plugin_wins_over_user_agent_and_is_lowercased() {
        let row = from_wakatime(
            &json!({ "time": 1.0, "plugin": "WebStorm/2024.1" }),
            &headers(&[("user-agent", "curl/8")]),
        );
        assert_eq!(row["source"], json!("webstorm"));
    }

    /// `(plugin.split(' ')[0] || 'wakatime-plugin')` — a leading space makes
    /// the first token empty, which is falsy in JS.
    #[test]
    fn leading_space_in_plugin_falls_back_to_the_literal() {
        let row = from_wakatime(&json!({ "time": 1.0, "plugin": " vim/9" }), &HeaderMap::new());
        assert_eq!(row["source"], json!("wakatime-plugin"));
    }

    #[test]
    fn machine_comes_from_the_header_and_defaults_to_unknown() {
        let row = from_wakatime(&json!({ "time": 1.0 }), &HeaderMap::new());
        assert_eq!(row["machine"], json!("unknown"));
        let row = from_wakatime(&json!({ "time": 1.0 }), &headers(&[("x-machine-name", "gcp")]));
        assert_eq!(row["machine"], json!("gcp"));
    }

    #[test]
    fn project_falls_back_to_alternate_project_then_unknown() {
        let base = HeaderMap::new();
        let row = from_wakatime(&json!({ "alternate_project": "alt" }), &base);
        assert_eq!(row["project"], json!("alt"));
        // An explicit project wins.
        let row = from_wakatime(&json!({ "project": "real", "alternate_project": "alt" }), &base);
        assert_eq!(row["project"], json!("real"));
        // A falsy project falls through to alternate_project.
        let row = from_wakatime(&json!({ "project": "", "alternate_project": "alt" }), &base);
        assert_eq!(row["project"], json!("alt"));
        assert_eq!(from_wakatime(&json!({}), &base)["project"], json!("unknown"));
    }

    #[test]
    fn entity_type_is_app_only_on_a_strict_string_match() {
        let base = HeaderMap::new();
        assert_eq!(
            from_wakatime(&json!({ "type": "app" }), &base)["entity_type"],
            json!("app")
        );
        for other in [json!("file"), json!("domain"), json!("App"), json!(1)] {
            let row = from_wakatime(&json!({ "type": other }), &base);
            assert_eq!(row["entity_type"], json!("file"));
        }
        assert_eq!(from_wakatime(&json!({}), &base)["entity_type"], json!("file"));
    }

    #[test]
    fn language_and_branch_are_null_when_falsy() {
        let base = HeaderMap::new();
        let row = from_wakatime(&json!({ "language": "", "branch": 0 }), &base);
        assert_eq!(row["language"], Value::Null);
        assert_eq!(row["branch"], Value::Null);
        assert_eq!(from_wakatime(&json!({}), &base)["language"], Value::Null);
    }

    #[test]
    fn is_write_uses_js_truthiness() {
        let base = HeaderMap::new();
        for truthy in [json!(true), json!(1), json!("yes"), json!([])] {
            let row = from_wakatime(&json!({ "is_write": truthy }), &base);
            assert_eq!(row["is_write"], json!(1));
        }
        for falsy in [json!(false), json!(0), json!(""), Value::Null] {
            let row = from_wakatime(&json!({ "is_write": falsy }), &base);
            assert_eq!(row["is_write"], json!(0));
        }
        assert_eq!(from_wakatime(&json!({}), &base)["is_write"], json!(0));
    }

    #[test]
    fn actor_is_agent_only_on_a_standalone_ai_word() {
        let base = HeaderMap::new();
        let actor = |category: Value| from_wakatime(&json!({ "category": category }), &base)["actor"].clone();
        for yes in [
            json!("ai"),
            json!("AI"),
            json!("Ai"),
            json!("ai coding"),
            json!("coding ai"),
            json!("code-ai-review"),
            json!("writing ai docs"),
        ] {
            assert_eq!(actor(yes.clone()), json!("agent"), "{yes:?}");
        }
        // \b is a word boundary: 'ai' glued to other word characters is not
        // a match ('_' counts as a word char in both engines).
        for no in [
            json!("coding"),
            json!("said"),
            json!("aid"),
            json!("chain"),
            json!("ai_x"),
            json!("x_ai"),
            json!("ai1"),
            json!(""),
        ] {
            assert_eq!(actor(no.clone()), json!("human"), "{no:?}");
        }
        assert_eq!(from_wakatime(&json!({}), &base)["actor"], json!("human"));
    }

    /// JS `\b` is ASCII-only, so a non-ASCII letter next to 'ai' still forms
    /// a boundary. A Unicode-mode Rust regex would disagree here.
    #[test]
    fn ai_word_boundary_is_ascii_like_javascript() {
        let base = HeaderMap::new();
        let row = from_wakatime(&json!({ "category": "é ai" }), &base);
        assert_eq!(row["actor"], json!("agent"));
        let row = from_wakatime(&json!({ "category": "éai" }), &base);
        assert_eq!(row["actor"], json!("agent"));
    }

    #[test]
    fn category_falls_back_to_coding_but_the_actor_test_sees_empty() {
        let base = HeaderMap::new();
        let row = from_wakatime(&json!({}), &base);
        assert_eq!(row["category"], json!("coding"));
        // The regex ran against '' (the fallback is applied separately).
        assert_eq!(row["actor"], json!("human"));
    }

    #[test]
    fn non_finite_time_becomes_null_so_insert_skips_the_row() {
        let base = HeaderMap::new();
        // Absent -> undefined -> NaN.
        assert_eq!(from_wakatime(&json!({}), &base)["time"], Value::Null);
        // Number("nope") -> NaN.
        assert_eq!(
            from_wakatime(&json!({ "time": "nope" }), &base)["time"],
            Value::Null
        );
        // Number("1700000000") -> a real number (JS coerces numeric strings).
        assert_eq!(
            from_wakatime(&json!({ "time": "1700000000" }), &base)["time"],
            json!(1700000000_i64)
        );
        // Number(null) is 0, not NaN.
        assert_eq!(
            from_wakatime(&json!({ "time": Value::Null }), &base)["time"],
            json!(0)
        );
    }

    /// The mapped row keeps the JS object literal's key order, which matters
    /// for any byte comparison of serialised rows.
    #[test]
    fn row_key_order_matches_the_js_object_literal() {
        let row = from_wakatime(&json!({ "time": 1.0 }), &HeaderMap::new());
        let keys: Vec<&str> = row
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "time",
                "machine",
                "source",
                "project",
                "entity",
                "entity_type",
                "category",
                "language",
                "branch",
                "is_write",
                "actor",
            ]
        );
    }

    // ---- echo pairs ------------------------------------------------------

    #[test]
    fn echo_pair_mirrors_the_raw_entity_and_time() {
        let pair = echo_pair(&json!({ "entity": "/a.js", "time": 1700000000.5, "extra": 1 }));
        assert_eq!(
            pair,
            json!([{ "data": { "id": null, "entity": "/a.js", "time": 1700000000.5 } }, 201])
        );
    }

    /// Undefined keys are OMITTED by JSON.stringify — never emitted as null.
    #[test]
    fn echo_pair_omits_absent_keys() {
        assert_eq!(
            serde_json::to_string(&echo_pair(&json!({}))).expect("serialize"),
            r#"[{"data":{"id":null}},201]"#
        );
        assert_eq!(
            serde_json::to_string(&echo_pair(&json!({ "time": 5 }))).expect("serialize"),
            r#"[{"data":{"id":null,"time":5}},201]"#
        );
    }

    /// An explicit null is a PRESENT key and is echoed as null.
    #[test]
    fn echo_pair_keeps_explicit_nulls() {
        assert_eq!(
            serde_json::to_string(&echo_pair(&json!({ "entity": null }))).expect("serialize"),
            r#"[{"data":{"id":null,"entity":null}},201]"#
        );
    }

    /// Raw values pass through unconverted — a string time stays a string.
    #[test]
    fn echo_pair_does_not_coerce_values() {
        let pair = echo_pair(&json!({ "entity": 7, "time": "1700000000" }));
        assert_eq!(pair[0]["data"]["entity"], json!(7));
        assert_eq!(pair[0]["data"]["time"], json!("1700000000"));
    }

    // ---- machine scoping -------------------------------------------------

    #[test]
    fn row_machine_applies_the_unknown_default() {
        assert_eq!(row_machine(&json!({ "machine": "mac" })), "mac");
        assert_eq!(row_machine(&json!({})), "unknown");
        assert_eq!(row_machine(&json!({ "machine": "" })), "unknown");
        assert_eq!(row_machine(&json!({ "machine": Value::Null })), "unknown");
        // `row?.machine` on a non-object is undefined, not an error.
        assert_eq!(row_machine(&Value::Null), "unknown");
        assert_eq!(row_machine(&json!(42)), "unknown");
        // String() of a truthy non-string.
        assert_eq!(row_machine(&json!({ "machine": 42 })), "42");
    }

    #[test]
    fn scope_check_rejects_a_batch_containing_a_foreign_row() {
        let p = Principal::Machine("mac".to_string());
        let violates =
            |batch: &[Value], p: &Principal| batch.iter().any(|r| !allows_machine(p, Some(&row_machine(r))));
        assert!(violates(
            &[json!({ "machine": "mac" }), json!({ "machine": "gcp" })],
            &p
        ));
        assert!(!violates(
            &[json!({ "machine": "mac" }), json!({ "machine": "mac" })],
            &p
        ));
        // A row with no machine at all is 'unknown' and therefore foreign.
        assert!(violates(&[json!({})], &p));
        // An open principal accepts anything.
        assert!(!violates(&[json!({ "machine": "gcp" })], &Principal::Open));
        assert!(!violates(&[json!({ "machine": "gcp" })], &Principal::Global));
    }

    #[tokio::test]
    async fn scope_denied_names_the_owning_machine() {
        let res = scope_denied(&Principal::Machine("mac".to_string()));
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.expect("body");
        assert_eq!(&bytes[..], br#"{"error":"token is restricted to machine mac"}"#);
    }

    #[test]
    fn machine_header_defaults_to_unknown() {
        assert_eq!(machine_header(&HeaderMap::new()), "unknown");
        assert_eq!(machine_header(&headers(&[("x-machine-name", "")])), "unknown");
        assert_eq!(machine_header(&headers(&[("x-machine-name", "mac")])), "mac");
    }

    // ---- body handling ---------------------------------------------------

    #[tokio::test]
    async fn read_json_rejects_oversized_bodies_with_413() {
        let body = Body::from(vec![b'x'; BODY_LIMIT + 1]);
        let res = read_json(body).await.expect_err("must reject");
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.expect("body");
        assert_eq!(&bytes[..], br#"{"error":"body too large"}"#);
    }

    /// Exactly at the cap is fine — JS rejects only on `size > limit`.
    #[tokio::test]
    async fn read_json_accepts_a_body_exactly_at_the_cap() {
        let mut payload = vec![b' '; BODY_LIMIT];
        payload[0] = b'[';
        payload[BODY_LIMIT - 1] = b']';
        let parsed = read_json(Body::from(payload)).await.expect("parses");
        assert_eq!(parsed, json!([]));
    }

    #[tokio::test]
    async fn read_json_rejects_bad_json_with_400() {
        for bad in ["", "{", "not json", "[1,]"] {
            let res = read_json(Body::from(bad)).await.expect_err("must reject");
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    #[tokio::test]
    async fn read_json_parses_scalars_and_containers() {
        assert_eq!(read_json(Body::from("null")).await.expect("null"), Value::Null);
        assert_eq!(read_json(Body::from(" [1] ")).await.expect("array"), json!([1]));
        assert_eq!(
            read_json(Body::from(r#"{"a":1}"#)).await.expect("object"),
            json!({ "a": 1 })
        );
    }

    // ---- end-to-end route behaviour --------------------------------------
    //
    // Only this module's routes are mounted (the sibling groups are still
    // scaffolded), which is enough to pin the ingest contract.

    mod routes {
        use super::*;
        use axum::http::Request;
        use stackhour_core::config::load_config;
        use tower::ServiceExt as _;

        struct Harness {
            router: Router,
            db_path: std::path::PathBuf,
            _dir: tempfile::TempDir,
        }

        impl Harness {
            fn new(user_cfg: &str) -> Self {
                let dir = tempfile::tempdir().expect("tempdir");
                let db_path = dir.path().join("stackhour.db");
                let cfg_path = dir.path().join("config.json");
                // Splice the temp DB path into the caller's config.
                let mut raw: Value = serde_json::from_str(user_cfg).expect("valid config json");
                raw["server"]["db"] = json!(db_path.to_string_lossy());
                std::fs::write(&cfg_path, raw.to_string()).expect("write config");
                let cfg = load_config(&cfg_path).expect("load config");
                let db = stackhour_store::open_db(&db_path).expect("open db");
                let app = crate::make_app(cfg, db, None);
                Harness {
                    router: super::super::routes().with_state(app),
                    db_path,
                    _dir: dir,
                }
            }

            async fn post(&self, uri: &str, headers: &[(&str, &str)], body: &str) -> (StatusCode, Value) {
                let mut req = Request::post(uri);
                for (k, v) in headers {
                    req = req.header(*k, *v);
                }
                let res = self
                    .router
                    .clone()
                    .oneshot(req.body(Body::from(body.to_string())).expect("request"))
                    .await
                    .expect("response");
                let status = res.status();
                let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
                    .await
                    .expect("body");
                let parsed = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                (status, parsed)
            }

            /// Row count straight from the file, bypassing the router.
            fn heartbeat_count(&self) -> i64 {
                let db = stackhour_store::open_db(&self.db_path).expect("reopen db");
                db.query_row("SELECT COUNT(*) FROM heartbeats", [], |r| r.get(0))
                    .expect("count")
            }
        }

        fn hb(time: f64, machine: &str) -> Value {
            json!({ "time": time, "machine": machine, "source": "cli", "project": "p", "entity": "e" })
        }

        #[tokio::test]
        async fn ingest_returns_inserted_and_received() {
            let h = Harness::new("{}");
            let body = json!([hb(1.0, "mac"), hb(2.0, "mac")]).to_string();
            let (status, body) = h.post("/api/ingest", &[], &body).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, json!({ "inserted": 2, "received": 2 }));
            assert_eq!(h.heartbeat_count(), 2);
        }

        /// Bad rows and duplicates make `inserted` < `received`.
        #[tokio::test]
        async fn ingest_counts_skipped_and_deduped_rows() {
            let h = Harness::new("{}");
            let body = json!([hb(1.0, "mac"), hb(1.0, "mac"), json!({ "machine": "mac" })]).to_string();
            let (status, body) = h.post("/api/ingest", &[], &body).await;
            assert_eq!(status, StatusCode::OK);
            // 1 inserted, 1 deduped by hb_dedupe2, 1 skipped for a missing time.
            assert_eq!(body, json!({ "inserted": 1, "received": 3 }));
        }

        #[tokio::test]
        async fn ingest_rejects_a_non_array_body() {
            let h = Harness::new("{}");
            for body in ["{}", "null", "5", r#""x""#] {
                let (status, body) = h.post("/api/ingest", &[], body).await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(body, json!({ "error": "expected array" }));
            }
        }

        #[tokio::test]
        async fn ingest_rejects_invalid_json() {
            let h = Harness::new("{}");
            let (status, body) = h.post("/api/ingest", &[], "[").await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body, json!({ "error": "invalid JSON" }));
        }

        #[tokio::test]
        async fn unauthorized_requests_never_reach_the_body() {
            let h = Harness::new(r#"{"server":{"token":"s3cret"}}"#);
            let (status, body) = h.post("/api/ingest", &[], "not even json").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(body, json!({ "error": "unauthorized" }));

            // …and the right token gets through.
            let rows = json!([hb(1.0, "mac")]).to_string();
            let (status, _) = h
                .post("/api/ingest", &[("authorization", "Bearer s3cret")], &rows)
                .await;
            assert_eq!(status, StatusCode::OK);
        }

        /// The whole batch is refused atomically: ZERO rows are inserted,
        /// including the ones the token WAS allowed to write.
        #[tokio::test]
        async fn machine_scope_violation_inserts_nothing() {
            let h = Harness::new(r#"{"server":{"tokens":{"mac":"m-tok"}}}"#);
            let auth = [("authorization", "Bearer m-tok")];
            let body = json!([hb(1.0, "mac"), hb(2.0, "gcp")]).to_string();
            let (status, body) = h.post("/api/ingest", &auth, &body).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body, json!({ "error": "token is restricted to machine mac" }));
            assert_eq!(h.heartbeat_count(), 0);

            // The same batch minus the foreign row goes through.
            let body = json!([hb(1.0, "mac")]).to_string();
            let (status, _) = h.post("/api/ingest", &auth, &body).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(h.heartbeat_count(), 1);
        }

        #[tokio::test]
        async fn agent_status_acks_with_the_clock_skew() {
            let h = Harness::new("{}");
            let body = json!({ "time": 0, "machine": "mac", "version": "0.1.0" }).to_string();
            let (status, body) = h.post("/api/agent-status", &[], &body).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["machine"], json!("mac"));
            // reported_at was 0, so the skew equals the server clock.
            let skew = body["clockSkewSeconds"].as_f64().expect("skew");
            let server_time = body["serverTime"].as_f64().expect("serverTime");
            assert!((skew - server_time).abs() < 1e-6);
            assert!(server_time > 1_600_000_000.0);
        }

        #[tokio::test]
        async fn agent_status_rejects_non_objects_and_invalid_reports() {
            let h = Harness::new("{}");
            for body in ["[]", "null", "5"] {
                let (status, body) = h.post("/api/agent-status", &[], body).await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(body, json!({ "error": "expected object" }));
            }
            // An object that fails upsert validation is a 400 with its message.
            let (status, body) = h.post("/api/agent-status", &[], r#"{"time":1}"#).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body, json!({ "error": "invalid agent status" }));
        }

        #[tokio::test]
        async fn agent_status_honours_machine_scope() {
            let h = Harness::new(r#"{"server":{"tokens":{"mac":"m-tok"}}}"#);
            let auth = [("authorization", "Bearer m-tok")];
            let body = json!({ "time": 1, "machine": "gcp" }).to_string();
            let (status, body) = h.post("/api/agent-status", &auth, &body).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body, json!({ "error": "token is restricted to machine mac" }));
        }

        #[tokio::test]
        async fn wakatime_bulk_answers_202_with_response_pairs() {
            let h = Harness::new("{}");
            let body = json!([
                { "time": 1.0, "entity": "/a.js", "plugin": "vscode/1.90 vscode-wakatime/24" },
                { "time": 2.0, "entity": "/b.js" },
            ])
            .to_string();
            for path in WAKATIME_PATHS {
                let (status, body) = h.post(path, &[("x-machine-name", "mac")], &body).await;
                assert_eq!(status, StatusCode::ACCEPTED, "{path}");
                assert_eq!(
                    body,
                    // `time` echoes back as an integer, not 1.0: Node's
                    // JSON.parse collapses 1.0 to 1 and JSON.stringify then
                    // prints "1". Verified against the real server:
                    //   curl -XPOST .../heartbeats -d '{"time":1.0,...}'
                    //   -> {"responses":[[{"data":{...,"time":1}},201]]}
                    json!({ "responses": [
                        [{ "data": { "id": null, "entity": "/a.js", "time": 1 } }, 201],
                        [{ "data": { "id": null, "entity": "/b.js", "time": 2 } }, 201],
                    ] }),
                    "{path}"
                );
            }
            // The four paths share one dedupe key, so only 2 rows exist.
            assert_eq!(h.heartbeat_count(), 2);
        }

        /// A single object is wrapped into a one-element batch.
        #[tokio::test]
        async fn wakatime_accepts_a_bare_object() {
            let h = Harness::new("{}");
            let body = json!({ "time": 1.0, "entity": "/a.js" }).to_string();
            let (status, body) = h.post("/api/v1/users/current/heartbeats", &[], &body).await;
            assert_eq!(status, StatusCode::ACCEPTED);
            assert_eq!(
                body,
                // Integer, not 1.0 — see the bulk test above.
                json!({ "responses": [
                    [{ "data": { "id": null, "entity": "/a.js", "time": 1 } }, 201],
                ] })
            );
            assert_eq!(h.heartbeat_count(), 1);
        }

        /// The echo is of the RAW request values, and absent keys vanish —
        /// even when the mapped row substituted a default.
        #[tokio::test]
        async fn wakatime_echo_omits_absent_keys() {
            let h = Harness::new("{}");
            let (status, body) = h.post("/users/current/heartbeats", &[], "[{}]").await;
            assert_eq!(status, StatusCode::ACCEPTED);
            assert_eq!(body, json!({ "responses": [[{ "data": { "id": null } }, 201]] }));
            // The row itself had no usable time and was skipped by the insert.
            assert_eq!(h.heartbeat_count(), 0);
        }

        #[tokio::test]
        async fn wakatime_scopes_on_the_machine_header() {
            let h = Harness::new(r#"{"server":{"tokens":{"mac":"m-tok"}}}"#);
            let body = json!([{ "time": 1.0, "entity": "/a.js" }]).to_string();
            let (status, resp) = h
                .post(
                    "/api/v1/users/current/heartbeats",
                    &[("authorization", "Bearer m-tok"), ("x-machine-name", "gcp")],
                    &body,
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(resp, json!({ "error": "token is restricted to machine mac" }));
            assert_eq!(h.heartbeat_count(), 0);

            // A missing header means 'unknown', which is also not 'mac'.
            let (status, _) = h
                .post(
                    "/api/v1/users/current/heartbeats",
                    &[("authorization", "Bearer m-tok")],
                    &body,
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }

        /// WakaTime plugins authenticate with Basic base64(key) / base64(key:).
        #[tokio::test]
        async fn wakatime_accepts_basic_auth() {
            use base64::Engine as _;
            let h = Harness::new(r#"{"server":{"token":"s3cret"}}"#);
            let encoded = base64::engine::general_purpose::STANDARD.encode("s3cret:");
            let body = json!([{ "time": 1.0, "entity": "/a.js" }]).to_string();
            let (status, _) = h
                .post(
                    "/api/v1/users/current/heartbeats",
                    &[("authorization", &format!("Basic {encoded}"))],
                    &body,
                )
                .await;
            assert_eq!(status, StatusCode::ACCEPTED);
        }

        /// A wrong method on a known path is the JS catch-all 404, not a 405.
        #[tokio::test]
        async fn wrong_method_on_a_known_path_is_404() {
            let h = Harness::new("{}");
            let res = h
                .router
                .clone()
                .oneshot(Request::put("/api/ingest").body(Body::empty()).expect("request"))
                .await
                .expect("response");
            assert_eq!(res.status(), StatusCode::NOT_FOUND);
            let bytes = axum::body::to_bytes(res.into_body(), 1024).await.expect("body");
            assert_eq!(&bytes[..], br#"{"error":"not found"}"#);
        }
    }
}
