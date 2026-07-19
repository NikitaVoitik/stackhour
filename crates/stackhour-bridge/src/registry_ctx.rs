//! `RegistryCtx` — the ONE seam between the bridge and the config registry.
//!
//! The coordinator does not call `registry::load` and does not own a
//! `Registry`. It owns a `RegistryCtx` and calls exactly two things:
//!
//! ```ignore
//! let mut ctx = RegistryCtx::new(&paths);        // at startup
//! // ...before dispatching each Telegram update:
//! if ctx.tick() { log_new_errors(ctx.new_errors()); }
//! let snap = ctx.snapshot();                    // cheap Arc clone
//! ```
//!
//! Two properties this type exists to guarantee:
//!
//! **A reload must never disturb an in-flight job.** `snapshot()` hands out an
//! `Arc<Registry>`. A job clones that Arc when it spawns and holds it for its
//! whole life, so it keeps reading the exact engine, agent and prompt text it
//! started with even if the user edits the config mid-run. `tick()` swaps in a
//! NEW Arc; the old one stays alive until the last job holding it finishes.
//! This is why the bridge does not need `AgentDef: Clone` — nothing is copied,
//! the whole registry is shared immutably.
//!
//! **Errors are surfaced once, not every tick.** A broken config file would
//! otherwise reprint its error before every single update. `tick()` diffs the
//! new error set against the previous one and `new_errors()` returns only what
//! actually appeared, so the coordinator log gets one line per real problem.
//! `errors()` returns everything, for `stackhour bridge doctor`.

use std::sync::Arc;

use stackhour_core::paths::StoragePaths;
use stackhour_core::registry::{self, Registry, RegistryError};

/// Owns the live registry and its reload cadence.
pub struct RegistryCtx {
    current: Arc<Registry>,
    /// Errors that have already been reported, as their rendered one-line
    /// form (the identity used for "is this new?").
    reported: Vec<String>,
    /// Errors that appeared on the most recent `tick()` (or on `new`).
    fresh: Vec<String>,
}

impl RegistryCtx {
    /// Load the registry rooted at `paths.config_dir`.
    ///
    /// Never fails. A missing config directory is the normal case and yields
    /// the shipped bridge. Whatever errors the initial load produced are
    /// available from `new_errors()` so startup can log them once.
    pub fn new(paths: &StoragePaths) -> Self {
        let registry = registry::load(&paths.config_dir);
        let fresh: Vec<String> = registry.errors.iter().map(ToString::to_string).collect();
        RegistryCtx {
            reported: fresh.clone(),
            fresh,
            current: Arc::new(registry),
        }
    }

    /// Build a context around an already-loaded registry (tests, doctor).
    pub fn from_registry(registry: Registry) -> Self {
        let fresh: Vec<String> = registry.errors.iter().map(ToString::to_string).collect();
        RegistryCtx {
            reported: fresh.clone(),
            fresh,
            current: Arc::new(registry),
        }
    }

    /// Re-stat the config dir and reload if anything changed.
    ///
    /// Returns true when a reload happened. Called once before each Telegram
    /// update is dispatched — the cost is six `stat` calls in the common case
    /// where nothing changed.
    pub fn tick(&mut self) -> bool {
        self.fresh.clear();
        // Six stats in the common case. Only when one of them moved do we
        // pay for a rescan.
        if !self.current.changed_on_disk() {
            return false;
        }
        // The Arc is shared with in-flight jobs, so the rebuild produces a
        // FRESH Registry which then replaces the Arc. Jobs holding the old
        // one are untouched.
        let next = registry::load_with(self.current.root(), self.current.env_source().clone());

        let all: Vec<String> = next.errors.iter().map(ToString::to_string).collect();
        self.fresh = all
            .iter()
            .filter(|e| !self.reported.contains(e))
            .cloned()
            .collect();
        self.reported = all;
        self.current = Arc::new(next);
        true
    }

    /// An immutable handle to the current registry. Clone this into a job at
    /// spawn time; the job then reads a consistent snapshot for its lifetime.
    pub fn snapshot(&self) -> Arc<Registry> {
        Arc::clone(&self.current)
    }

    /// Borrow the current registry for the duration of one dispatch.
    pub fn registry(&self) -> &Registry {
        &self.current
    }

    /// Every current validation error (bridge doctor).
    pub fn errors(&self) -> &[RegistryError] {
        &self.current.errors
    }

