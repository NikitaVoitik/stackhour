//! The pull-worker daemon — the remote half of the worker lane.
//!
//! Outbound only. The worker never listens: it repeatedly SSHes into the GCP
//! coordinator to claim a `mac` job, runs the engine locally, and pipes the
//! result back through `return`. No inbound connectivity, no sudo, no Remote
//! Login — just the existing SSH key.
//!
//! One job at a time, strictly serial. The claim call blocks ~25s on the
//! remote side, so the loop is mostly one long-lived SSH per half-minute
//! rather than a busy poll, and it is that call which refreshes the
//! worker-heartbeat the coordinator reads.
//!
//! ## Structure
//!
//! Everything that touches the network is behind [`Remote`], and the engine
//! run is behind [`EngineRunner`]. [`Worker::poll_once`] is the loop body
//! MINUS the sleeps and returns the branch it took, so the whole state
//! machine — the SSH-failure, empty, bad-JSON and handled branches, the UUID
//! gate, the media path check, the resume retry and the payload shape — is
//! testable without an SSH server, an engine binary or a single second of
//! wall clock. [`Worker::run_forever`] adds the sleeps.
//!
//! ## The behaviours that are easy to get wrong
//!
//! * **An invalid job id strands the job.** The worker logs and RETURNS
//!   without calling `return`, so the file sits in `inprogress/` forever and
//!   the coordinator's status message says `working…` until it is restarted.
//!   Nothing reaps it. Preserved: the alternative is inventing a failure
//!   protocol the coordinator does not speak.
//! * **A media download failure short-circuits the engine.** The job does NOT
//!   run; the error is returned as the result. Otherwise the engine would be
//!   handed a prompt pointing at a file that is not there.
//! * **The media path prefix check is a security boundary**, not a
//!   convenience. `media.path` arrives from the coordinator and is fed to
//!   `scp <host>:<path>`. Without the `<remoteDir>/media/` prefix gate, a
//!   crafted job reads any file the SSH user can read.
//! * **The return has no retry.** One failed `ssh return` permanently loses
//!   that result: the payload is only logged, the inprogress marker stays,
//!   and the user gets nothing back. Reference behaviour, and the single
//!   worst failure mode in the lane — see the crate docs.
//! * **The resume retry is per-attempt.** `r.code && r.code !== 0 &&
//!   job.sessionId && !r.text` — a clean exit, a signal kill (`code === null`),
//!   no stored session, or ANY captured text all suppress it, and it runs at
//!   most once.

use crate::config;
use crate::engines::{self, RunResult};
use crate::jobs;
use crate::media;
use crate::{log_line, BridgePaths};
use serde_json::{Map, Value};
use stackhour_core::registry::Registry;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Back-off after an SSH failure — a GCP reboot or a flaky link.
pub const SSH_FAIL_SLEEP: Duration = Duration::from_millis(5000);
/// Back-off after a claim that found no work. The remote side already blocked
/// ~25s, so this is only to avoid hammering on a fast empty return.
pub const EMPTY_SLEEP: Duration = Duration::from_millis(500);
/// Back-off after an unparseable claim payload.
pub const BAD_JSON_SLEEP: Duration = Duration::from_millis(1000);

/// `-o` options passed verbatim to both `ssh` and `scp`.
const SSH_OPTS: [&str; 6] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ConnectTimeout=15",
];

/// The result of one remote command.
#[derive(Debug, Clone, Default)]
pub struct RemoteOut {
    /// -1 when the local `ssh` binary could not be spawned at all, matching
    /// the JS `child.on('error')` branch.
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Everything the worker does over the network, behind one trait so the loop
/// is testable without an SSH server.
pub trait Remote: Send + Sync {
    /// `ssh <opts> <host> <cmd>`, optionally piping `stdin`.
    fn ssh(&self, cmd: &str, stdin: Option<&str>) -> RemoteOut;
    /// `scp <opts> <host>:<remote_path> <local>`. `Err` carries the
    /// user-visible reason.
    fn scp(&self, remote_path: &str, local: &Path) -> Result<(), String>;
}

/// One engine attempt. The retry rule lives in the worker, not here, so a
/// test can count attempts.
pub trait EngineRunner: Send + Sync {
    fn run(&self, engine: &str, prompt: &str, session: Option<&str>) -> RunResult;
}

/// Which branch [`Worker::poll_once`] took, and how long the loop should
/// sleep before the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    /// The claim command itself failed.
    SshFailed,
    /// The claim returned nothing — no work this round.
    Empty,
    /// The claim returned something that is not JSON.
    BadJson,
    /// A job was claimed and run to completion (or to a reported error).
    Handled,
    /// A job was claimed but rejected by the UUID gate and stranded.
    Rejected,
}

impl Poll {
    /// The reference's back-off for this branch.
    pub fn sleep(self) -> Duration {
        match self {
            Poll::SshFailed => SSH_FAIL_SLEEP,
            Poll::Empty => EMPTY_SLEEP,
            Poll::BadJson => BAD_JSON_SLEEP,
            // A handled job loops straight back into the next claim; the
            // remote side does the blocking.
            Poll::Handled | Poll::Rejected => Duration::ZERO,
        }
    }
}

