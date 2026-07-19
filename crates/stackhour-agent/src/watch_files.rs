//! `files` watcher: mtime scan of agent.projectRoots.
//!
//! filesLastScan is set BEFORE walking (clock-backwards `since` recovery);
//! dot-skip with the `.env` whitelist BEFORE the ignoreDirs exact-basename
//! check; maxScanDepth; mtime accepted in (since, now+60] (future
//! tolerance); LANG_BY_EXT extension table; per-tick git-branch cache that
//! caches None too. Rows: source `editor-files`, category `coding`,
//! language, branch, is_write 1, project via
//! `stackhour_core::project::resolve_project`.

use crate::{Gate, Watcher};
use serde_json::Value;
use stackhour_core::config::Config;
use stackhour_core::Result;

/// The project-roots mtime scanner.
#[derive(Debug, Default)]
pub struct FilesWatcher;

impl Watcher for FilesWatcher {
    fn name(&self) -> &str {
        "files"
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
