//! stackhour-server — the ONLY async crate in the workspace (tokio + axum).
//!
//! DB access is serialised through one `Arc<Mutex<Connection>>` exactly like
//! Node's single thread — deliberately NOT a pool; handler DB work goes
//! through `spawn_blocking`. `start_server` builds its own tokio runtime so
//! the bin crate stays synchronous.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use rusqlite::Connection;
use serde_json::{json, Value};
use stackhour_core::config::Config;
use stackhour_core::{Error, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub mod auth;
pub mod dashboard;
pub mod detail;
pub mod ingest;
pub mod read_api;

pub use dashboard::DashboardLocator;

/// The `readBody` cap from src/server.js (`5 * 1024 * 1024`). Exceeding it is
/// a `413 body too large`; kept here so the ingest module and the tests share
/// one constant.
pub const BODY_LIMIT: usize = 5 * 1024 * 1024;

/// Shared application state for all routes.
#[derive(Clone)]
// The fields are reached through the accessors below; the direct reads live
// in the sibling route modules, which are still scaffolded.
#[allow(dead_code)]
pub struct App {
    /// Single serialised connection (parity with Node's single event loop).
    pub(crate) db: Arc<Mutex<Connection>>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) dashboard: DashboardLocator,
}

impl App {
    /// Run a closure against the one shared connection on the blocking pool.
    ///
    /// The mutex serialises every DB touch (Node ran all of this on a single
    /// thread, so no handler ever observed a concurrent write); the
    /// `spawn_blocking` hop keeps the rusqlite call off the async workers. A
    /// poisoned mutex is recovered from rather than propagated: one panicking
    /// handler must not permanently 500 every later request, which is also
    /// how a Node process behaved after an uncaught exception in a handler.
    #[allow(dead_code)] // used by sibling route modules
    pub(crate) async fn with_db<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = Arc::clone(&self.db);
        let joined = tokio::task::spawn_blocking(move || {
            let mut guard = match db.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            f(&mut guard)
        })
        .await;
        match joined {
            Ok(result) => result,
            // A panic inside the closure surfaces the way an uncaught throw
            // did in Node: the catch-all 500 with a message.
            Err(e) => Err(Error::msg(format!("database task failed: {e}"))),
        }
    }

    /// Read access to the loaded config (route modules need `summary.*` and
    /// the raw `server` section for auth).
    #[allow(dead_code)] // used by sibling route modules
    pub(crate) fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// The dashboard file locator (re-reads from disk on every request).
    #[allow(dead_code)] // used by the dashboard route module
    pub(crate) fn dashboard(&self) -> &DashboardLocator {
        &self.dashboard
    }
}

/// A JSON response with exactly the header src/server.js sets
/// (`content-type: application/json`, no charset suffix).
pub fn json_response(status: StatusCode, body: &Value) -> Response {
    // Not serde_json::to_vec: its float formatting diverges from
    // JSON.stringify (see stackhour_core::jsnum::to_js_json).
    let bytes = stackhour_core::jsnum::to_js_json(body).into_bytes();
    (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response()
}

/// `json(res, status, { error: <message> })`.
pub fn json_error(status: StatusCode, message: &str) -> Response {
    json_response(status, &json!({ "error": message }))
}

/// Handler error wrapper reproducing the JS catch-all:
/// `json(res, err.statusCode || 500, { error: String(err.message || err) })`.
///
/// Note on "headers already sent": Node could fail *mid-response* and then
/// only `res.destroy()` the socket. Axum handlers build a complete response
/// value before anything is written, so that branch is structurally
/// unreachable here — there is no partially-sent response to salvage. It
/// would only be observable for a streamed body, and no route streams.
#[derive(Debug, Clone)]
pub struct ApiError(pub Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        json_error(status, self.0.message())
    }
}

/// The catch-all: any unmatched method/path is `404 {"error":"not found"}`.
async fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "not found")
}

/// Answers every HEAD request with the catch-all 404, without routing it.
///
/// src/server.js guards each route with an explicit `req.method === 'GET'`,
/// so a HEAD probe matched nothing and fell through to
/// `json(res, 404, { error: 'not found' })`. axum is the opposite: a GET route
/// implicitly serves HEAD (and `MethodFilter::GET` does NOT opt out of that —
/// `MethodRouter` retries HEAD against its GET handler by design), so without
/// this layer every read endpoint would answer HEAD with a 200 that Node never
/// sends. Rejecting ahead of the router is the only place the two agree.
async fn reject_head(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if req.method() == axum::http::Method::HEAD {
        return not_found().await;
    }
    next.run(req).await
}

