//! The coordinator's WORKER lanes: `dispatchMac` / `pollResults` /
//! `cancelQueuedMac` / `workerAlive`, generalized from the single hardcoded
//! `mac` lane to one [`WorkerLane`] per non-local target.
//!
//! This is coordinator.mjs lines 304-335, lifted into a type the daemon can
//! own — one instance per pull-worker target. Each lane is constructed for a
//! target NAME and stamps that name into every job it dispatches (the
//! `target` field a targeted `bridge claim <name>` filters on); sessions,
//! pending entries and delivered results are all keyed by the lane's target.
//! The disk half of the protocol — the job/inprogress/results dance and the
//! heartbeat files — lives in [`crate::jobs`]; this module is the
//! Telegram-facing half: it writes a job, posts a status message, and later
//! turns a result file back into a delivered answer.
//!
//! Unlike the local lane there is no live progress: the worker runs claude
//! WITHOUT `--include-partial-messages`, so the status message is posted
//! once at dispatch and never edited again until it is deleted by
//! `deliverFinal` (or replaced by `🛑 Cancelled.`). There is no typing
//! indicator and no 800ms throttle on this lane.
//!
//! ## The behaviours that are easy to get wrong
//!
//! * **`pending` is memory only.** After a coordinator restart, results still
//!   arrive and are still delivered, but with no pending entry: the stale
//!   `working…` message is never deleted and keeps a live Stop button
//!   forever, and the duration footer degrades to the literal `done`.
//!   Preserved — persisting it would change what the owner sees.
//! * **The session is filed under the LANE's target explicitly.** The JS
//!   `pollResults` called `setSession('mac', engine, …)` with a hardcoded
//!   target, NOT the active one; here it is the lane's own target, same
//!   principle. If the user switches to `/gcp` while a worker job is in
//!   flight, a port that wrote to the active target would corrupt both
//!   sessions.
//! * **The worker's echo wins.** The engine used to render a result is
//!   `res.engine || info?.engine || 'claude'`. After a restart, a payload
//!   that omits `engine` is labelled Claude even if it ran under Codex — and
//!   its session id is filed under the claude key.
//! * **Cancellation is racy by construction.** `existsSync(jobs/<id>.json)`
//!   is the only test for claimed-vs-queued, and the worker can claim in the
//!   window between the check and the unlink. The job is then counted as
//!   cancelled while it actually runs, and its result later arrives with no
//!   pending entry. Preserved; the alternative is a distributed lock.
//! * **Iteration order.** The JS `Map` is insertion-ordered, so the
//!   `🛑 Cancelled.` edits arrive oldest-first. `pending` is an
//!   [`IndexMap`] for exactly that reason, and cancellation collects the ids
//!   before mutating, which JS gets away with and Rust does not.
//! * **Orphan results belong to ONE lane.** The results/ directory is shared
//!   by every lane; a result is normally matched to a lane by its pending
//!   id. A result with NO pending entry anywhere (coordinator restart) has
//!   no target of record — [`WorkerLane::poll_results`] takes those too,
//!   and the runtime designates exactly one lane (its first) as the orphan
//!   sweeper while the rest poll with [`WorkerLane::poll_matched`].
//!
//! ## One declared divergence
//!
//! An unparseable result file is QUARANTINED (renamed to `<id>.json.bad`)
//! and logged once. The JS `continue`s without unlinking, so a corrupt result
//! is re-read and re-failed every 1000ms forever — a permanent 1 Hz hot loop
//! with no log line and no way for the owner to notice. Nothing else about
//! the ordering changes: the file is still parsed BEFORE it is removed, so a
//! result that parses is never lost to a delete-then-crash.

use crate::jobs;
use crate::keyboard;
use crate::local_lane::{deliver_final, LaneContext};
use crate::telegram::Tg;
use crate::BridgePaths;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// Registry template names this lane renders. Named, not inlined, so the set
/// of strings the lane can produce is greppable in one place.
mod tpl {
    pub const STATUS_LINE: &str = "status-line";
    pub const STATUS_WORKING: &str = "status-working";
    pub const STATUS_QUEUED: &str = "status-queued";
    pub const FINAL: &str = "final";
    pub const NO_OUTPUT: &str = "no-output";
    pub const CANCELLED: &str = "cancelled";
    pub const ERROR_MAC: &str = "error-mac";
    pub const ERROR_EXIT_MAC: &str = "error-exit-mac";
}

/// The duration footer when the pending entry is gone (coordinator restart).
const DURATION_UNKNOWN: &str = "done";

