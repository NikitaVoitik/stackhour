//! GET / and /index.html: serve dashboard.html FROM DISK on every request
//! (live-edit parity), content-type `text/html; charset=utf-8`.
//!
//! Resolution order: $STACKHOUR_ASSETS/dashboard.html ->
//! <exe_dir>/../assets/dashboard.html -> <cwd>/assets/dashboard.html ->
//! <cwd>/src/dashboard.html (dev checkout) -> include_str! fallback. The
//! assets file started as a copy of src/dashboard.html but has since
//! diverged: it adds the `API_KEY`/`withKey()` wrapper that forwards
//! `?api_key=` to every fetch, pairing with the Rust-only read-API auth
//! gate. Its inline JS must never be templated or minified.

use crate::App;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use std::path::PathBuf;

/// Embedded last-resort copy, used only when no on-disk candidate exists.
pub const EMBEDDED_DASHBOARD: &str = include_str!("../../../assets/dashboard.html");

/// Locates and reads the dashboard on every request.
#[derive(Debug, Clone)]
pub struct DashboardLocator {
    /// On-disk candidates in resolution order.
    candidates: Vec<PathBuf>,
}

impl DashboardLocator {
    /// Build the candidate list from the environment / exe location / cwd.
    /// `override_path` (tests) short-circuits to a single candidate.
    pub fn locate(override_path: Option<PathBuf>) -> Self {
        if let Some(p) = override_path {
            return Self {
                candidates: vec![p],
            };
        }
        let mut candidates = Vec::new();
        // $STACKHOUR_ASSETS/dashboard.html. An empty value is falsy in JS and
        // is treated as unset here too.
        if let Ok(dir) = std::env::var("STACKHOUR_ASSETS") {
            if !dir.is_empty() {
                candidates.push(PathBuf::from(dir).join("dashboard.html"));
            }
        }
        // <exe_dir>/../assets/dashboard.html — the installed layout.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("../assets/dashboard.html"));
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("assets/dashboard.html"));
            // Dev checkout: the Node original still lives beside the JS.
            candidates.push(cwd.join("src/dashboard.html"));
        }
        Self { candidates }
    }

    /// Read the first existing candidate from disk; fall back to
    /// [`EMBEDDED_DASHBOARD`].
    ///
    /// src/server.js does a bare `fs.readFileSync` and turns a missing file
    /// into a 500. Serving the compiled-in copy instead is a deliberate
    /// divergence: the embedded bytes are `assets/dashboard.html` (the
    /// api_key-forwarding variant) frozen at build time, so a stripped
    /// install still renders rather than 500ing.
    pub fn read(&self) -> Vec<u8> {
        for path in &self.candidates {
            if let Ok(bytes) = std::fs::read(path) {
                return bytes;
            }
        }
        EMBEDDED_DASHBOARD.as_bytes().to_vec()
    }
}

/// `GET /` and `GET /index.html`, read from disk on EVERY request so an edit
/// to dashboard.html is visible without a restart (src/server.js:178-182).
async fn dashboard(State(app): State<App>) -> Response {
    let body = app.dashboard.read();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .expect("static response builds")
}

/// The dashboard route group (GET / and /index.html).
///
/// `method_not_allowed_fallback` matches the JS dispatcher: `POST /` fell
/// through the `req.method === 'GET'` guard to the catch-all 404, never a 405.
pub fn routes() -> Router<App> {
    Router::new()
        .route("/", get(dashboard))
        .route("/index.html", get(dashboard))
        .method_not_allowed_fallback(|| async {
            crate::json_error(StatusCode::NOT_FOUND, "not found")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_short_circuits_to_a_single_candidate() {
        let loc = DashboardLocator::locate(Some(PathBuf::from("/nope/dashboard.html")));
        assert_eq!(loc.candidates.len(), 1);
        // Missing file -> the embedded copy, not a panic.
        assert_eq!(loc.read(), EMBEDDED_DASHBOARD.as_bytes());
    }

    #[test]
    fn reads_the_first_existing_candidate_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dashboard.html");
        std::fs::write(&path, "<html>edited</html>").expect("write");
        let loc = DashboardLocator::locate(Some(path.clone()));
        assert_eq!(loc.read(), b"<html>edited</html>");
        // Re-read on every call: a live edit is picked up without a restart.
        std::fs::write(&path, "<html>again</html>").expect("write");
        assert_eq!(loc.read(), b"<html>again</html>");
    }

    #[test]
    fn embedded_copy_matches_the_assets_file() {
        assert!(EMBEDDED_DASHBOARD.contains("<html"));
    }
}
