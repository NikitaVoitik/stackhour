//! `stackhour status` — split from main (line budget).
//!
//! Unauthenticated GET `/api/summary?days=1&groupBy=project,source` with a
//! 5s timeout; on failure prints `server unreachable at <url>: <msg>` and
//! exits 1 IMMEDIATELY (unlike other verbs' deferred exit code). `h()`
//! duration formatting ('XhYm' i.e. `${floor(s/3600)}h ${round((s%3600)/60)}m`),
//! padEnd(9) columns, top 15 rows.

use stackhour_core::config::Config;

/// Run the status verb; returns the exit code (may also exit directly on
/// unreachable-server, parity permitting).
pub fn run_status(cfg: &Config) -> i32 {
    let _ = cfg;
    todo!()
}