/// The engine a result is attributed to when neither the worker nor the
/// pending entry names one. Matches `res.engine || info?.engine || 'claude'`.
const FALLBACK_ENGINE: &str = "claude";

/// What a worker lane needs on top of [`LaneContext`].
///
/// `LaneContext::target` only resolves LOCAL targets, and a worker lane's
/// target is by definition not one, so the label comes through its own
/// accessor. (This trait was `MacContext` when the only lane was `mac`.)
pub trait WorkerContext: LaneContext {
    /// `targets[<target>].label`, falling back to the bare target key.
    fn worker_label(&self, target: &str) -> String;
}

/// A job dispatched to a worker, awaiting its result file.
#[derive(Debug, Clone)]
pub struct WorkerPending {
    pub job_id: String,
    /// The engine captured at dispatch, used only if the worker's payload
    /// does not echo one back.
    pub engine: String,
    /// Active agent at dispatch, for the session key.
    pub agent: Option<String>,
    /// The status message posted at dispatch, deleted on delivery.
    pub status_message_id: Option<i64>,
    /// Dispatch timestamp (ms) for the duration footer.
    pub dispatched_ms: i64,
}

/// The result of a `/stop` sweep over one lane's queued jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CancelOutcome {
    /// Jobs whose file was still in `jobs/` and was removed.
    pub cancelled: usize,
    /// Jobs the worker had already claimed — not interruptible remotely.
    pub running: usize,
}

/// One worker lane, bound to a target name. Clone-cheap; every clone shares
/// one pending map.
#[derive(Clone)]
pub struct WorkerLane {
    tg: Arc<Tg>,
    ctx: Arc<dyn WorkerContext>,
    paths: BridgePaths,
    /// The roster target this lane dispatches under: stamped into every job,
    /// used as the session key and the heartbeat lane.
    target: String,
    pending: Arc<Mutex<IndexMap<String, WorkerPending>>>,
}

impl WorkerLane {
    pub fn new(
        tg: Arc<Tg>,
        ctx: Arc<dyn WorkerContext>,
        paths: BridgePaths,
        target: impl Into<String>,
    ) -> WorkerLane {
        WorkerLane {
            tg,
            ctx,
            paths,
            target: target.into(),
            pending: Arc::new(Mutex::new(IndexMap::new())),
        }
    }

    /// The target name this lane dispatches under.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Whether this lane's worker heartbeat is fresh. Read LIVE at every
    /// render site (`/where`, the startup banner, dispatch) — never cached.
    /// Per-lane: `worker-heartbeat-<target>` when it exists, else the legacy
    /// shared file (see [`jobs::worker_alive_for`]).
    pub fn worker_alive(&self) -> bool {
        jobs::worker_alive_for(&self.paths, &self.target)
    }

