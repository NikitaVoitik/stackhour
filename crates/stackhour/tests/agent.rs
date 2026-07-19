//! End-to-end agent tests against a real `stackhour serve`.
//!
//! This is the scenario the agent exists for and the one that was previously
//! unrunnable: touch a file, confirm the heartbeat reaches the server, take
//! the server away, confirm heartbeats queue on disk, bring it back, confirm
//! the queue drains. Unit tests cannot cover it — it needs two real
//! processes and a real socket.

use serde_json::Value;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn bin() -> PathBuf {
    let mut path = std::env::current_exe().expect("test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("stackhour")
}

/// A port nothing is listening on right now. Bind-and-drop leaves a small
/// race, but each test uses its own port so a collision is improbable.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Env {
    home: TempDir,
    port: u16,
}

impl Env {
    /// A sandbox HOME with a server config, a project root, and no server
    /// running yet.
    fn new() -> Self {
        let env = Env {
            home: TempDir::new().unwrap(),
            port: free_port(),
        };
        std::fs::create_dir_all(env.project_root()).unwrap();
        let out = env.run(&[
            "init",
            "server",
            &format!("--port={}", env.port),
            "--machine=box",
            &format!("--project-root={}", env.project_root().display()),
        ]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        env
    }

    fn project_root(&self) -> PathBuf {
        self.home.path().join("proj")
    }

    fn data_dir(&self) -> PathBuf {
        self.home.path().join(".local").join("share").join("stackhour")
    }

    fn queue_path(&self) -> PathBuf {
        self.data_dir().join("queue.jsonl")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", std::env::var("PATH").unwrap_or_default());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().expect("the binary must be executable")
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Start `stackhour serve` and wait until it answers.
    fn serve(&self) -> Server {
        let child = self
            .cmd(&["serve"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("serve must start");
        let server = Server(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if get(&self.url("/api/health")).is_some() {
                return server;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("server never became reachable on port {}", self.port);
    }

    fn queued_rows(&self) -> Vec<Value> {
        let Ok(text) = std::fs::read_to_string(self.queue_path()) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

/// A server child process, killed on drop so a failing assert cannot leak it.
struct Server(Child);

impl Server {
    fn stop(mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Minimal blocking GET returning the parsed JSON body, or None if the
/// connection failed.
fn get(url: &str) -> Option<Value> {
    let out = Command::new("curl")
        .args(["-s", "--max-time", "5", url])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn touch(path: &Path) {
    std::fs::write(path, format!("// {:?}\n", Instant::now())).unwrap();
}

/// The full offline/online cycle in one test, because the states are
/// sequential: each step's precondition is the previous step's outcome.
#[test]
fn heartbeats_queue_while_the_server_is_down_and_drain_when_it_returns() {
    let env = Env::new();

    // --- 1. Server DOWN: a touched file must land on disk, not vanish. ---
    touch(&env.project_root().join("a.rs"));
    let out = env.run(&["agent", "--once"]);
    assert!(
        out.status.success(),
        "agent --once failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("server unreachable"),
        "expected an unreachable-server notice"
    );
    let queued = env.queued_rows();
    assert_eq!(queued.len(), 1, "the heartbeat was not queued: {queued:?}");
    assert_eq!(queued[0]["machine"], "box");
    assert_eq!(queued[0]["source"], "editor-files");
    assert_eq!(queued[0]["project"], "proj");
    assert!(queued[0]["entity"].as_str().unwrap().ends_with("/a.rs"));

    // --- 2. Server UP: the queue drains and the file is deleted. ---
    let server = env.serve();
    let out = env.run(&["agent", "--once"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("sent 1 heartbeats"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !env.queue_path().exists(),
        "a fully drained queue must be removed, not left empty"
    );

    // --- 3. The server actually recorded it. ---
    let summary = get(&env.url("/api/summary?days=1")).expect("summary");
    assert!(
        summary["total"].as_f64().unwrap() > 0.0,
        "server recorded no time: {summary}"
    );
    assert_eq!(summary["totals"][0]["project"], "proj");

    // --- 4. And the health report arrived. ---
    let statuses = get(&env.url("/api/agent-status")).expect("agent-status");
    let local = statuses
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["machine"] == "box")
        .expect("a status row for this machine");
    assert_eq!(local["queueDepth"], 0);
    assert_eq!(local["version"], stackhour_core_version());
    assert_eq!(local["watchers"]["files"]["enabled"], true);
    assert_eq!(local["watchers"]["files"]["available"], true);
    assert!(local["watchers"]["files"]["lastOk"].as_f64().unwrap() > 0.0);

    // --- 5. A new file with the server up goes straight through. ---
    touch(&env.project_root().join("b.rs"));
    let out = env.run(&["agent", "--once"]);
    assert!(out.status.success());
    assert!(!env.queue_path().exists());
    let summary = get(&env.url("/api/summary?days=1")).expect("summary");
    assert!(summary["total"].as_f64().unwrap() > 0.0);

    server.stop();
}

/// Queued heartbeats accumulate across ticks rather than overwriting each
/// other — the classic append-vs-rewrite bug.
#[test]
fn repeated_offline_ticks_accumulate_the_queue() {
    let env = Env::new();
    for name in ["a.rs", "b.rs", "c.rs"] {
        touch(&env.project_root().join(name));
        // Each tick must see the file as NEW, so space them past the mtime
        // granularity of the previous scan.
        std::thread::sleep(Duration::from_millis(1100));
        assert!(env.run(&["agent", "--once"]).status.success());
    }
    let queued = env.queued_rows();
    assert_eq!(queued.len(), 3, "queued rows: {queued:?}");
    let mut entities: Vec<&str> = queued
        .iter()
        .map(|r| r["entity"].as_str().unwrap())
        .collect();
    entities.sort_unstable();
    assert!(entities[0].ends_with("/a.rs"));
    assert!(entities[1].ends_with("/b.rs"));
    assert!(entities[2].ends_with("/c.rs"));
}

/// The lock is what stops two agents double-counting every heartbeat.
#[test]
fn a_second_agent_refuses_to_start_while_one_holds_the_lock() {
    let env = Env::new();
    std::fs::create_dir_all(env.data_dir()).unwrap();
    // pid 1 always exists and is not us, so the lock reads as live.
    std::fs::write(env.data_dir().join("agent.lock"), "1").unwrap();

    let out = env.run(&["agent", "--once"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "stackhour agent: stackhour agent already running (pid 1)\n"
    );
}

/// A crashed agent's stale lock must not wedge every future run.
#[test]
fn a_stale_lock_does_not_block_the_agent() {
    let env = Env::new();
    std::fs::create_dir_all(env.data_dir()).unwrap();
    std::fs::write(env.data_dir().join("agent.lock"), "4194303").unwrap();
    assert!(env.run(&["agent", "--once"]).status.success());
    assert!(
        !env.data_dir().join("agent.lock").exists(),
        "the lock must be released when the agent exits"
    );
}

/// State persists watcher offsets, so an unchanged tree produces nothing on
/// the second pass.
#[test]
fn an_unchanged_tree_produces_no_heartbeats_on_the_next_tick() {
    let env = Env::new();
    touch(&env.project_root().join("a.rs"));
    assert!(env.run(&["agent", "--once"]).status.success());
    assert_eq!(env.queued_rows().len(), 1);

    std::thread::sleep(Duration::from_millis(1100));
    assert!(env.run(&["agent", "--once"]).status.success());
    assert_eq!(
        env.queued_rows().len(),
        1,
        "an unchanged tree must not re-report"
    );

    // The state file records where the scan got to.
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(env.data_dir().join("agent-state.json")).unwrap(),
    )
    .unwrap();
    assert!(state["filesLastScan"].as_f64().unwrap() > 0.0);
    assert_eq!(state["watcherHealth"]["files"]["consecutiveErrors"], 0);
}

fn stackhour_core_version() -> &'static str {
    // Kept in one place so a version bump does not silently skip the check.
    env!("CARGO_PKG_VERSION")
}
