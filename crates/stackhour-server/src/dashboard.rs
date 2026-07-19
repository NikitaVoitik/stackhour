//! GET / and /index.html: serve dashboard.html FROM DISK on every request
//! (live-edit parity), content-type `text/html; charset=utf-8`.
//!
//! Resolution order: $STACKHOUR_ASSETS/dashboard.html ->
//! <exe_dir>/../assets/dashboard.html -> <cwd>/assets/dashboard.html ->
//! <cwd>/src/dashboard.html (dev checkout) -> include_str! fallback. The
//! assets file is a byte-copy of src/dashboard.html — its inline JS is
//! executed by the Node test suite and must never be templated or minified.

use crate::App;
use axum::Router;
use std::path::PathBuf;

/// Embedded last-resort copy, used only when no on-disk candidate exists.
pub const EMBEDDED_DASHBOARD: &str = include_str!("../../../assets/dashboard.html");

/// Locates and reads the dashboard on every request.
#[derive(Debug, Clone)]
pub struct DashboardLocator {
    /// On-disk candidates in resolution order.
    #[allow(dead_code)] // scaffold: read only by the todo!() bodies
    candidates: Vec<PathBuf>,
}

impl DashboardLocator {
    /// Build the candidate list from the environment / exe location / cwd.
    /// `override_path` (tests) short-circuits to a single candidate.
    pub fn locate(override_path: Option<PathBuf>) -> Self {
        let _ = override_path;
        todo!()
    }

    /// Read the first existing candidate from disk; fall back to
    /// [`EMBEDDED_DASHBOARD`].
    pub fn read(&self) -> Vec<u8> {
        todo!()
    }
}

/// The dashboard route group (GET / and /index.html).
pub fn routes() -> Router<App> {
    todo!()
}