    /// How many dispatched jobs are still awaiting a result.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().expect("pending").len()
    }

    fn engine_label(&self, engine: &str) -> String {
        self.ctx
            .engine(engine)
            .map(|d| d.label)
            .unwrap_or_else(|| engine.to_string())
    }

    /// This lane's target label, from the config via the context.
    fn label(&self) -> String {
        self.ctx.worker_label(&self.target)
    }

    /// `dispatchMac(prompt, engine, media)` — write the job, post the status
    /// message, remember it. The job is stamped with this lane's target.
    ///
    /// Order is the reference's and it matters: the job file is written
    /// FIRST, and the heartbeat is only read afterwards. A worker that claims
    /// the file in that window still gets the `queued (… offline…)` wording.
    /// Harmless, but observable, so it is preserved.
    ///
    /// Returns the job id, or `None` if the job file could not be written (in
    /// which case nothing was posted and nothing is pending).
    pub fn dispatch(
        &self,
        prompt: &str,
        engine: &str,
        agent: Option<&str>,
        media: Option<Value>,
    ) -> Option<String> {
        let t0 = self.ctx.now_ms();
        let session = self.ctx.session(&self.target, engine, agent);

        // Key order is the on-disk contract with the Node worker; `id` is
        // prepended by write_job. `media` and `sessionId` are explicit nulls,
        // not omitted keys. `target` sits before `ts` — the one roster-era
        // addition, which the Node worker ignores and a targeted
        // `bridge claim <target>` filters on.
        let job = json!({
            "prompt": prompt,
            "engine": engine,
            "media": media.unwrap_or(Value::Null),
            "sessionId": session,
            "target": self.target,
            "ts": t0,
        });
        let id = match jobs::write_job(&self.paths.jobs_dir, &job) {
            Ok(id) => id,
            Err(e) => {
                self.ctx.log(&format!("dispatchMac err {e}"));
                return None;
            }
        };

        let engine_label = self.engine_label(engine);
        let target_label = self.label();
        let activity = if self.worker_alive() {
            self.ctx.prompt(tpl::STATUS_WORKING, &[])
        } else {
            self.ctx.prompt(tpl::STATUS_QUEUED, &[])
        };
        let text = self.ctx.prompt(
            tpl::STATUS_LINE,
            &[
                ("engine", &engine_label),
                ("target", &target_label),
                ("activity", &activity),
            ],
        );
        // Plain text, no parse_mode, NOT escaped — the same first-send shape
        // the local lane uses, and the Stop button rides along.
        let sent = self.tg.send_message(
            &text,
            None,
            Some(&json!({ "reply_markup": keyboard::stop_keyboard() })),
        );
        let status_message_id = sent
            .as_ref()
            .and_then(|m| m.get("message_id"))
            .and_then(Value::as_i64);

        self.pending.lock().expect("pending").insert(
            id.clone(),
            WorkerPending {
                job_id: id.clone(),
                engine: engine.to_string(),
                agent: agent.map(str::to_string),
                status_message_id,
                dispatched_ms: t0,
            },
        );
        Some(id)
    }

    /// The job ids this lane is still awaiting. The runtime uses this to
    /// tell the orphan-sweeping lane which results belong to its siblings.
    pub fn pending_ids(&self) -> Vec<String> {
        self.pending.lock().expect("pending").keys().cloned().collect()
    }

    /// `pollResults()` — one sweep of `results/`, called once a second.
    ///
    /// Takes results matched to this lane's pending map AND orphan results
    /// (no pending entry — coordinator restart). With a single lane this is
    /// exactly the reference behaviour; with several, the runtime calls
    /// [`poll_with_orphans`](Self::poll_with_orphans) on ONE designated lane
    /// and [`poll_matched`](Self::poll_matched) on the rest, so orphans are
    /// delivered exactly once.
    ///
    /// Returns how many results were delivered, which is what the timer
    /// thread's tests assert on.
    pub fn poll_results(&self) -> usize {
        self.poll(Some(&std::collections::HashSet::new()))
    }

    /// [`poll_results`](Self::poll_results) restricted to results whose id
    /// is in THIS lane's pending map. Orphans (and other lanes' results) are
    /// left untouched.
    pub fn poll_matched(&self) -> usize {
        self.poll(None)
    }

    /// [`poll_results`](Self::poll_results) that also skips `foreign` ids —
    /// results pending on a SIBLING lane, which must be delivered by that
    /// lane (under its target's label and session key), never by this one.
    pub fn poll_with_orphans(&self, foreign: &std::collections::HashSet<String>) -> usize {
        self.poll(Some(foreign))
    }

    /// `orphans`: `None` = matched-only; `Some(foreign)` = matched + every
    /// unmatched result NOT pending on a sibling lane (the foreign set).
    fn poll(&self, orphans: Option<&std::collections::HashSet<String>>) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.paths.results_dir) else {
            return 0; // the directory is gone; the next tick will find it
        };
        // Raw readdir order, NOT sorted — matching the reference. Under load
        // two results landing in the same tick are delivered in whatever
        // order the filesystem hands them over.
        let files: Vec<std::path::PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();

        let mut delivered = 0;
        for path in files {
            let id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Skip what is not this poll's to consume (or quarantine): a
            // matched-only poll takes nothing beyond its own pending ids,
            // and an orphan sweep leaves sibling lanes' results alone.
            if !self.pending.lock().expect("pending").contains_key(&id) {
                match orphans {
                    None => continue,
                    Some(foreign) if foreign.contains(&id) => continue,
                    Some(_) => {}
                }
            }
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok());
            let Some(res) = parsed else {
                self.quarantine(&path);
                continue;
            };
            // Parse first, THEN unlink: a result that parses is never lost to
            // a delete-then-crash.
            let _ = std::fs::remove_file(&path);

            let info = self.pending.lock().expect("pending").shift_remove(&id);
            self.deliver(&res, info.as_ref());
            delivered += 1;
        }
        delivered
    }

    /// Move an unparseable result aside so the 1s poller does not re-read it
    /// forever, and say so once. See the module docs — this is the one
    /// declared divergence from the reference.
    fn quarantine(&self, path: &std::path::Path) {
        let bad = path.with_extension("json.bad");
        match std::fs::rename(path, &bad) {
            Ok(()) => self.ctx.log(&format!(
                "pollResults: unparseable result quarantined as {}",
                bad.display()
            )),
            Err(e) => self.ctx.log(&format!(
                "pollResults: unparseable result {} ({e})",
                path.display()
            )),
        }
    }

    /// Turn one result payload into a delivered Telegram message.
    fn deliver(&self, res: &Value, info: Option<&WorkerPending>) {
        // The WORKER's echo wins over the coordinator's recollection, and
        // both lose to a hardcoded fallback.
        let engine = str_field(res, "engine")
            .or_else(|| info.map(|i| i.engine.clone()))
            .unwrap_or_else(|| FALLBACK_ENGINE.to_string());
        let engine_label = self.engine_label(&engine);

        // Explicitly the LANE's target, never the active one: switching
        // targets mid-flight must not file this session elsewhere.
        if let Some(session) = str_field(res, "sessionId") {
            self.ctx.set_session(
                &self.target,
                &engine,
                info.and_then(|i| i.agent.as_deref()),
                Some(session),
            );
        }

        let body = self.result_text(res, &engine_label);
        let duration = match info {
            Some(i) => crate::render::fmt_dur(self.ctx.now_ms() - i.dispatched_ms),
            None => DURATION_UNKNOWN.to_string(),
        };
        let final_text = self.ctx.prompt(
            tpl::FINAL,
            &[
                ("text", &body),
                ("engine", &engine_label),
                ("target", &self.label()),
                ("duration", &duration),
            ],
        );
        deliver_final(
            &self.tg,
            self.ctx.as_ref(),
            &final_text,
            info.and_then(|i| i.status_message_id),
        );
    }

    /// `res.text || (res.error ? … : res.code ? … : '(no output)')`.
    ///
    /// Note that a clean exit (`code === 0`) is FALSY in the reference, so a
    /// zero exit with no text yields `(no output)`, not an exit-code error.
    /// A payload with no `code` at all — the shape the worker sends when the
    /// spawn itself failed — is likewise not an exit error.
    fn result_text(&self, res: &Value, engine_label: &str) -> String {
        if let Some(text) = str_field(res, "text") {
            return text;
        }
        if let Some(error) = str_field(res, "error") {
            return self.ctx.prompt(tpl::ERROR_MAC, &[("error", &error)]);
        }
        match res.get("code").and_then(Value::as_i64) {
            Some(code) if code != 0 => self.ctx.prompt(
                tpl::ERROR_EXIT_MAC,
                &[("engine", engine_label), ("code", &code.to_string())],
            ),
            _ => self.ctx.prompt(tpl::NO_OUTPUT, &[]),
        }
    }

    /// `cancelQueuedMac()` — drop every job the worker has not claimed yet.
    ///
    /// A job file still sitting in `jobs/` is cancellable; one that is gone
    /// has been claimed and cannot be interrupted remotely. The status
    /// message of a cancelled job is edited to `🛑 Cancelled.` with NO
    /// parse_mode and an EMPTY extra, which leaves the ⏹ Stop keyboard
    /// attached — deliberate in the reference, preserved here.
    pub fn cancel_queued(&self) -> CancelOutcome {
        // Collect first: JS tolerates deleting from a Map mid-iteration, Rust
        // does not, and the insertion order is what the edits must follow.
        let ids: Vec<String> = self.pending.lock().expect("pending").keys().cloned().collect();

        let mut out = CancelOutcome::default();
        for id in ids {
            let job_path = self.paths.jobs_dir.join(format!("{id}.json"));
            if !job_path.exists() {
                out.running += 1; // already claimed by the worker
                continue;
            }
            // Racy by construction: the worker can claim between the exists()
            // and this unlink. The failure is swallowed and the job counted
            // as cancelled, exactly as in the reference.
            let _ = std::fs::remove_file(&job_path);

            let info = self.pending.lock().expect("pending").shift_remove(&id);
            if let Some(status_id) = info.and_then(|i| i.status_message_id) {
                let text = self.ctx.prompt(tpl::CANCELLED, &[]);
                self.tg.edit_message(status_id, &text, None, Some(&json!({})));
            }
            out.cancelled += 1;
        }
        out
    }
}