/// The Mac worker.
pub struct Worker {
    remote: Arc<dyn Remote>,
    runner: Arc<dyn EngineRunner>,
    /// Held for its prompt templates: the media prompt is rebuilt here
    /// against the local path.
    registry: Arc<Registry>,
    /// Where downloaded attachments land on the Mac.
    media_dir: PathBuf,
    /// The coordinator's runtime dir, as seen from the leader box.
    remote_dir: String,
    /// The target name this worker claims for (`worker-config.json`'s
    /// `target`). `None` = the legacy claim-anything mode: take the oldest
    /// job regardless of which target dispatched it.
    target: Option<String>,
    log_path: PathBuf,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        remote: Arc<dyn Remote>,
        runner: Arc<dyn EngineRunner>,
        registry: Arc<Registry>,
        media_dir: PathBuf,
        remote_dir: String,
        target: Option<String>,
        log_path: PathBuf,
    ) -> Worker {
        Worker {
            remote,
            runner,
            registry,
            media_dir,
            remote_dir,
            target,
            log_path,
        }
    }

    fn log(&self, msg: &str) {
        log_line(&self.log_path, msg);
    }

    /// One `bridge` invocation of the leader's own `stackhour` binary, as a
    /// shell command for SSH.
    ///
    /// `bridge install coordinator` copies the binary to `<remoteDir>/
    /// stackhour`, so the worker execs it directly. Every component is
    /// shell-quoted: `remote_dir` and `target` come from config, and `id`
    /// from a coordinator payload.
    ///
    /// `--runtime-dir` is passed explicitly because an SSH command carries no
    /// environment, so `$STACKHOUR_BRIDGE_HOME` would not survive the hop.
    /// It trails the positional argument, which `positional_after` skips.
    fn remote_cmd(&self, verb: &str, arg: Option<&str>) -> String {
        let dir = self.remote_dir.trim_end_matches('/');
        let mut cmd = format!(
            "{} bridge {verb}",
            config::shell_quote(&format!("{dir}/stackhour"))
        );
        if let Some(arg) = arg {
            cmd.push(' ');
            cmd.push_str(&config::shell_quote(arg));
        }
        cmd.push_str(" --runtime-dir ");
        cmd.push_str(&config::shell_quote(dir));
        cmd
    }

    /// One iteration of the claim loop, without the sleep.
    pub fn poll_once(&self) -> Poll {
        // A configured target rides along as `bridge claim <target>`; no
        // target is the legacy claim-anything mode.
        let cmd = self.remote_cmd("claim", self.target.as_deref());
        let res = self.remote.ssh(&cmd, None);
        if res.code != 0 {
            self.log(&format!(
                "claim ssh failed ({}): {}",
                res.code,
                truncate_utf16(res.stderr.trim(), 120)
            ));
            return Poll::SshFailed;
        }
        let out = res.stdout.trim();
        if out.is_empty() {
            return Poll::Empty; // no job this round (claim blocked ~25s)
        }
        let Ok(job) = serde_json::from_str::<Value>(out) else {
            self.log(&format!("bad job json: {}", truncate_utf16(out, 120)));
            return Poll::BadJson;
        };
        self.handle(&job)
    }

    /// Run one claimed job and return its result to the coordinator.
    fn handle(&self, job: &Value) -> Poll {
        let id = job.get("id").and_then(Value::as_str).unwrap_or("");
        if !jobs::uuid_ok(id) {
            // No `return` call: the job stays in inprogress/ forever and the
            // coordinator's status message never resolves. Reference
            // behaviour — the coordinator speaks no failure protocol.
            self.log("rejected job with invalid id");
            return Poll::Rejected;
        }
        let prompt = job.get("prompt").and_then(Value::as_str).unwrap_or("");
        self.log(&format!("claimed {id}: {}", truncate_utf16(prompt, 60)));

        // Anything that is not exactly 'codex' runs on claude, including a
        // missing or unknown engine.
        let engine = match job.get("engine").and_then(Value::as_str) {
            Some("codex") => "codex",
            _ => "claude",
        };
        let session = job
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());

        // A media job whose bytes cannot be fetched must NOT run: the prompt
        // would point at a file that is not there.
        let (prompt, mut result) = match job.get("media").filter(|m| !m.is_null()) {
            None => (prompt.to_string(), None),
            Some(m) => match self.download_media(m) {
                Ok(local) => (self.rebuild_media_prompt(prompt, m, &local), None),
                Err(e) => (
                    prompt.to_string(),
                    Some(RunResult {
                        error: Some(format!("Could not download Telegram attachment: {e}")),
                        ..RunResult::default()
                    }),
                ),
            },
        };

        if result.is_none() {
            result = Some(self.runner.run(engine, &prompt, session));
        }
        let mut r = result.expect("set on both branches");

        // Resume may fail on a stale or rotated session — retry fresh, once.
        if r.code.is_some_and(|c| c != 0) && session.is_some() && r.text.is_empty() {
            self.log(&engines::resume_retry_log_line(engine, r.code));
            r = self.runner.run(engine, &prompt, None);
        }

        self.return_result(id, engine, &r);
        Poll::Handled
    }

    /// Fetch an attachment named by a job onto the Mac.
    ///
    /// The prefix check is a security boundary: `media.path` comes from the
    /// coordinator and is handed straight to `scp <host>:<path>`. Without it,
    /// a crafted job reads any file the SSH user can read.
    fn download_media(&self, media: &Value) -> Result<PathBuf, String> {
        let path = media.get("path").and_then(Value::as_str).unwrap_or("");
        let want = format!("{}/media/", self.remote_dir);
        if path.is_empty() || !path.starts_with(&want) {
            return Err("Invalid remote media path.".to_string());
        }
        // `basename` only — the prefix check already bounds the directory,
        // and this bounds the local write.
        let name = Path::new(path)
            .file_name()
            .ok_or_else(|| "Invalid remote media path.".to_string())?;
        let local = self.media_dir.join(name);
        let _ = std::fs::create_dir_all(&self.media_dir);
        self.remote.scp(path, &local)?;
        Ok(local)
    }

    /// Rebuild the media prompt against the path the file landed at LOCALLY.
    /// The coordinator's prompt named a GCP path that does not exist here.
    fn rebuild_media_prompt(&self, caption: &str, media: &Value, local: &Path) -> String {
        let saved = media::SavedMedia {
            path: local.to_path_buf(),
            kind: media
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            mime: media
                .get("mime")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: media
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            size: media.get("size").and_then(Value::as_u64).unwrap_or(0),
        };
        media::media_prompt_at(&self.registry.prompts, caption, &saved, &local.to_string_lossy())
    }

    /// Pipe the result payload into the coordinator's `return`.
    ///
    /// No retry: one failed SSH permanently loses this result. Reference
    /// behaviour, preserved, and logged loudly enough to diagnose.
    fn return_result(&self, id: &str, engine: &str, r: &RunResult) {
        let payload = result_payload(id, engine, r);
        let cmd = self.remote_cmd("return", Some(id));
        let out = self.remote.ssh(&cmd, Some(&payload));
        if out.code == 0 {
            self.log(&format!("returned {id}"));
        } else {
            self.log(&format!("return failed for {id}: {}", out.stderr.trim()));
        }
    }

    /// The claim loop, forever. Never returns and never propagates an error:
    /// a worker that exits stops answering the owner's phone.
    pub fn run_forever(&self, media_prune_dir: &Path) -> ! {
        media::prune_media(media_prune_dir, media::PRUNE_MAX_AGE);
        self.log("worker online");
        loop {
            let outcome = self.poll_once();
            let nap = outcome.sleep();
            if !nap.is_zero() {
                std::thread::sleep(nap);
            }
        }
    }
}

