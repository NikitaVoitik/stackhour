//! The filesystem job protocol between coordinator and mac worker.
//!
//! Three directories and one file, all under the runtime dir:
//!
//! ```text
//! jobs/<uuid-v4>.json        written by the coordinator, claimed by the Mac
//! inprogress/<uuid-v4>.json  claimed but not yet returned
//! results/<uuid-v4>.json     the worker's answer, consumed by the coordinator
//! worker-heartbeat           bare ms epoch, rewritten by every claim poll
//! ```
//!
//! `claim` (hidden verb `stackhour bridge claim`, invoked over SSH by the
//! Mac) polls for ~25s at 1s cadence, writes the heartbeat EVERY iteration,
//! takes files in lexicographic order, tolerates rename races (a lost rename
//! just moves to the next file), prints the raw job JSON to stdout and exits
//! 0. `return` (`stackhour bridge return <id>`) gates on the strict UUID
//! regex (exit 2), pipes stdin to `results/<id>.json.tmp` then renames it
//! into place (direct-write fallback), and clears the inprogress marker. The
//! installer writes node-invokable claim.mjs/return.mjs shims that exec these
//! verbs, so the on-disk protocol stays byte-compatible with a half-migrated
//! pairing (Rust coordinator + Node worker, or the reverse).
//!
//! Two deliberate divergences from the reference, both strict improvements
//! that the JS on the other side of the protocol tolerates unchanged:
//!
//! 1. [`write_job`] and [`beat`] publish via tmp+rename. The JS writes job
//!    files straight into the watched `jobs/` directory, so a `claim` racing
//!    the write can hand the worker truncated JSON — which the worker logs as
//!    `bad job json` and DROPS, losing the job and stranding the pending
//!    entry forever. The tmp file is named `.json.tmp`, which the `.json`
//!    filter already skips.
//! 2. [`run_return_with`] validates the job id. The LIVE
//!    `~/.claude-remote/return.mjs` does not: `argv[2]` is joined straight
//!    into a path, so `../../etc/foo` writes outside `results/`. The repo
//!    copy already fixed this; the gate is kept here.
//!
//! Not fixed, on purpose: `claim` sorts UUID filenames lexicographically, so
//! the queue drains in random order despite the JS comment claiming "oldest
//! pending job". Changing the filename scheme would break a mixed Node/Rust
//! pairing, so the random order is preserved.

use serde_json::{Map, Value};
use stackhour_core::Result;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a `claim` invocation blocks waiting for a job before exiting
/// empty-handed. The Mac worker calls it back-to-back, so this is really the
/// SSH connection-reuse period, not a timeout.
pub const CLAIM_DEADLINE: Duration = Duration::from_millis(25_000);

/// The gap between claim polls — and therefore the heartbeat cadence.
pub const CLAIM_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// How stale the heartbeat may be before the coordinator calls the Mac
/// offline. At a 1s cadence this tolerates ~60 consecutive missed polls or
/// SSH hiccups.
pub const HEARTBEAT_WINDOW: Duration = Duration::from_millis(60_000);

/// `return.mjs: missing job id`.
pub const RETURN_MISSING_ID: &str = "return.mjs: missing job id";
/// `return.mjs: invalid job id`.
pub const RETURN_INVALID_ID: &str = "return.mjs: invalid job id";

