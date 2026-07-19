//! Blocking HTTP posts to the server.
//!
//! /api/ingest: 10s timeout, Bearer header only when the token is truthy,
//! non-2xx -> error `/api/ingest failed: HTTP N`, returns the inserted
//! count. /api/agent-status: 3s timeout, failures logged only, never fatal.
//! The health report retains the JSON key `nodeVersion` =
//! `stackhour_core::build_info()` (flagged parity decision; the dashboard
//! only displays it).

use serde_json::Value;
use stackhour_core::Result;

/// POST rows to /api/ingest; returns the server's inserted count.
pub fn post_ingest(server_url: &str, token: &str, rows: &[Value]) -> Result<i64> {
    let _ = (server_url, token, rows);
    todo!()
}

/// POST the health report to /api/agent-status (best-effort; logs failures).
pub fn post_status(server_url: &str, token: &str, report: &Value) {
    let _ = (server_url, token, report);
    todo!()
}