/// The result JSON the worker pipes into `return`.
///
/// Key order is the on-disk contract: `id, engine, text, sessionId, code,
/// error`. `code` is OMITTED entirely when the spawn itself failed — the JS
/// `JSON.stringify` drops an `undefined` — and the coordinator's
/// `res.code ?` ladder depends on the difference between an absent code and a
/// zero one. There is deliberately NO `stderr` field: it never crosses the
/// SSH hop, which is why a Mac-side crash shows the user only an exit code.
pub fn result_payload(id: &str, engine: &str, r: &RunResult) -> String {
    let mut out: Map<String, Value> = Map::new();
    out.insert("id".into(), Value::String(id.to_string()));
    out.insert("engine".into(), Value::String(engine.to_string()));
    out.insert("text".into(), Value::String(r.text.clone()));
    out.insert(
        "sessionId".into(),
        r.session_id
            .clone()
            .filter(|s| !s.is_empty())
            .map_or(Value::Null, Value::String),
    );
    if let Some(code) = r.code {
        out.insert("code".into(), Value::from(code));
    }
    out.insert(
        "error".into(),
        r.error
            .clone()
            .filter(|s| !s.is_empty())
            .map_or(Value::Null, Value::String),
    );
    serde_json::to_string(&Value::Object(out)).unwrap_or_else(|_| "{}".to_string())
}

/// `s.slice(0, n)` — UTF-16 code units, so a log line truncates exactly where
/// the JS truncates it.
fn truncate_utf16(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= n {
        return s.to_string();
    }
    String::from_utf16_lossy(&units[..n])
}

/// [`Remote`] over the real `ssh` and `scp` binaries.
pub struct SshRemote {
    /// `-i <key>`; omitted entirely when None (agent auth).
    pub key: Option<String>,
    /// `user@host`.
    pub host: String,
}

impl SshRemote {
    fn base_args(&self) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        if let Some(key) = &self.key {
            args.push("-i".into());
            args.push(key.clone());
        }
        args.extend(SSH_OPTS.iter().map(|s| (*s).to_string()));
        args
    }
}