/// Whether any ingest token is configured (legacy global OR a non-empty
/// per-machine map). Mirrors
/// `!cfg.server.token && Object.keys(cfg.server.tokens || {}).length === 0`.
///
/// The JS check counts a machine entry even when its value is the empty
/// string (it only inspects `Object.keys`), so a `{"mac": ""}` map suppresses
/// the warning even though `authenticate` would never accept that entry.
fn has_configured_tokens(cfg: &Config) -> bool {
    if !cfg.server.token.is_empty() {
        return true;
    }
    cfg.raw
        .get("server")
        .and_then(|s| s.get("tokens"))
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty())
}

/// The exact lines src/server.js prints from its `listen` callback, in order.
fn startup_lines(cfg: &Config) -> Vec<String> {
    let mut lines = vec![format!(
        "[stackhour] server listening on http://{}:{} (db: {})",
        cfg.server.host,
        cfg.server.port,
        cfg.server.db.display()
    )];
    if !has_configured_tokens(cfg) {
        lines.push(
            "[stackhour] WARNING: no server tokens configured — ingest is open to anyone who can reach this port"
                .to_string(),
        );
    }
    lines
}

/// Start the HTTP server (blocking until shutdown).
///
/// Sequence: maintenance-lock startup check ONCE (`database maintenance is in
/// progress: <db>`), open_db, router assembly, bind `host:port`, exact
/// startup log + no-token warning lines. Catch-all 404 `{"error":"not
/// found"}`; error responses map through `err.statusCode || 500`; the body is
/// dropped when headers were already sent.
pub fn start_server(cfg: Config) -> Result<()> {
    // Checked ONCE at startup, never per request (parity with src/server.js).
    let lock = stackhour_store::maintenance_lock_path(&cfg.server.db);
    if lock.exists() {
        return Err(Error::msg(format!(
            "database maintenance is in progress: {}",
            cfg.server.db.display()
        )));
    }

    let db = stackhour_store::open_db(&cfg.server.db)?;
    let bind_addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let lines = startup_lines(&cfg);
    let app = make_app(cfg, db, None);
    let router = build_router(app);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::msg(e.to_string()))?;

    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| Error::msg(e.to_string()))?;
        // Node logs from inside the `listen` callback — i.e. only once the
        // socket is actually bound, never before.
        for line in lines {
            println!("{line}");
        }
        axum::serve(listener, router)
            .await
            .map_err(|e| Error::msg(e.to_string()))
    })
    // The Connection closes when the App (and its last Arc) drops as this
    // function returns — the JS `server.on('close')` handler, whose close
    // errors were swallowed just as a Drop impl swallows them here.
}

/// Assemble the full router (exposed for in-process tests via
/// `tower::ServiceExt::oneshot` — no sockets).
pub fn build_router(app: App) -> Router {
    // Group order is irrelevant to axum's matcher (the paths are disjoint),
    // but it is kept in src/server.js dispatch order for readability.
    Router::new()
        .merge(dashboard::routes())
        .merge(read_api::routes())
        .merge(ingest::routes())
        .merge(detail::routes())
        .fallback(not_found)
        .with_state(app)
        .layer(axum::middleware::from_fn(reject_head))
}

