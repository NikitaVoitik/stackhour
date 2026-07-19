//! `zed` watcher: Zed threads.db differ (this module uses rusqlite directly —
//! the only sqlite use outside stackhour-store).
//!
//! First-existing ZED_DB_PATHS candidate; bigint-stat change signature over
//! db and db+'-wal' (NEVER -shm); early return on an unchanged signature;
//! rusqlite backup-API snapshot to zed-threads-copy.db via a pid tmp
//! (fallback: open the live db read-only); schema discovery (prefer table
//! 'threads', require id+updated_at, optional summary, QUOTED identifiers);
//! thread diff vs state zedThreads with zedInitDone first-run suppression
//! (init without emission; reappeared ids DO emit); summary-first-120-chars
//! entities; legacy zedDbMtime key cleanup; chmod 0600 on copies in a
//! finally-equivalent.

use crate::{Gate, Watcher};
use serde_json::Value;
use stackhour_core::config::Config;
use stackhour_core::Result;

/// The Zed agent-threads watcher.
#[derive(Debug, Default)]
pub struct ZedWatcher;

impl Watcher for ZedWatcher {
    fn name(&self) -> &str {
        "zed"
    }

    fn gate(&self, cfg: &Config) -> Gate {
        let _ = cfg;
        todo!()
    }

    fn input_marker(&self, state: &Value) -> Option<String> {
        let _ = state;
        todo!()
    }

    fn run(&mut self, cfg: &Config, state: &mut Value, now: f64) -> Result<Vec<Value>> {
        let _ = (cfg, state, now);
        todo!()
    }
}