impl Remote for SshRemote {
    fn ssh(&self, cmd: &str, stdin: Option<&str>) -> RemoteOut {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let mut args = self.base_args();
        args.push(self.host.clone());
        args.push(cmd.to_string());

        let spawned = Command::new("ssh")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            // The JS `child.on('error')` branch: code -1, the message as
            // stderr.
            Err(e) => {
                return RemoteOut {
                    code: -1,
                    stdout: String::new(),
                    stderr: e.to_string(),
                }
            }
        };
        // Always close stdin, with or without a payload — `return` blocks on
        // EOF and would otherwise hang forever.
        if let Some(mut pipe) = child.stdin.take() {
            if let Some(body) = stdin {
                let _ = pipe.write_all(body.as_bytes());
            }
            drop(pipe);
        }
        match child.wait_with_output() {
            Ok(out) => RemoteOut {
                code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => RemoteOut {
                code: -1,
                stdout: String::new(),
                stderr: e.to_string(),
            },
        }
    }

    fn scp(&self, remote_path: &str, local: &Path) -> Result<(), String> {
        let mut args = self.base_args();
        args.push(format!("{}:{remote_path}", self.host));
        args.push(local.to_string_lossy().into_owned());

        let out = std::process::Command::new("scp")
            .args(&args)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail = stderr.trim();
        let tail = truncate_tail_utf16(tail, 240);
        Err(format!("scp exited {}: {tail}", out.status.code().unwrap_or(-1)))
    }
}

/// `s.slice(-n)` — the LAST n UTF-16 code units.
fn truncate_tail_utf16(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= n {
        return s.to_string();
    }
    String::from_utf16_lossy(&units[units.len() - n..])
}

/// Run the worker daemon forever.
///
/// Wiring only: every piece it composes lives elsewhere —
/// [`crate::config::load_worker_cfg`] reads `worker-config.json`, the job
/// protocol lives in [`crate::jobs`], and the engines run themselves.
pub fn run_worker(runtime_dir: &Path) -> ! {
    let paths = BridgePaths::from_runtime_dir(runtime_dir);
    let _ = paths.ensure_dirs();
    let log_path = paths.runtime_dir.join("worker.log");

    let cfg = match crate::config::load_worker_cfg(&paths.worker_config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            log_line(&log_path, &format!("worker config error: {e}"));
            std::process::exit(1);
        }
    };
    let registry = Arc::new(stackhour_core::registry::load(&paths.runtime_dir));

    let remote: Arc<dyn Remote> = Arc::new(SshRemote {
        key: cfg.leader_key.clone(),
        host: cfg.leader_ssh.clone(),
    });
    let runner: Arc<dyn EngineRunner> = Arc::new(RegistryRunner {
        registry: Arc::clone(&registry),
        cfg: cfg.clone(),
    });
    let worker = Worker::new(
        remote,
        runner,
        registry,
        paths.media_dir.clone(),
        cfg.remote_dir.clone(),
        cfg.target,
        log_path,
    );
    worker.run_forever(&paths.media_dir)
}

/// [`EngineRunner`] backed by the registry's engine definitions and the
/// worker config's per-engine binaries and models.
///
/// `live_status` is FALSE: the worker runs claude without
/// `--include-partial-messages`, which is why a Mac job shows no live
/// activity in Telegram.
struct RegistryRunner {
    registry: Arc<Registry>,
    cfg: crate::config::WorkerCfg,
}