/// Milliseconds since the unix epoch — the JS `Date.now()`.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The strict claim/return UUID validation gate.
///
/// The same regex the worker uses to decide whether a claimed job is worth
/// running: `^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`,
/// case-insensitive. Hand-rolled rather than compiled, so it costs nothing on
/// the hot path.
pub fn uuid_ok(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        let ok = match i {
            8 | 13 | 18 | 23 => c == b'-',
            // version nibble: 1-5
            14 => (b'1'..=b'5').contains(&c),
            // variant nibble: 8, 9, a or b
            19 => matches!(c.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'),
            _ => c.is_ascii_hexdigit(),
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Write a job file into `jobs_dir`; returns the generated job id (uuid v4).
///
/// The id is generated here and inserted as the FIRST key, so the on-disk key
/// order is `id, prompt, engine, media, sessionId, ts` exactly as
/// `dispatchMac` produces it. Compact JSON, no indent — the file is machine
/// traffic, not something a human reads.
pub fn write_job(jobs_dir: &Path, job: &Value) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let mut out: Map<String, Value> = Map::new();
    out.insert("id".into(), Value::String(id.clone()));
    if let Some(fields) = job.as_object() {
        for (k, v) in fields {
            if k == "id" {
                continue;
            }
            out.insert(k.clone(), v.clone());
        }
    }
    let body = serde_json::to_string(&Value::Object(out))?;

    std::fs::create_dir_all(jobs_dir)?;
    // tmp+rename: a `claim` polling this directory must never be able to
    // rename a half-written file out from under us. `.json.tmp` is invisible
    // to the `.json` filter on both sides.
    let final_path = jobs_dir.join(format!("{id}.json"));
    let tmp = jobs_dir.join(format!("{id}.json.tmp"));
    std::fs::write(&tmp, body.as_bytes())?;
    if let Err(e) = std::fs::rename(&tmp, &final_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(id)
}

/// Refresh the worker heartbeat.
///
/// A bare decimal millisecond epoch as ASCII with NO trailing newline — the
/// format the JS `beat()` writes and `workerAlive()` parses. Best-effort: a
/// failed heartbeat costs at most one poll's worth of liveness, and throwing
/// here would kill the claim loop.
pub fn beat(heartbeat_path: &Path) {
    let body = now_ms().to_string();
    // tmp+rename so a coordinator read landing mid-write cannot observe a
    // truncated string (which `Number('')` turns into 0, i.e. "offline").
    let tmp = heartbeat_path.with_extension("tmp");
    if std::fs::write(&tmp, body.as_bytes()).is_ok() && std::fs::rename(&tmp, heartbeat_path).is_ok()
    {
        return;
    }
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::write(heartbeat_path, body.as_bytes());
}

/// Whether the worker heartbeat is fresh (< 60s old).
///
/// Reproduces `Date.now() - Number(readFileSync(HEARTBEAT)) < 60000` including
/// its quirks: a missing or unreadable file is offline; an empty file parses
/// as `Number('') === 0` and is therefore also offline; anything non-numeric
/// is `NaN`, and every comparison against NaN is false, so offline again.
///
/// The one quirk that is not a parsing quirk: a heartbeat timestamp in the
/// FUTURE yields a negative difference, which is `< 60000`, so a Mac with a
/// skewed clock reads as online indefinitely. Preserved.
pub fn worker_alive(heartbeat_path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(heartbeat_path) else {
        return false;
    };
    // `Number(s)`: surrounding whitespace is ignored, "" is 0.
    let trimmed = text.trim();
    let value: f64 = if trimmed.is_empty() {
        0.0
    } else {
        match trimmed.parse::<f64>() {
            Ok(v) if v.is_finite() => v,
            // NaN / Infinity: every comparison against NaN is false, and an
            // infinite past is certainly not fresh.
            _ => return false,
        }
    };
    (now_ms() as f64 - value) < HEARTBEAT_WINDOW.as_millis() as f64
}

/// Atomically claim one job, returning its RAW file text.
///
/// The `rename` IS the claim: it is atomic within a filesystem, so two
/// concurrent claimers can never both win the same file. A lost race fails,
/// and the loser just moves to the next candidate.
pub fn try_claim(jobs_dir: &Path, inprogress_dir: &Path) -> Option<String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(jobs_dir)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    // Lexicographic on the whole filename, matching `readdirSync(...).sort()`.
    // The filenames are random UUIDs, so this is NOT FIFO — see the module
    // docs.
    files.sort();

    for path in files {
        let Some(name) = path.file_name() else { continue };
        let dest = inprogress_dir.join(name);
        if std::fs::rename(&path, &dest).is_err() {
            continue; // lost the race — try the next file
        }
        match std::fs::read_to_string(&dest) {
            Ok(body) => return Some(body),
            // Claimed but unreadable: the JS swallows this too, which strands
            // the job in inprogress/ forever. Nothing reaps it.
            Err(_) => continue,
        }
    }
    None
}

/// The `stackhour bridge claim` hidden verb. Returns the process exit code.
///
/// Blocks up to [`CLAIM_DEADLINE`], beating the heartbeat once per second.
/// Prints the claimed job's raw JSON (no trailing newline) on success and
/// nothing at all on timeout. BOTH outcomes exit 0 — the worker distinguishes
/// them by whether stdout was empty, exactly as it does against the JS.
pub fn run_claim(runtime_dir: &Path) -> i32 {
    let paths = crate::BridgePaths::from_runtime_dir(runtime_dir);
    let _ = std::fs::create_dir_all(&paths.jobs_dir);
    let _ = std::fs::create_dir_all(&paths.inprogress_dir);

    // Computed BEFORE the loop and checked AFTER the claim attempt, so a
    // claim always beats at least once and always makes at least one attempt.
    let deadline = now_ms() + CLAIM_DEADLINE.as_millis() as i64;
    loop {
        beat(&paths.heartbeat_path);
        if let Some(job) = try_claim(&paths.jobs_dir, &paths.inprogress_dir) {
            use std::io::Write as _;
            let mut out = std::io::stdout();
            let _ = out.write_all(job.as_bytes());
            let _ = out.flush();
            return 0;
        }
        if now_ms() > deadline {
            return 0;
        }
        std::thread::sleep(CLAIM_POLL_INTERVAL);
    }
}

/// The `stackhour bridge return <id>` hidden verb, reading the payload from
/// stdin. Returns the exit code (2 on a missing or invalid job id).
pub fn run_return(runtime_dir: &Path, id: &str) -> i32 {
    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        payload.clear();
    }
    run_return_with(runtime_dir, id, &payload)
}

/// [`run_return`] with the stdin payload supplied directly (testable core).
pub fn run_return_with(runtime_dir: &Path, id: &str, payload: &str) -> i32 {
    if id.is_empty() {
        eprintln!("{RETURN_MISSING_ID}");
        return 2;
    }
    // The id is joined straight into a path. Without this gate a crafted
    // `../..`-bearing id writes outside results/ — which is exactly what the
    // live Node return.mjs does today.
    if !uuid_ok(id) {
        eprintln!("{RETURN_INVALID_ID}");
        return 2;
    }

    let paths = crate::BridgePaths::from_runtime_dir(runtime_dir);
    let _ = std::fs::create_dir_all(&paths.results_dir);

    // Empty stdin becomes the literal '{}', which parses cleanly and renders
    // as '(no output)' plus the footer. The user gets a message, not silence.
    let body = if payload.is_empty() { "{}" } else { payload };

    let final_path = paths.results_dir.join(format!("{id}.json"));
    let tmp = paths.results_dir.join(format!("{id}.json.tmp"));
    if std::fs::write(&tmp, body.as_bytes()).is_ok() {
        let _ = std::fs::remove_file(&final_path);
        if std::fs::rename(&tmp, &final_path).is_err() {
            // Non-atomic fallback: a failed rename is better served by a
            // possible torn read than by losing the result entirely.
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::write(&final_path, body.as_bytes());
        }
    } else {
        let _ = std::fs::write(&final_path, body.as_bytes());
    }

    let _ = std::fs::remove_file(paths.inprogress_dir.join(format!("{id}.json")));
    0
}

/// Delete media files older than 7 days (best-effort).
///
/// The sweep itself lives in [`crate::media::prune_media`] alongside the rest
/// of the attachment lifecycle; this is the fixed-horizon alias the
/// coordinator and worker startup paths call.
pub fn prune_media(dir: &Path) {
    crate::media::prune_media(dir, crate::media::PRUNE_MAX_AGE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rt() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        crate::BridgePaths::from_runtime_dir(tmp.path())
            .ensure_dirs()
            .unwrap();
        tmp
    }

    // ---- the uuid gate ----

    #[test]
    fn the_uuid_gate_accepts_v4_and_rejects_everything_else() {
        assert!(uuid_ok("3f2504e0-4f89-41d3-9a0c-0305e82c3301"));
        assert!(uuid_ok("3F2504E0-4F89-41D3-9A0C-0305E82C3301"), "case-insensitive");
        assert!(uuid_ok(&uuid::Uuid::new_v4().to_string()), "what we generate");

        for bad in [
            "",
            "not-a-uuid",
            // version nibble 0 and 6 are outside [1-5]
            "3f2504e0-4f89-01d3-9a0c-0305e82c3301",
            "3f2504e0-4f89-61d3-9a0c-0305e82c3301",
            // variant nibble outside [89ab]
            "3f2504e0-4f89-41d3-ca0c-0305e82c3301",
            // wrong length / wrong separators
            "3f2504e0-4f89-41d3-9a0c-0305e82c330",
            "3f2504e04f8941d39a0c0305e82c3301",
            "3f2504e0_4f89_41d3_9a0c_0305e82c3301",
            // non-hex
            "3f2504e0-4f89-41d3-9a0c-0305e82c330g",
        ] {
            assert!(!uuid_ok(bad), "should have been rejected: {bad:?}");
        }
    }

    /// The gate is the only thing standing between `argv[2]` and a path join.
    #[test]
    fn the_uuid_gate_rejects_path_traversal() {
        for evil in [
            "../../etc/passwd",
            "../../../home/nikita/.claude-remote/state",
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301/../../x",
        ] {
            assert!(!uuid_ok(evil), "traversal accepted: {evil}");
        }
    }

    // ---- write_job ----

    #[test]
    fn a_written_job_has_the_reference_key_order_and_a_fresh_id() {
        let tmp = rt();
        let jobs = tmp.path().join("jobs");
        let id = write_job(
            &jobs,
            &json!({
                "prompt": "hello",
                "engine": "codex",
                "media": null,
                "sessionId": null,
                "ts": 1_700_000_000_000i64,
            }),
        )
        .unwrap();
        assert!(uuid_ok(&id), "job ids must pass the worker's own gate");

        let body = std::fs::read_to_string(jobs.join(format!("{id}.json"))).unwrap();
        // Compact, no indent — machine traffic.
        assert!(!body.contains('\n'));
        assert_eq!(
            body,
            format!(
                r#"{{"id":"{id}","prompt":"hello","engine":"codex","media":null,"sessionId":null,"ts":1700000000000}}"#
            ),
            "key order is part of the on-disk contract with the Node worker"
        );
    }

    /// The tmp file must be invisible to a concurrent claimer, or the worker
    /// gets truncated JSON and silently drops the job.
    #[test]
    fn write_job_leaves_no_tmp_file_behind() {
        let tmp = rt();
        let jobs = tmp.path().join("jobs");
        let id = write_job(&jobs, &json!({ "prompt": "x" })).unwrap();
        let names: Vec<String> = std::fs::read_dir(&jobs)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![format!("{id}.json")]);
    }

    // ---- the heartbeat ----

    #[test]
    fn a_fresh_heartbeat_is_alive_and_a_stale_one_is_not() {
        let tmp = rt();
        let hb = tmp.path().join("worker-heartbeat");
        assert!(!worker_alive(&hb), "a missing file is offline");

        beat(&hb);
        assert!(worker_alive(&hb));

        // Exactly the on-disk format the JS writes: bare decimal ms, no
        // newline. Cross-implementation compatibility depends on it.
        let body = std::fs::read_to_string(&hb).unwrap();
        assert!(!body.ends_with('\n'), "no trailing newline: {body:?}");
        assert!(body.parse::<i64>().is_ok(), "bare decimal ms: {body:?}");

        std::fs::write(&hb, (now_ms() - 60_001).to_string()).unwrap();
        assert!(!worker_alive(&hb), "60s+ old is offline");
        std::fs::write(&hb, (now_ms() - 59_000).to_string()).unwrap();
        assert!(worker_alive(&hb), "59s old is still online");
    }

    /// `Number('')` is 0 and `Number('garbage')` is NaN; both read as offline
    /// in the JS, and a Rust port that parsed loosely would report a torn
    /// write as ONLINE.
    #[test]
    fn a_junk_heartbeat_reads_as_offline() {
        let tmp = rt();
        let hb = tmp.path().join("worker-heartbeat");
        for junk in ["", "   ", "nope", "12x34", "NaN"] {
            std::fs::write(&hb, junk).unwrap();
            assert!(!worker_alive(&hb), "junk read as online: {junk:?}");
        }
        // Whitespace around a real number is fine — Number() trims.
        std::fs::write(&hb, format!("  {}  ", now_ms())).unwrap();
        assert!(worker_alive(&hb));
    }

    /// Documented reference quirk, preserved: there is no lower bound, so a
    /// clock-skewed Mac reads as online forever.
    #[test]
    fn a_future_heartbeat_reads_as_online_like_the_reference() {
        let tmp = rt();
        let hb = tmp.path().join("worker-heartbeat");
        std::fs::write(&hb, (now_ms() + 10_000_000).to_string()).unwrap();
        assert!(worker_alive(&hb));
    }

    // ---- claim ----

    #[test]
    fn claim_takes_the_lexicographically_first_job_and_moves_it_to_inprogress() {
        let tmp = rt();
        let p = crate::BridgePaths::from_runtime_dir(tmp.path());
        for name in ["bbb", "aaa", "ccc"] {
            std::fs::write(
                p.jobs_dir.join(format!("{name}.json")),
                format!(r#"{{"id":"{name}"}}"#),
            )
            .unwrap();
        }
        // Non-.json entries are invisible, including our own tmp files.
        std::fs::write(p.jobs_dir.join("zzz.json.tmp"), "half").unwrap();

        let claimed = try_claim(&p.jobs_dir, &p.inprogress_dir).unwrap();
        assert_eq!(claimed, r#"{"id":"aaa"}"#, "raw file text, verbatim");
        assert!(!p.jobs_dir.join("aaa.json").exists());
        assert!(p.inprogress_dir.join("aaa.json").exists());

        assert_eq!(try_claim(&p.jobs_dir, &p.inprogress_dir).unwrap(), r#"{"id":"bbb"}"#);
        assert_eq!(try_claim(&p.jobs_dir, &p.inprogress_dir).unwrap(), r#"{"id":"ccc"}"#);
        assert_eq!(try_claim(&p.jobs_dir, &p.inprogress_dir), None);
        // The tmp file is still sitting there, unclaimed and unnoticed.
        assert!(p.jobs_dir.join("zzz.json.tmp").exists());
    }

    /// The rename is the mutual-exclusion primitive: no two claimers may ever
    /// win the same job.
    #[test]
    fn concurrent_claimers_never_get_the_same_job() {
        let tmp = rt();
        let p = crate::BridgePaths::from_runtime_dir(tmp.path());
        for i in 0..40 {
            std::fs::write(p.jobs_dir.join(format!("job{i:03}.json")), format!("{i}")).unwrap();
        }
        let claimed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let (jobs, prog, out) = (p.jobs_dir.clone(), p.inprogress_dir.clone(), claimed.clone());
            handles.push(std::thread::spawn(move || {
                while let Some(j) = try_claim(&jobs, &prog) {
                    out.lock().unwrap().push(j);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let mut got = claimed.lock().unwrap().clone();
        let total = got.len();
        got.sort();
        got.dedup();
        assert_eq!(got.len(), 40, "a job was lost");
        assert_eq!(total, 40, "a job was claimed twice");
    }

    #[test]
    fn a_claim_poll_beats_even_when_it_claims_nothing() {
        let tmp = rt();
        let p = crate::BridgePaths::from_runtime_dir(tmp.path());
        beat(&p.heartbeat_path);
        assert!(try_claim(&p.jobs_dir, &p.inprogress_dir).is_none());
        // The heartbeat is what keeps the coordinator calling the Mac online
        // while it idles, so it must not be conditional on finding work.
        assert!(worker_alive(&p.heartbeat_path));
    }

    #[test]
    fn run_claim_beats_and_returns_zero_immediately_when_a_job_is_waiting() {
        let tmp = rt();
        let p = crate::BridgePaths::from_runtime_dir(tmp.path());
        write_job(&p.jobs_dir, &json!({ "prompt": "hi" })).unwrap();
        let started = now_ms();
        assert_eq!(run_claim(tmp.path()), 0);
        assert!(
            now_ms() - started < 1000,
            "a waiting job must be claimed without a poll sleep"
        );
        assert!(worker_alive(&p.heartbeat_path));
        assert_eq!(std::fs::read_dir(&p.jobs_dir).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&p.inprogress_dir).unwrap().count(), 1);
    }

    // ---- return ----

    #[test]
    fn return_publishes_the_payload_and_clears_the_inprogress_marker() {
        let tmp = rt();
        let p = crate::BridgePaths::from_runtime_dir(tmp.path());
        let id = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
        std::fs::write(p.inprogress_dir.join(format!("{id}.json")), "{}").unwrap();

        let payload =
            r#"{"id":"x","engine":"codex","text":"done","sessionId":"s1","code":0,"error":null}"#;
        assert_eq!(run_return_with(tmp.path(), id, payload), 0);

        assert_eq!(
            std::fs::read_to_string(p.results_dir.join(format!("{id}.json"))).unwrap(),
            payload
        );
        assert!(!p.inprogress_dir.join(format!("{id}.json")).exists());
        // No `.json.tmp` may survive: the coordinator's `.json` filter skips
        // it, but leaving litter in results/ is still a bug.
        let names: Vec<String> = std::fs::read_dir(&p.results_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![format!("{id}.json")]);
    }

    /// Empty stdin becomes `{}` — the user gets '(no output)' plus a footer,
    /// not silence.
    #[test]
    fn return_turns_empty_stdin_into_an_empty_object() {
        let tmp = rt();
        let p = crate::BridgePaths::from_runtime_dir(tmp.path());
        let id = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
        assert_eq!(run_return_with(tmp.path(), id, ""), 0);
        assert_eq!(
            std::fs::read_to_string(p.results_dir.join(format!("{id}.json"))).unwrap(),
            "{}"
        );
    }

    #[test]
    fn return_overwrites_an_existing_result_for_the_same_id() {
        let tmp = rt();
        let p = crate::BridgePaths::from_runtime_dir(tmp.path());
        let id = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
        run_return_with(tmp.path(), id, r#"{"text":"first"}"#);
        run_return_with(tmp.path(), id, r#"{"text":"second"}"#);
        assert_eq!(
            std::fs::read_to_string(p.results_dir.join(format!("{id}.json"))).unwrap(),
            r#"{"text":"second"}"#
        );
    }

    #[test]
    fn return_rejects_a_missing_or_traversing_id_before_touching_the_disk() {
        let tmp = rt();
        let p = crate::BridgePaths::from_runtime_dir(tmp.path());
        assert_eq!(run_return_with(tmp.path(), "", "{}"), 2);
        for evil in ["../../pwned", "..", "not-a-uuid", "/etc/passwd"] {
            assert_eq!(run_return_with(tmp.path(), evil, r#"{"text":"pwn"}"#), 2, "{evil}");
        }
        assert_eq!(
            std::fs::read_dir(&p.results_dir).unwrap().count(),
            0,
            "a rejected id must write nothing at all"
        );
        assert!(!tmp.path().join("pwned.json").exists(), "traversal escaped results/");
    }

    // ---- the round trip ----

    /// The whole disk protocol end to end, which is what a mixed Node/Rust
    /// pairing actually depends on.
    #[test]
    fn a_job_survives_dispatch_claim_and_return() {
        let tmp = rt();
        let p = crate::BridgePaths::from_runtime_dir(tmp.path());

        let id = write_job(
            &p.jobs_dir,
            &json!({ "prompt": "ship it", "engine": "claude", "media": null,
                     "sessionId": "prev", "ts": now_ms() }),
        )
        .unwrap();

        let raw = try_claim(&p.jobs_dir, &p.inprogress_dir).expect("claimed");
        let job: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(job["id"], id);
        assert_eq!(job["prompt"], "ship it");
        assert_eq!(job["sessionId"], "prev");

        let payload = json!({ "id": id, "engine": "claude", "text": "ok",
                              "sessionId": "next", "code": 0, "error": null });
        assert_eq!(run_return_with(tmp.path(), &id, &payload.to_string()), 0);

        let result: Value = serde_json::from_str(
            &std::fs::read_to_string(p.results_dir.join(format!("{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(result["sessionId"], "next");
        assert_eq!(std::fs::read_dir(&p.inprogress_dir).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&p.jobs_dir).unwrap().count(), 0);
    }
}