/// Build an [`App`] for tests / embedding.
pub fn make_app(cfg: Config, db: Connection, dashboard_override: Option<PathBuf>) -> App {
    App {
        db: Arc::new(Mutex::new(db)),
        cfg: Arc::new(cfg),
        dashboard: DashboardLocator::locate(dashboard_override),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stackhour_core::config::load_config;
    use std::path::Path;

    /// A Config built from an on-disk user config file (load_config applies
    /// the same deep-merge the server sees in production).
    fn config_with(user_json: &str, dir: &Path) -> Config {
        let path = dir.join("config.json");
        std::fs::write(&path, user_json).expect("write config");
        load_config(&path).expect("load config")
    }

    #[test]
    fn startup_line_matches_node_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("stackhour.db");
        let cfg = config_with(
            &json!({ "server": { "host": "127.0.0.1", "port": 4141, "db": db } }).to_string(),
            dir.path(),
        );
        let lines = startup_lines(&cfg);
        assert_eq!(
            lines[0],
            format!(
                "[stackhour] server listening on http://127.0.0.1:4141 (db: {})",
                db.display()
            )
        );
    }

    #[test]
    fn warns_when_no_tokens_are_configured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_with("{}", dir.path());
        assert!(!has_configured_tokens(&cfg));
        let lines = startup_lines(&cfg);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1],
            "[stackhour] WARNING: no server tokens configured — ingest is open to anyone who can reach this port"
        );
    }

    #[test]
    fn legacy_token_suppresses_the_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_with(r#"{"server":{"token":"s3cret"}}"#, dir.path());
        assert!(has_configured_tokens(&cfg));
        assert_eq!(startup_lines(&cfg).len(), 1);
    }

    #[test]
    fn machine_tokens_map_suppresses_the_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_with(r#"{"server":{"tokens":{"mac":"t"}}}"#, dir.path());
        assert!(has_configured_tokens(&cfg));
        assert_eq!(startup_lines(&cfg).len(), 1);
    }

    /// JS counts `Object.keys(tokens)` only — an empty-string value still
    /// suppresses the warning even though auth would reject that token.
    #[test]
    fn empty_string_token_value_still_counts_as_configured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_with(r#"{"server":{"tokens":{"mac":""}}}"#, dir.path());
        assert!(has_configured_tokens(&cfg));
    }

    /// `server.tokens` is read leniently: a non-object (array, scalar) is not
    /// a token map, so the warning still fires.
    #[test]
    fn malformed_tokens_map_does_not_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_with(r#"{"server":{"tokens":["mac"]}}"#, dir.path());
        assert!(!has_configured_tokens(&cfg));
    }

    #[test]
    fn maintenance_lock_refuses_startup_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("stackhour.db");
        let cfg = config_with(&json!({ "server": { "db": db } }).to_string(), dir.path());
        std::fs::write(stackhour_store::maintenance_lock_path(&db), b"").expect("write lock");
        let err = start_server(cfg).expect_err("must refuse to start");
        assert_eq!(
            err.message(),
            format!("database maintenance is in progress: {}", db.display())
        );
        // …and it never got as far as creating the database.
        assert!(!db.exists());
    }

    #[test]
    fn api_error_maps_status_code_or_500() {
        let status = |e: ApiError| e.into_response().status();
        assert_eq!(
            status(ApiError(Error::with_status("body too large", 413))),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            status(ApiError(Error::with_status("invalid JSON", 400))),
            StatusCode::BAD_REQUEST
        );
        // No statusCode -> 500 (JS `err.statusCode || 500`).
        assert_eq!(
            status(ApiError(Error::msg("boom"))),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        // A nonsense status falls back to 500 rather than panicking.
        assert_eq!(
            status(ApiError(Error::with_status("weird", 9000))),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn json_helpers_set_the_exact_content_type() {
        let res = json_error(StatusCode::NOT_FOUND, "not found");
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            res.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let bytes = axum::body::to_bytes(res.into_body(), 1024)
            .await
            .expect("body");
        assert_eq!(&bytes[..], br#"{"error":"not found"}"#);
    }

    #[tokio::test]
    async fn error_body_carries_the_message_verbatim() {
        let res = ApiError(Error::with_status("body too large", 413)).into_response();
        let bytes = axum::body::to_bytes(res.into_body(), 1024)
            .await
            .expect("body");
        assert_eq!(&bytes[..], br#"{"error":"body too large"}"#);
    }

    #[tokio::test]
    async fn fallback_is_a_json_404() {
        let res = not_found().await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(res.into_body(), 1024)
            .await
            .expect("body");
        assert_eq!(&bytes[..], br#"{"error":"not found"}"#);
    }

    /// Exercises make_app, which calls DashboardLocator::locate.
    #[tokio::test]
    async fn with_db_serialises_access_and_survives_panics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("stackhour.db");
        let cfg = config_with(
            &json!({ "server": { "db": db_path } }).to_string(),
            dir.path(),
        );
        let db = stackhour_store::open_db(&db_path).expect("open db");
        let app = make_app(cfg, db, None);

        let count = |app: App| async move {
            app.with_db(|c| {
                c.query_row("SELECT COUNT(*) FROM heartbeats", [], |r| r.get::<_, i64>(0))
                    .map_err(|e| Error::msg(e.to_string()))
            })
            .await
        };
        assert_eq!(count(app.clone()).await.expect("query"), 0);

        // A closure error propagates as the workspace error type, status kept.
        let err = app
            .with_db(|_| Err::<(), _>(Error::with_status("nope", 400)))
            .await
            .expect_err("error");
        assert_eq!(err.status_code(), 400);

        // A panic becomes an error instead of poisoning the connection for
        // good: the next call still succeeds.
        assert!(app
            .with_db(|_| -> Result<()> { panic!("boom") })
            .await
            .is_err());
        assert_eq!(count(app.clone()).await.expect("query after panic"), 0);
    }

    #[test]
    fn body_limit_matches_node() {
        assert_eq!(BODY_LIMIT, 5 * 1024 * 1024);
    }

    /// Whole-router assembly tests. These are the first thing that exercises
    /// `build_router` end to end — the four merged groups, the deliberate
    /// merge ordering, and the catch-all — rather than a single group.
    mod router {
        use super::*;
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        struct Harness {
            router: Router,
            _dir: tempfile::TempDir,
        }

        impl Harness {
            fn new() -> Self {
                let dir = tempfile::tempdir().expect("tempdir");
                let db_path = dir.path().join("stackhour.db");
                let cfg = config_with(
                    &json!({ "server": { "db": db_path } }).to_string(),
                    dir.path(),
                );
                let db = stackhour_store::open_db(&db_path).expect("open db");
                // A dashboard override that does not exist on disk, so the
                // embedded copy is served and the test is HOME-independent.
                let app = make_app(cfg, db, Some(dir.path().join("dashboard.html")));
                Harness {
                    router: build_router(app),
                    _dir: dir,
                }
            }

            async fn send(&self, method: &str, uri: &str) -> (StatusCode, String, Vec<u8>) {
                let req = Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request");
                let res = self.router.clone().oneshot(req).await.expect("response");
                let status = res.status();
                let ctype = res
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
                    .await
                    .expect("body");
                (status, ctype, bytes.to_vec())
            }
        }

        /// build_router merges all four groups without an axum route/fallback
        /// conflict. Merely constructing the Harness proves it (a conflict is
        /// a panic at merge time), but assert on a live route too.
        #[tokio::test]
        async fn build_router_assembles_all_four_groups() {
            let h = Harness::new();
            for path in [
                "/",
                "/api/health",
                "/api/summary",
                "/api/now",
                "/api/timeline",
                "/api/recent",
                "/api/wakatime-days",
                "/api/agent-status",
                "/api/detail?dimension=project&value=x",
            ] {
                let (status, _, _) = h.send("GET", path).await;
                assert_eq!(status, StatusCode::OK, "GET {path}");
            }
        }

        #[tokio::test]
        async fn dashboard_is_served_as_html_on_both_paths() {
            let h = Harness::new();
            for path in ["/", "/index.html"] {
                let (status, ctype, body) = h.send("GET", path).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(ctype, "text/html; charset=utf-8");
                assert_eq!(body, dashboard::EMBEDDED_DASHBOARD.as_bytes());
            }
        }

        /// src/server.js guards every read route with `req.method === 'GET'`,
        /// so a HEAD probe falls through to the catch-all 404 — it is NOT an
        /// implicit 200 the way `axum::routing::get` would give.
        #[tokio::test]
        async fn head_falls_through_to_the_json_404() {
            let h = Harness::new();
            for path in ["/", "/api/health", "/api/agent-status", "/api/detail"] {
                let (status, ctype, _) = h.send("HEAD", path).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "HEAD {path}");
                assert_eq!(ctype, "application/json", "HEAD {path}");
            }
        }

        /// A known path with the wrong method is a JSON 404, never a 405 with
        /// an empty body — including /api/detail, whose group was the one
        /// missing `method_not_allowed_fallback`.
        #[tokio::test]
        async fn wrong_method_is_a_json_404_on_every_group() {
            let h = Harness::new();
            for (method, path) in [
                ("PUT", "/api/detail"),
                ("PUT", "/api/health"),
                ("PUT", "/api/ingest"),
                ("POST", "/"),
                ("POST", "/api/summary"),
                ("GET", "/api/ingest"),
            ] {
                let (status, _, body) = h.send(method, path).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
                assert_eq!(body, br#"{"error":"not found"}"#, "{method} {path}");
            }
        }

        #[tokio::test]
        async fn unknown_path_is_the_catch_all_404() {
            let h = Harness::new();
            let (status, ctype, body) = h.send("GET", "/nope").await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(ctype, "application/json");
            assert_eq!(body, br#"{"error":"not found"}"#);
        }
    }
}