impl EngineRunner for RegistryRunner {
    fn run(&self, engine: &str, prompt: &str, session: Option<&str>) -> RunResult {
        let Some(def) = self.registry.engines.get(engine) else {
            return RunResult {
                error: Some(format!("unknown engine '{engine}'")),
                ..RunResult::default()
            };
        };
        let (bin, model) = if engine == "codex" {
            (Some(self.cfg.codex_bin.clone()), self.cfg.codex_model.clone())
        } else {
            (self.cfg.claude_bin.clone(), self.cfg.model.clone())
        };
        let req = engines::RunRequest {
            prompt: prompt.to_string(),
            session_id: session.map(str::to_string),
            model,
            permission_mode: self.cfg.permission_mode.clone(),
            cwd: self.cfg.cwd.as_ref().map(PathBuf::from),
            live_status: false,
            extra_path: self.cfg.extra_path.clone(),
            bin,
            ..engines::RunRequest::default()
        };
        engines::run_engine(def, req, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// A scripted [`Remote`]: every command is recorded, and the reply for
    /// each command PREFIX is configured up front.
    #[derive(Default)]
    struct FakeRemote {
        claim: Mutex<RemoteOut>,
        ret: Mutex<RemoteOut>,
        calls: Mutex<Vec<(String, Option<String>)>>,
        scps: Mutex<Vec<(String, PathBuf)>>,
        scp_err: Mutex<Option<String>>,
    }

    impl FakeRemote {
        fn claiming(job: &Value) -> FakeRemote {
            FakeRemote {
                claim: Mutex::new(RemoteOut {
                    code: 0,
                    stdout: job.to_string(),
                    stderr: String::new(),
                }),
                ..FakeRemote::default()
            }
        }
        fn returned_payload(&self) -> Value {
            let calls = self.calls.lock().unwrap();
            let body = calls
                .iter()
                .find(|(c, _)| c.contains("bridge return"))
                .unwrap_or_else(|| panic!("no return call; saw {calls:?}"))
                .1
                .clone()
                .expect("the payload is piped on stdin");
            drop(calls);
            serde_json::from_str(&body).expect("valid JSON payload")
        }
        fn returned(&self) -> bool {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .any(|(c, _)| c.contains("bridge return"))
        }
    }

    impl Remote for FakeRemote {
        fn ssh(&self, cmd: &str, stdin: Option<&str>) -> RemoteOut {
            self.calls
                .lock()
                .unwrap()
                .push((cmd.to_string(), stdin.map(str::to_string)));
            if cmd.contains("bridge claim") {
                self.claim.lock().unwrap().clone()
            } else {
                self.ret.lock().unwrap().clone()
            }
        }
        fn scp(&self, remote_path: &str, local: &Path) -> Result<(), String> {
            self.scps
                .lock()
                .unwrap()
                .push((remote_path.to_string(), local.to_path_buf()));
            let scp_error = self.scp_err.lock().unwrap().clone();
            match scp_error {
                Some(e) => Err(e),
                None => {
                    let _ = std::fs::create_dir_all(local.parent().unwrap());
                    let _ = std::fs::write(local, b"bytes");
                    Ok(())
                }
            }
        }
    }

    /// A scripted [`EngineRunner`] that records every attempt.
    struct FakeRunner {
        results: Mutex<Vec<RunResult>>,
        seen: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl FakeRunner {
        fn with(results: Vec<RunResult>) -> Arc<FakeRunner> {
            Arc::new(FakeRunner {
                results: Mutex::new(results),
                seen: Mutex::new(Vec::new()),
            })
        }
        fn attempts(&self) -> Vec<(String, String, Option<String>)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl EngineRunner for FakeRunner {
        fn run(&self, engine: &str, prompt: &str, session: Option<&str>) -> RunResult {
            self.seen.lock().unwrap().push((
                engine.to_string(),
                prompt.to_string(),
                session.map(str::to_string),
            ));
            let mut q = self.results.lock().unwrap();
            if q.is_empty() {
                RunResult::default()
            } else {
                q.remove(0)
            }
        }
    }

    struct Harness {
        worker: Worker,
        remote: Arc<FakeRemote>,
        runner: Arc<FakeRunner>,
        _dir: tempfile::TempDir,
    }

    fn harness(remote: FakeRemote, runner: Arc<FakeRunner>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let remote = Arc::new(remote);
        let registry = Arc::new(stackhour_core::registry::load(Path::new("/nonexistent-cfg")));
        let worker = Worker::new(
            Arc::clone(&remote) as Arc<dyn Remote>,
            Arc::clone(&runner) as Arc<dyn EngineRunner>,
            registry,
            dir.path().join("media"),
            "/remote/bridge".to_string(),
            None,
            dir.path().join("worker.log"),
        );
        Harness {
            worker,
            remote,
            runner,
            _dir: dir,
        }
    }

    fn job(extra: Value) -> Value {
        let mut base = json!({
            "id": "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            "prompt": "do the thing",
            "engine": "claude",
            "media": null,
            "sessionId": null,
            "ts": 1,
        });
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    // ---- the loop branches ----

    #[test]
    fn a_failed_claim_backs_off_for_five_seconds_and_runs_nothing() {
        let remote = FakeRemote {
            claim: Mutex::new(RemoteOut {
                code: 255,
                stdout: String::new(),
                stderr: "ssh: connect to host: Connection refused\n".into(),
            }),
            ..FakeRemote::default()
        };
        let h = harness(remote, FakeRunner::with(vec![]));
        assert_eq!(h.worker.poll_once(), Poll::SshFailed);
        assert_eq!(Poll::SshFailed.sleep(), SSH_FAIL_SLEEP);
        assert!(h.runner.attempts().is_empty());
    }

    #[test]
    fn an_empty_claim_is_the_idle_path() {
        let h = harness(FakeRemote::default(), FakeRunner::with(vec![]));
        // The default RemoteOut has code 0 and empty stdout.
        assert_eq!(h.worker.poll_once(), Poll::Empty);
        assert_eq!(Poll::Empty.sleep(), EMPTY_SLEEP);
        assert!(!h.remote.returned(), "an idle round must not call return");
    }

    #[test]
    fn an_unparseable_claim_payload_backs_off_without_running_anything() {
        let remote = FakeRemote {
            claim: Mutex::new(RemoteOut {
                code: 0,
                stdout: "<html>gateway timeout</html>".into(),
                stderr: String::new(),
            }),
            ..FakeRemote::default()
        };
        let h = harness(remote, FakeRunner::with(vec![]));
        assert_eq!(h.worker.poll_once(), Poll::BadJson);
        assert_eq!(Poll::BadJson.sleep(), BAD_JSON_SLEEP);
        assert!(h.runner.attempts().is_empty());
    }

    /// The documented dead end: an invalid id is neither run nor returned, so
    /// the job is stranded in inprogress/ and the user's status message never
    /// resolves.
    #[test]
    fn a_job_with_an_invalid_id_is_stranded_not_returned() {
        let h = harness(
            FakeRemote::claiming(&job(json!({ "id": "../../etc/passwd" }))),
            FakeRunner::with(vec![]),
        );
        assert_eq!(h.worker.poll_once(), Poll::Rejected);
        assert!(h.runner.attempts().is_empty());
        assert!(!h.remote.returned(), "no failure protocol exists to report this");
    }

    // ---- running a job ----

    #[test]
    fn a_claimed_job_runs_and_returns_the_reference_payload() {
        let h = harness(
            FakeRemote::claiming(&job(json!({ "engine": "codex", "sessionId": "s-prev" }))),
            FakeRunner::with(vec![RunResult {
                text: "the answer".into(),
                session_id: Some("s-next".into()),
                code: Some(0),
                ..RunResult::default()
            }]),
        );
        assert_eq!(h.worker.poll_once(), Poll::Handled);

        assert_eq!(
            h.runner.attempts(),
            vec![(
                "codex".to_string(),
                "do the thing".to_string(),
                Some("s-prev".to_string())
            )]
        );
        let payload = h.remote.returned_payload();
        assert_eq!(payload["id"], "3f2504e0-4f89-41d3-9a0c-0305e82c3301");
        assert_eq!(payload["engine"], "codex");
        assert_eq!(payload["text"], "the answer");
        assert_eq!(payload["sessionId"], "s-next");
        assert_eq!(payload["code"], 0);
        assert_eq!(payload["error"], Value::Null);
    }

    /// Anything that is not exactly `codex` is claude — including a missing
    /// or unknown engine name.
    #[test]
    fn the_engine_is_codex_or_claude_and_nothing_else() {
        for (given, expected) in [
            (json!("codex"), "codex"),
            (json!("claude"), "claude"),
            (json!("gpt-9"), "claude"),
            (Value::Null, "claude"),
        ] {
            let h = harness(
                FakeRemote::claiming(&job(json!({ "engine": given }))),
                FakeRunner::with(vec![]),
            );
            h.worker.poll_once();
            assert_eq!(h.runner.attempts()[0].0, expected);
            assert_eq!(h.remote.returned_payload()["engine"], expected);
        }
    }

    /// `code` is OMITTED when the spawn itself failed. The coordinator's
    /// `res.code ?` ladder distinguishes an absent code from a zero one, so
    /// writing `"code": null` would change what the user sees.
    #[test]
    fn a_spawn_failure_omits_the_code_key_entirely() {
        let h = harness(
            FakeRemote::claiming(&job(json!({}))),
            FakeRunner::with(vec![RunResult {
                error: Some("ENOENT: claude".into()),
                ..RunResult::default()
            }]),
        );
        h.worker.poll_once();
        let payload = h.remote.returned_payload();
        assert!(
            payload.get("code").is_none(),
            "an undefined code must be dropped, not nulled: {payload}"
        );
        assert_eq!(payload["error"], "ENOENT: claude");
        assert_eq!(payload["text"], "");
        assert_eq!(payload["sessionId"], Value::Null);
    }

    /// The exact key order the Node worker produces — the coordinator does
    /// not care, but a human diffing two bridges does.
    #[test]
    fn the_payload_key_order_matches_the_reference() {
        let r = RunResult {
            text: "t".into(),
            session_id: Some("s".into()),
            code: Some(1),
            error: Some("e".into()),
            ..RunResult::default()
        };
        assert_eq!(
            result_payload("the-id", "claude", &r),
            r#"{"id":"the-id","engine":"claude","text":"t","sessionId":"s","code":1,"error":"e"}"#
        );
    }

    // ---- the resume retry ----

    #[test]
    fn a_failed_resume_retries_once_with_no_session() {
        let h = harness(
            FakeRemote::claiming(&job(json!({ "sessionId": "stale" }))),
            FakeRunner::with(vec![
                RunResult {
                    code: Some(1),
                    ..RunResult::default()
                },
                RunResult {
                    text: "second time lucky".into(),
                    session_id: Some("fresh".into()),
                    code: Some(0),
                    ..RunResult::default()
                },
            ]),
        );
        h.worker.poll_once();
        let attempts = h.runner.attempts();
        assert_eq!(attempts.len(), 2, "exactly one retry");
        assert_eq!(attempts[0].2, Some("stale".to_string()));
        assert_eq!(attempts[1].2, None, "the retry must start fresh");
        assert_eq!(h.remote.returned_payload()["text"], "second time lucky");
    }

    /// Every one of the four suppressors, individually.
    #[test]
    fn the_resume_retry_is_suppressed_by_text_by_success_and_by_a_signal_kill() {
        let cases = [
            // captured text: the run said something, so it did not fail
            (
                json!("stale"),
                RunResult {
                    code: Some(1),
                    text: "partial".into(),
                    ..RunResult::default()
                },
            ),
            // clean exit
            (
                json!("stale"),
                RunResult {
                    code: Some(0),
                    ..RunResult::default()
                },
            ),
            // SIGTERM: code is None, not a failure to resume
            (
                json!("stale"),
                RunResult {
                    code: None,
                    ..RunResult::default()
                },
            ),
            // no stored session to have failed on
            (
                Value::Null,
                RunResult {
                    code: Some(1),
                    ..RunResult::default()
                },
            ),
        ];
        for (session, result) in cases {
            let h = harness(
                FakeRemote::claiming(&job(json!({ "sessionId": session }))),
                FakeRunner::with(vec![result]),
            );
            h.worker.poll_once();
            assert_eq!(h.runner.attempts().len(), 1, "retried for {session:?}");
        }
    }

    // ---- media ----

    #[test]
    fn a_media_job_is_downloaded_and_its_prompt_rebuilt_against_the_local_path() {
        let media = json!({
            "path": "/remote/bridge/media/1700-abc.jpg",
            "kind": "image", "mime": "image/jpeg", "name": "telegram-photo.jpg", "size": 99,
        });
        let h = harness(
            FakeRemote::claiming(&job(json!({ "prompt": "what is this?", "media": media }))),
            FakeRunner::with(vec![]),
        );
        h.worker.poll_once();

        let scps = h.remote.scps.lock().unwrap().clone();
        assert_eq!(scps.len(), 1);
        assert_eq!(scps[0].0, "/remote/bridge/media/1700-abc.jpg");
        let local = scps[0].1.clone();
        assert_eq!(local.file_name().unwrap(), "1700-abc.jpg");

        let prompt = &h.runner.attempts()[0].1;
        assert!(prompt.starts_with("what is this?\n\n"), "{prompt}");
        assert!(
            prompt.contains(&format!("saved locally at: {}", local.display())),
            "the prompt must name the LOCAL path, not the GCP one: {prompt}"
        );
        assert!(
            !prompt.contains("/remote/bridge/media/"),
            "the GCP path leaked into the prompt: {prompt}"
        );
    }

    /// The security boundary. `media.path` is attacker-influenced input that
    /// is handed to `scp <host>:<path>`.
    #[test]
    fn a_media_path_outside_the_remote_media_dir_is_refused_before_any_scp() {
        for evil in [
            json!("/etc/passwd"),
            json!("/remote/bridge/../../etc/shadow"),
            json!("/remote/bridge/state.json"),
            json!("/remote/bridgemedia/x.jpg"),
            json!(""),
            Value::Null,
        ] {
            let media = json!({ "path": evil, "kind": "image", "mime": "image/jpeg", "name": "x" });
            let h = harness(
                FakeRemote::claiming(&job(json!({ "media": media }))),
                FakeRunner::with(vec![]),
            );
            h.worker.poll_once();

            assert!(
                h.remote.scps.lock().unwrap().is_empty(),
                "scp was attempted for {evil:?}"
            );
            assert!(
                h.runner.attempts().is_empty(),
                "the engine ran on a missing file for {evil:?}"
            );
            assert_eq!(
                h.remote.returned_payload()["error"],
                "Could not download Telegram attachment: Invalid remote media path."
            );
        }
    }

    /// A download failure returns an error result INSTEAD of running the
    /// engine — otherwise the prompt points at a file that is not there.
    #[test]
    fn a_failed_download_short_circuits_the_engine() {
        let media = json!({
            "path": "/remote/bridge/media/gone.jpg",
            "kind": "video", "mime": "video/mp4", "name": "clip.mp4",
        });
        let mut remote = FakeRemote::claiming(&job(json!({ "media": media })));
        remote.scp_err = Mutex::new(Some("scp exited 1: No such file".into()));
        let h = harness(remote, FakeRunner::with(vec![]));
        h.worker.poll_once();

        assert!(h.runner.attempts().is_empty());
        assert_eq!(
            h.remote.returned_payload()["error"],
            "Could not download Telegram attachment: scp exited 1: No such file"
        );
    }

    // ---- log lines ----

    #[test]
    fn the_log_lines_match_the_reference_wording() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("worker.log");
        let remote = Arc::new(FakeRemote::claiming(&job(json!({ "sessionId": "stale" }))));
        let runner = FakeRunner::with(vec![RunResult {
            code: Some(7),
            ..RunResult::default()
        }]);
        let worker = Worker::new(
            Arc::clone(&remote) as Arc<dyn Remote>,
            Arc::clone(&runner) as Arc<dyn EngineRunner>,
            Arc::new(stackhour_core::registry::load(Path::new("/nonexistent-cfg"))),
            dir.path().join("media"),
            "/remote/bridge".into(),
            None,
            log.clone(),
        );
        worker.poll_once();

        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            body.contains("claimed 3f2504e0-4f89-41d3-9a0c-0305e82c3301: do the thing"),
            "{body}"
        );
        assert!(body.contains("claude resume failed (7); retry fresh"), "{body}");
        assert!(
            body.contains("returned 3f2504e0-4f89-41d3-9a0c-0305e82c3301"),
            "{body}"
        );
    }

    #[test]
    fn a_long_prompt_and_a_long_stderr_are_truncated_like_the_js() {
        assert_eq!(truncate_utf16(&"x".repeat(200), 60).len(), 60);
        assert_eq!(truncate_utf16("short", 60), "short");
        // slice(-n) keeps the TAIL.
        assert_eq!(truncate_tail_utf16("abcdef", 3), "def");
        assert_eq!(truncate_tail_utf16("ab", 3), "ab");
    }

    /// The SSH wire contract with the leader. This used to be
    /// `node <remoteDir>/claim.mjs`; Node is retired, so the worker execs the
    /// leader's own binary and names the runtime dir explicitly, because an
    /// SSH command inherits no environment.
    #[test]
    fn the_claim_and_return_commands_are_built_from_the_remote_config() {
        let h = harness(FakeRemote::claiming(&job(json!({}))), FakeRunner::with(vec![]));
        h.worker.poll_once();
        let calls = h.remote.calls.lock().unwrap().clone();
        assert_eq!(
            calls[0].0,
            "'/remote/bridge/stackhour' bridge claim --runtime-dir '/remote/bridge'"
        );
        assert_eq!(calls[0].1, None, "claim takes no stdin");
        assert_eq!(
            calls[1].0,
            "'/remote/bridge/stackhour' bridge return '3f2504e0-4f89-41d3-9a0c-0305e82c3301' \
             --runtime-dir '/remote/bridge'"
        );
        assert!(calls[1].1.is_some(), "return takes the payload on stdin");
    }

    /// Every interpolated component is shell-quoted: `remoteDir` and `target`
    /// come from a config file and the job id from a coordinator payload, so
    /// none of them may reach the remote shell as bare words.
    #[test]
    fn the_remote_command_shell_quotes_every_component() {
        let dir = tempfile::tempdir().unwrap();
        let worker = Worker::new(
            Arc::new(FakeRemote::default()) as Arc<dyn Remote>,
            FakeRunner::with(vec![]) as Arc<dyn EngineRunner>,
            Arc::new(stackhour_core::registry::load(Path::new("/nonexistent-cfg"))),
            dir.path().join("media"),
            "/srv/it's here/".into(),
            Some("; rm -rf /".into()),
            dir.path().join("worker.log"),
        );
        assert_eq!(
            worker.remote_cmd("claim", worker.target.as_deref()),
            "'/srv/it'\"'\"'s here/stackhour' bridge claim '; rm -rf /' \
             --runtime-dir '/srv/it'\"'\"'s here'",
            "a trailing slash is trimmed and the quote is closed/escaped/reopened"
        );
    }

    /// A worker with a configured target passes it as `bridge claim`'s
    /// argument; the return path is unchanged.
    #[test]
    fn a_configured_target_rides_the_remote_claim_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let remote = Arc::new(FakeRemote::claiming(&job(json!({ "target": "attic" }))));
        let worker = Worker::new(
            Arc::clone(&remote) as Arc<dyn Remote>,
            FakeRunner::with(vec![]) as Arc<dyn EngineRunner>,
            Arc::new(stackhour_core::registry::load(Path::new("/nonexistent-cfg"))),
            dir.path().join("media"),
            "/remote/bridge".into(),
            Some("attic".into()),
            dir.path().join("worker.log"),
        );
        assert_eq!(worker.poll_once(), Poll::Handled);
        let calls = remote.calls.lock().unwrap().clone();
        assert_eq!(
            calls[0].0,
            "'/remote/bridge/stackhour' bridge claim 'attic' --runtime-dir '/remote/bridge'"
        );
        assert_eq!(
            calls[1].0,
            "'/remote/bridge/stackhour' bridge return '3f2504e0-4f89-41d3-9a0c-0305e82c3301' \
             --runtime-dir '/remote/bridge'",
            "the return path carries no target"
        );
    }

    /// One failed return permanently loses the result. Reference behaviour;
    /// the test exists so nobody "fixes" it without noticing.
    #[test]
    fn a_failed_return_is_logged_and_never_retried() {
        let mut remote = FakeRemote::claiming(&job(json!({})));
        remote.ret = Mutex::new(RemoteOut {
            code: 255,
            stdout: String::new(),
            stderr: "  Connection reset by peer  ".into(),
        });
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("worker.log");
        let remote = Arc::new(remote);
        let worker = Worker::new(
            Arc::clone(&remote) as Arc<dyn Remote>,
            FakeRunner::with(vec![]) as Arc<dyn EngineRunner>,
            Arc::new(stackhour_core::registry::load(Path::new("/nonexistent-cfg"))),
            dir.path().join("media"),
            "/remote/bridge".into(),
            None,
            log.clone(),
        );
        assert_eq!(worker.poll_once(), Poll::Handled);

        let returns = remote
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, _)| c.contains("bridge return"))
            .count();
        assert_eq!(returns, 1, "no retry — the result is simply lost");
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            body.contains("return failed for 3f2504e0-4f89-41d3-9a0c-0305e82c3301: Connection reset by peer"),
            "{body}"
        );
    }

    #[test]
    fn the_ssh_argv_carries_the_reference_options_verbatim() {
        let with_key = SshRemote {
            key: Some("/home/n/.ssh/gcp".into()),
            host: "n@gcp".into(),
        };
        assert_eq!(
            with_key.base_args(),
            vec![
                "-i",
                "/home/n/.ssh/gcp",
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ConnectTimeout=15"
            ]
        );
        // No key configured: fall back to agent auth rather than passing an
        // empty `-i`.
        let no_key = SshRemote {
            key: None,
            host: "n@gcp".into(),
        };
        assert_eq!(no_key.base_args()[0], "-o");
    }
}