/// A JSON string field, treating `null`, a non-string and `""` alike as
/// absent — the JS truthiness the result-text ladder is built on.
fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stackhour_core::registry::{EngineDef, Registry};
    use std::sync::atomic::{AtomicI64, Ordering};

    /// One recorded `set_session` call: (target, engine, agent, id).
    type SessionWrite = (String, String, Option<String>, Option<String>);

    /// A [`WorkerContext`] backed by the real registry defaults, so the
    /// template bodies under test are the shipped ones.
    pub(crate) struct FakeCtx {
        pub reg: Registry,
        pub now: AtomicI64,
        pub sessions: Mutex<Vec<SessionWrite>>,
        pub logs: Mutex<Vec<String>>,
        pub stored: Mutex<IndexMap<String, String>>,
        pub label: String,
    }

    impl FakeCtx {
        pub fn new() -> FakeCtx {
            FakeCtx {
                reg: stackhour_core::registry::load(std::path::Path::new("/nonexistent-cfg-dir")),
                now: AtomicI64::new(1_000_000),
                sessions: Mutex::new(Vec::new()),
                logs: Mutex::new(Vec::new()),
                stored: Mutex::new(IndexMap::new()),
                label: "🖥️ Mac".to_string(),
            }
        }
    }

    impl LaneContext for FakeCtx {
        fn engine(&self, name: &str) -> Option<EngineDef> {
            self.reg.engines.get(name).cloned()
        }
        fn target(&self, _name: &str, _engine: &str) -> Option<crate::local_lane::LocalTarget> {
            None // a worker target is never a local one
        }
        fn prompt(&self, name: &str, vars: &[(&str, &str)]) -> String {
            self.reg.prompts.render(name, vars)
        }
        fn control_keyboard(&self) -> Value {
            json!({ "inline_keyboard": [] })
        }
        fn session(&self, target: &str, engine: &str, agent: Option<&str>) -> Option<String> {
            self.stored
                .lock()
                .unwrap()
                .get(&key(target, engine, agent))
                .cloned()
        }
        fn set_session(&self, target: &str, engine: &str, agent: Option<&str>, id: Option<String>) {
            self.sessions.lock().unwrap().push((
                target.to_string(),
                engine.to_string(),
                agent.map(str::to_string),
                id.clone(),
            ));
            if let Some(id) = id {
                self.stored.lock().unwrap().insert(key(target, engine, agent), id);
            }
        }
        fn log(&self, line: &str) {
            self.logs.lock().unwrap().push(line.to_string());
        }
        fn now_ms(&self) -> i64 {
            self.now.load(Ordering::SeqCst)
        }
        fn default_target(&self) -> String {
            "gcp".to_string()
        }
    }

    impl WorkerContext for FakeCtx {
        fn worker_label(&self, target: &str) -> String {
            if target == "mac" {
                self.label.clone()
            } else {
                target.to_string()
            }
        }
    }

    fn key(target: &str, engine: &str, agent: Option<&str>) -> String {
        match agent {
            Some(a) => format!("{target}:{engine}@{a}"),
            None => format!("{target}:{engine}"),
        }
    }

    fn lane_for(ctx: Arc<FakeCtx>, dir: &std::path::Path, target: &str) -> WorkerLane {
        let paths = BridgePaths::from_runtime_dir(dir);
        paths.ensure_dirs().unwrap();
        // A Tg pointed at an unroutable port: these tests exercise the lane's
        // decisions, not its transport. `tg()` degrades every failure to None,
        // which is precisely the "the message did not happen" path.
        let mut cfg =
            crate::telegram::TgConfig::new("test-token", 1).with_api_root("http://127.0.0.1:1/".to_string());
        cfg.backoff_base_ms = 0;
        cfg.attempts = 1;
        WorkerLane::new(Arc::new(Tg::with_config(cfg)), ctx, paths, target)
    }

    fn lane_with(ctx: Arc<FakeCtx>, dir: &std::path::Path) -> WorkerLane {
        lane_for(ctx, dir, "mac")
    }

    // ---- the result-text ladder ----

    #[test]
    fn result_text_prefers_text_then_error_then_a_nonzero_exit() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(ctx, tmp.path());

        // Any text at all wins, even when an error is also present.
        assert_eq!(
            lane.result_text(&json!({ "text": "hi", "error": "boom", "code": 3 }), "Claude"),
            "hi"
        );
        assert_eq!(
            lane.result_text(&json!({ "text": "", "error": "boom" }), "Claude"),
            "⚠️ Mac error: boom"
        );
        assert_eq!(
            lane.result_text(&json!({ "code": 3 }), "Codex"),
            "⚠️ Codex exited on Mac (code 3)."
        );
    }

    /// `res.code === 0` is FALSY in the reference, so a clean exit with no
    /// output is '(no output)', not 'exited (code 0)'.
    #[test]
    fn a_clean_exit_with_no_text_is_no_output() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(ctx, tmp.path());
        assert_eq!(lane.result_text(&json!({ "code": 0 }), "Claude"), "(no output)");
        // The spawn-failed shape: no `code` key at all.
        assert_eq!(lane.result_text(&json!({}), "Claude"), "(no output)");
        assert_eq!(
            lane.result_text(&json!({ "code": null }), "Claude"),
            "(no output)"
        );
    }

    // ---- dispatch ----

    #[test]
    fn dispatch_writes_the_job_with_the_stored_session_and_tracks_it() {
        let ctx = Arc::new(FakeCtx::new());
        ctx.set_session("mac", "codex", None, Some("prev-session".into()));
        ctx.sessions.lock().unwrap().clear();
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(Arc::clone(&ctx), tmp.path());

        let id = lane
            .dispatch("do the thing", "codex", None, None)
            .expect("dispatched");
        assert_eq!(lane.pending_len(), 1);

        let body = std::fs::read_to_string(tmp.path().join("jobs").join(format!("{id}.json"))).unwrap();
        let job: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(job["id"], id);
        assert_eq!(job["prompt"], "do the thing");
        assert_eq!(job["engine"], "codex");
        assert_eq!(job["media"], Value::Null);
        assert_eq!(job["sessionId"], "prev-session");
        assert_eq!(job["target"], "mac", "the job is stamped with the lane's target");
        assert_eq!(job["ts"], 1_000_000);
    }

    /// Two lanes over the same runtime dir: each stamps its own target, reads
    /// its own session key, and a targeted claim only surfaces its own jobs.
    #[test]
    fn two_lanes_stamp_their_own_target_and_targeted_claims_stay_separate() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let mac = lane_for(Arc::clone(&ctx), tmp.path(), "mac");
        let pi = lane_for(Arc::clone(&ctx), tmp.path(), "pi");

        let mac_id = mac.dispatch("for the mac", "claude", None, None).unwrap();
        let pi_id = pi.dispatch("for the pi", "codex", None, None).unwrap();

        let paths = BridgePaths::from_runtime_dir(tmp.path());
        let claimed =
            jobs::try_claim_target(&paths.jobs_dir, &paths.inprogress_dir, Some("pi")).expect("pi's job");
        let job: Value = serde_json::from_str(&claimed).unwrap();
        assert_eq!(job["id"], pi_id.as_str());
        assert_eq!(job["target"], "pi");
        assert!(
            paths.jobs_dir.join(format!("{mac_id}.json")).exists(),
            "the mac job must not be visible to a pi claim"
        );
        assert_eq!(
            jobs::try_claim_target(&paths.jobs_dir, &paths.inprogress_dir, Some("pi")),
            None
        );

        // Results are filed under each lane's own target.
        write_result(tmp.path(), &pi_id, json!({ "text": "done", "sessionId": "s-pi" }));
        assert_eq!(pi.poll_matched(), 1);
        assert_eq!(
            ctx.sessions.lock().unwrap().last().unwrap().0,
            "pi",
            "the session files under the LANE's target"
        );
    }

    /// A matched-only poll leaves other lanes' results and orphans alone;
    /// the designated orphan sweeper takes them.
    #[test]
    fn poll_matched_leaves_foreign_results_for_the_orphan_sweeper() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let mac = lane_for(Arc::clone(&ctx), tmp.path(), "mac");
        let pi = lane_for(Arc::clone(&ctx), tmp.path(), "pi");

        // An orphan (no pending anywhere) and a result pending on `mac`.
        write_result(
            tmp.path(),
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            json!({ "text": "orphan" }),
        );
        let mac_id = mac.dispatch("mine", "claude", None, None).unwrap();
        write_result(tmp.path(), &mac_id, json!({ "text": "mac's" }));

        assert_eq!(pi.poll_matched(), 0, "pi owns neither result");
        assert_eq!(
            std::fs::read_dir(tmp.path().join("results")).unwrap().count(),
            2,
            "poll_matched must not consume foreign results"
        );
        assert_eq!(mac.poll_matched(), 1, "mac takes its own, not the orphan");
        assert_eq!(mac.poll_results(), 1, "the full poll sweeps the orphan");
        assert_eq!(std::fs::read_dir(tmp.path().join("results")).unwrap().count(), 0);
    }

    /// The media object is serialised VERBATIM, coordinator-absolute path and
    /// all. The worker scps the bytes and rebuilds the prompt with a local
    /// path; the coordinator transfers nothing.
    #[test]
    fn dispatch_carries_the_media_object_through_unchanged() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(ctx, tmp.path());
        let media = json!({
            "path": "/home/nikita/.claude-remote/media/1-abc.jpg",
            "kind": "image", "mime": "image/jpeg", "name": "telegram-photo.jpg", "size": 1234,
        });
        let id = lane
            .dispatch("look", "claude", None, Some(media.clone()))
            .unwrap();
        let job: Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("jobs").join(format!("{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(job["media"], media);
    }

    /// A job with no stored session writes an explicit `null`, not a missing
    /// key — the worker reads `job.sessionId` and must see the difference
    /// between "fresh" and a malformed payload.
    #[test]
    fn dispatch_writes_a_null_session_when_there_is_none() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(ctx, tmp.path());
        let id = lane.dispatch("x", "claude", None, None).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("jobs").join(format!("{id}.json"))).unwrap();
        assert!(body.contains(r#""sessionId":null"#), "{body}");
    }

    // ---- poll_results ----

    fn write_result(dir: &std::path::Path, id: &str, payload: Value) {
        std::fs::write(
            dir.join("results").join(format!("{id}.json")),
            payload.to_string(),
        )
        .unwrap();
    }

    #[test]
    fn a_result_persists_its_session_under_the_lane_target_explicitly() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(Arc::clone(&ctx), tmp.path());
        let id = lane.dispatch("hi", "codex", None, None).unwrap();

        write_result(
            tmp.path(),
            &id,
            json!({ "engine": "codex", "text": "done", "sessionId": "s-next", "code": 0 }),
        );
        assert_eq!(lane.poll_results(), 1);

        assert_eq!(
            *ctx.sessions.lock().unwrap(),
            vec![(
                "mac".to_string(),
                "codex".to_string(),
                None,
                Some("s-next".to_string())
            )],
            "the session must be filed under the lane's target, never the active one"
        );
        assert_eq!(lane.pending_len(), 0, "the pending entry is consumed");
        assert!(!tmp.path().join("results").join(format!("{id}.json")).exists());
    }

    /// The worker's echoed engine beats the coordinator's recollection, and a
    /// payload with neither is attributed to the fallback engine.
    #[test]
    fn the_engine_attribution_ladder_is_worker_then_pending_then_fallback() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(Arc::clone(&ctx), tmp.path());

        // (1) the worker's echo wins over the dispatched engine
        let id = lane.dispatch("a", "claude", None, None).unwrap();
        write_result(tmp.path(), &id, json!({ "engine": "codex", "sessionId": "s1" }));
        lane.poll_results();
        assert_eq!(ctx.sessions.lock().unwrap()[0].1, "codex");

        // (2) no echo: the pending entry's engine
        ctx.sessions.lock().unwrap().clear();
        let id = lane.dispatch("b", "codex", None, None).unwrap();
        write_result(tmp.path(), &id, json!({ "sessionId": "s2" }));
        lane.poll_results();
        assert_eq!(ctx.sessions.lock().unwrap()[0].1, "codex");

        // (3) neither — an orphan result after a coordinator restart
        ctx.sessions.lock().unwrap().clear();
        write_result(
            tmp.path(),
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            json!({ "sessionId": "s3" }),
        );
        lane.poll_results();
        assert_eq!(ctx.sessions.lock().unwrap()[0].1, FALLBACK_ENGINE);
    }

    /// A result whose pending entry is gone (restart) is still delivered; only
    /// the duration footer degrades.
    #[test]
    fn an_orphan_result_is_still_delivered_with_a_done_footer() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(Arc::clone(&ctx), tmp.path());
        write_result(
            tmp.path(),
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            json!({ "engine": "claude", "text": "answer" }),
        );
        assert_eq!(lane.poll_results(), 1);
        // Nothing panicked, nothing was left behind, and no session was set.
        assert!(ctx.sessions.lock().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(tmp.path().join("results")).unwrap().count(), 0);
    }

    #[test]
    fn a_missing_session_id_leaves_the_stored_session_untouched() {
        let ctx = Arc::new(FakeCtx::new());
        ctx.set_session("mac", "claude", None, Some("keep-me".into()));
        ctx.sessions.lock().unwrap().clear();
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(Arc::clone(&ctx), tmp.path());

        let id = lane.dispatch("x", "claude", None, None).unwrap();
        write_result(tmp.path(), &id, json!({ "text": "ok", "sessionId": null }));
        lane.poll_results();
        assert!(
            ctx.sessions.lock().unwrap().is_empty(),
            "a falsy sessionId must never clear a stored session"
        );
        assert_eq!(ctx.session("mac", "claude", None).as_deref(), Some("keep-me"));
    }

    /// The declared divergence: the reference re-reads a corrupt result file
    /// forever at 1 Hz, silently. Quarantining it breaks the hot loop and
    /// leaves a trace, and does not touch any result that parses.
    #[test]
    fn an_unparseable_result_is_quarantined_once_instead_of_looping_forever() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(Arc::clone(&ctx), tmp.path());
        let id = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
        std::fs::write(
            tmp.path().join("results").join(format!("{id}.json")),
            "{ truncated",
        )
        .unwrap();

        assert_eq!(lane.poll_results(), 0, "nothing was delivered");
        assert_eq!(lane.poll_results(), 0, "and the next tick finds nothing to redo");
        assert!(!tmp.path().join("results").join(format!("{id}.json")).exists());
        assert!(tmp.path().join("results").join(format!("{id}.json.bad")).exists());
        assert_eq!(ctx.logs.lock().unwrap().len(), 1, "logged exactly once");
    }

    #[test]
    fn a_missing_results_dir_is_not_an_error() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(ctx, tmp.path());
        std::fs::remove_dir_all(tmp.path().join("results")).unwrap();
        assert_eq!(lane.poll_results(), 0);
    }

    // ---- cancellation ----

    #[test]
    fn cancel_removes_queued_jobs_and_counts_claimed_ones_as_running() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(Arc::clone(&ctx), tmp.path());

        let queued = lane.dispatch("a", "claude", None, None).unwrap();
        let claimed = lane.dispatch("b", "claude", None, None).unwrap();
        // Simulate the worker claiming the second job.
        std::fs::rename(
            tmp.path().join("jobs").join(format!("{claimed}.json")),
            tmp.path().join("inprogress").join(format!("{claimed}.json")),
        )
        .unwrap();

        let out = lane.cancel_queued();
        assert_eq!(
            out,
            CancelOutcome {
                cancelled: 1,
                running: 1
            }
        );
        assert!(!tmp.path().join("jobs").join(format!("{queued}.json")).exists());
        assert!(
            tmp.path()
                .join("inprogress")
                .join(format!("{claimed}.json"))
                .exists(),
            "cancellation must never touch inprogress/"
        );
        assert_eq!(
            lane.pending_len(),
            1,
            "the claimed job stays pending — its result is still coming"
        );
    }

    #[test]
    fn cancelling_an_empty_queue_reports_nothing() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(ctx, tmp.path());
        assert_eq!(lane.cancel_queued(), CancelOutcome::default());
    }

    /// A job the user cancelled while the worker was claiming it still
    /// returns a result. That result must be delivered rather than panicking
    /// on a missing pending entry — the documented race.
    #[test]
    fn a_result_for_a_cancelled_job_is_still_delivered() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(Arc::clone(&ctx), tmp.path());
        let id = lane.dispatch("a", "claude", None, None).unwrap();
        assert_eq!(lane.cancel_queued().cancelled, 1);
        assert_eq!(lane.pending_len(), 0);

        write_result(tmp.path(), &id, json!({ "text": "ran anyway" }));
        assert_eq!(lane.poll_results(), 1);
    }

    // ---- heartbeat wiring ----

    #[test]
    fn the_lane_reads_the_heartbeat_live() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let lane = lane_with(ctx, tmp.path());
        assert!(!lane.worker_alive());
        jobs::beat(&tmp.path().join("worker-heartbeat"));
        assert!(lane.worker_alive(), "no caching — the banner reads it per render");
    }

    /// Per-lane liveness: a lane's own heartbeat file wins over the legacy
    /// shared one, and a lane without its own file falls back to it.
    #[test]
    fn each_lane_reads_its_own_heartbeat_with_the_legacy_file_as_fallback() {
        let ctx = Arc::new(FakeCtx::new());
        let tmp = tempfile::tempdir().unwrap();
        let mac = lane_for(Arc::clone(&ctx), tmp.path(), "mac");
        let pi = lane_for(Arc::clone(&ctx), tmp.path(), "pi");

        jobs::beat(&tmp.path().join("worker-heartbeat-pi"));
        assert!(pi.worker_alive());
        assert!(!mac.worker_alive(), "pi's heartbeat says nothing about mac");

        // The legacy shared file flips lanes WITHOUT their own file online.
        jobs::beat(&tmp.path().join("worker-heartbeat"));
        assert!(mac.worker_alive(), "the legacy fallback covers the Node worker");
        std::fs::write(
            tmp.path().join("worker-heartbeat-pi"),
            (jobs::now_ms() - 120_000).to_string(),
        )
        .unwrap();
        assert!(!pi.worker_alive(), "a lane with its own stale file is offline");
    }
}