    /// Only the errors that appeared since the previous tick, rendered
    /// one-per-line (coordinator log). Empty on a tick with no new problems.
    pub fn new_errors(&self) -> &[String] {
        &self.fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn paths_for(dir: &Path) -> StoragePaths {
        StoragePaths {
            config_path: dir.join("config.json"),
            config_dir: dir.to_path_buf(),
            data_dir: dir.join("data"),
            db_path: dir.join("data/stackhour.db"),
        }
    }

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        bump_mtime(path.parent().unwrap());
    }

    /// Push a directory's mtime into the future.
    ///
    /// Reload detection is mtime-based, and a filesystem's timestamp
    /// granularity can be coarser than the microseconds a test takes to
    /// create and then delete a file. Production is never that fast (a human
    /// is typing), but a test is, so the tests state the change explicitly
    /// rather than racing the clock.
    fn bump_mtime(dir: &Path) {
        let f = std::fs::File::open(dir).unwrap();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        f.set_times(std::fs::FileTimes::new().set_modified(future))
            .unwrap();
    }

    #[test]
    fn a_missing_config_dir_loads_the_shipped_bridge_without_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = RegistryCtx::new(&paths_for(&dir.path().join("nope")));
        assert!(ctx.errors().is_empty());
        assert!(ctx.new_errors().is_empty());
        assert!(ctx.registry().engines.contains_key("claude"));
    }

    #[test]
    fn tick_is_false_when_nothing_changed() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = RegistryCtx::new(&paths_for(dir.path()));
        assert!(!ctx.tick());
        assert!(!ctx.tick());
    }

    #[test]
    fn tick_picks_up_a_new_entity_and_swaps_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = RegistryCtx::new(&paths_for(dir.path()));
        let before = ctx.snapshot();
        assert!(!before.engines.contains_key("ollama"));

        write(
            dir.path(),
            "engines/ollama.toml",
            "bin = \"ollama\"\nkind = \"plain-lines\"\n",
        );
        assert!(ctx.tick());
        assert!(ctx.registry().engines.contains_key("ollama"));

        // The IN-FLIGHT snapshot is unchanged — this is the whole point.
        assert!(!before.engines.contains_key("ollama"));
        assert!(!Arc::ptr_eq(&before, &ctx.snapshot()));
    }

    #[test]
    fn an_in_flight_snapshot_survives_the_config_being_deleted() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "engines/ollama.toml",
            "bin = \"ollama\"\nkind = \"plain-lines\"\nargs = [\"run\", \"-\"]\n",
        );
        let mut ctx = RegistryCtx::new(&paths_for(dir.path()));
        let job = ctx.snapshot();
        assert_eq!(job.engines["ollama"].bin, "ollama");

        std::fs::remove_file(dir.path().join("engines/ollama.toml")).unwrap();
        bump_mtime(&dir.path().join("engines"));
        assert!(ctx.tick());
        assert!(!ctx.registry().engines.contains_key("ollama"));
        // The running job still has its engine.
        assert_eq!(job.engines["ollama"].bin, "ollama");
        assert_eq!(job.engines["ollama"].args, vec!["run", "-"]);
    }

    #[test]
    fn a_new_error_is_reported_once_and_then_stays_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = RegistryCtx::new(&paths_for(dir.path()));
        assert!(ctx.new_errors().is_empty());

        write(dir.path(), "engines/bad.toml", "= not toml\n");
        assert!(ctx.tick());
        assert_eq!(ctx.new_errors().len(), 1, "{:?}", ctx.new_errors());
        assert!(ctx.new_errors()[0].contains("engines/bad.toml"));
        assert!(ctx.new_errors()[0].contains("invalid TOML"));

        // A subsequent tick that changes nothing must not re-report it...
        assert!(!ctx.tick());
        assert!(ctx.new_errors().is_empty());
        // ...but it is still visible to doctor.
        assert_eq!(ctx.errors().len(), 1);
    }

    #[test]
    fn a_second_distinct_error_is_reported_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "engines/bad.toml", "= not toml\n");
        let mut ctx = RegistryCtx::new(&paths_for(dir.path()));
        assert_eq!(ctx.new_errors().len(), 1, "startup reports what it found");

        write(dir.path(), "engines/worse.toml", "= also not toml\n");
        assert!(ctx.tick());
        assert_eq!(ctx.new_errors().len(), 1);
        assert!(ctx.new_errors()[0].contains("worse.toml"));
        assert_eq!(ctx.errors().len(), 2);
    }

    #[test]
    fn defaults_are_visible_through_the_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"bridge":{"defaultTarget":"mac"}}"#,
        )
        .unwrap();
        let ctx = RegistryCtx::new(&paths_for(dir.path()));
        assert_eq!(ctx.registry().defaults.target, "mac");
    }
}
