//! Session-state parity with coordinator.mjs, proven by driving the REAL
//! release/debug binary as a daemon.
//!
//! SAFETY: this test never touches api.telegram.org. It starts a local HTTP
//! server that speaks the Bot API request/response SHAPE and points the
//! coordinator at it with the `apiRoot` config key, using an obviously-fake
//! token. The owner's Node coordinator holds the only legitimate long poll on
//! the real token; a second poller would silently steal his messages.
//!
//! What is pinned here, against the Node's own `loadState` / `sessionKey` /
//! `getSession` / `setSession` (coordinator.mjs L41-60) and its reply strings:
//!
//! 1. Sessions are keyed `<target>:<engine>` — switching either dimension
//!    addresses a different slot.
//! 2. Resume-vs-fresh: the switch reply reads the session AFTER the switch,
//!    so it says `(resuming session)` iff the destination key already holds a
//!    session id.
//! 3. `/new` clears ONLY the current key, writing a JSON `null` (not deleting
//!    it), and leaves every other key untouched.
//! 4. All of it survives a restart, and the state FILE is byte-identical to
//!    what the Node writes for the same sequence (2-space pretty JSON, insertion
//!    key order, no trailing newline).
//!
//! DIVERGENCE, deliberate (see `divergence_new_survives_restart_unlike_the_node`):
//! the Node's legacy-key migration is `if (s.sessions[t] && !s.sessions[t+':claude'])`.
//! A session cleared by `/new` is `null`, which is falsy, so on the NEXT restart
//! the Node re-migrates the bare legacy key over it and RESURRECTS the session
//! the user just cleared. The Rust keys off presence, not truthiness, so `/new`
//! sticks. This is reachable on the owner's live state.json today (its bare
//! `"mac"` key holds a string).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CHAT: i64 = 4242;
/// Obviously fake. Never a real token, in any file, ever.
const FAKE_TOKEN: &str = "111111:FAKE-TOKEN-FOR-LOCAL-MOCK-ONLY";

/// The exact seed the Node harness was run against.
const SEED_STATE: &str = r#"{
  "offset": 0,
  "active": "gcp",
  "sessions": {
    "gcp": null,
    "mac": "LEGACY-MAC",
    "gcp:claude": "SEED-GCP-CLAUDE",
    "gcp:codex": "SEED-GCP-CODEX"
  },
  "engine": "claude"
}"#;

// ---------------------------------------------------------------- mock API

/// A local stand-in for the Bot API: hands out a fixed update list to
/// `getUpdates` (honouring `offset`, so a confirmed update is not redelivered)
/// and records every other call.
struct MockApi {
    base: String,
    sent: Arc<Mutex<Vec<(String, Value)>>>,
}

impl MockApi {
    fn start(updates: Vec<Value>) -> MockApi {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let sent: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let log = sent.clone();
        std::thread::spawn(move || {
            let mut next_id = 100i64;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                serve_one(stream, &updates, &log, &mut next_id);
            }
        });
        MockApi { base, sent }
    }

    /// The texts of every `sendMessage`, in order.
    fn texts(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "sendMessage")
            .map(|(_, b)| b["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

fn serve_one(
    mut stream: TcpStream,
    updates: &[Value],
    log: &Arc<Mutex<Vec<(String, Value)>>>,
    next_id: &mut i64,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut raw = vec![0u8; length];
    let _ = reader.read_exact(&mut raw);
    let body: Value = serde_json::from_slice(&raw).unwrap_or_else(|_| json!({}));
    let method = path.rsplit('/').next().unwrap_or("").to_string();

    let result = if method == "getUpdates" {
        let offset = body["offset"].as_i64().unwrap_or(0);
        let pending: Vec<Value> = updates
            .iter()
            .filter(|u| u["update_id"].as_i64().unwrap_or(0) >= offset)
            .cloned()
            .collect();
        if pending.is_empty() {
            // Long-poll: idle rather than spinning the daemon's loop hot.
            std::thread::sleep(Duration::from_millis(200));
        }
        json!(pending)
    } else {
        log.lock().unwrap().push((method, body.clone()));
        *next_id += 1;
        json!({ "message_id": *next_id, "date": 0, "chat": { "id": CHAT } })
    };

    let payload = json!({ "ok": true, "result": result }).to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        payload.len(),
        payload
    );
    let _ = stream.flush();
}

// ------------------------------------------------------------------ driver

fn text_update(id: i64, text: &str) -> Value {
    json!({
        "update_id": id,
        "message": {
            "message_id": id, "date": 0,
            "chat": { "id": CHAT }, "from": { "id": CHAT },
            "text": text,
        }
    })
}

fn write_config(dir: &Path, api_root: &str) {
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_string_pretty(&json!({
            "token": FAKE_TOKEN,
            "chatId": CHAT,
            "defaultTarget": "gcp",
            "apiRoot": api_root,
            "targets": {
                // `claudeBin` points at /bin/false: nothing in this test sends a
                // prompt, and if a regression ever made it, the child dies
                // instantly instead of running a real agent.
                "gcp": { "label": "☁️ GCP", "type": "local", "cwd": "/tmp", "claudeBin": "/bin/false" },
                "mac": { "label": "🖥️ Mac", "type": "worker" },
            },
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Run the coordinator against a mock serving `commands`, until it has replied
/// to all of them (or the deadline). Returns the `sendMessage` texts.
///
/// The daemon never returns on its own — it is a `-> !` poll loop — so it is
/// killed once the expected number of replies has landed.
fn drive(dir: &Path, first_id: i64, commands: &[&str]) -> Vec<String> {
    let updates: Vec<Value> = commands
        .iter()
        .enumerate()
        .map(|(i, t)| text_update(first_id + i as i64, t))
        .collect();
    let api = MockApi::start(updates);
    write_config(dir, &api.base);

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_stackhour"))
        .args(["bridge", "coordinator", "--runtime-dir"])
        .arg(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the coordinator binary");

    // 1 online banner + one reply per command.
    let want = commands.len() + 1;
    let deadline = Instant::now() + Duration::from_secs(30);
    while api.texts().len() < want && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    // The reply is sent AFTER saveState, but give the last write a moment to
    // land before the kill so the assertions read a settled file.
    std::thread::sleep(Duration::from_millis(300));
    let _ = child.kill();
    let _ = child.wait();

    let texts = api.texts();
    assert!(
        texts.len() >= want,
        "coordinator produced {} of {want} expected messages: {texts:#?}",
        texts.len()
    );
    texts
}

fn state(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("state.json")).unwrap()).unwrap()
}

// ------------------------------------------------------------------- tests

/// The whole slice in one drive: keying, resume-vs-fresh on both dimensions,
/// and `/new`.
#[test]
fn sessions_are_keyed_by_target_and_engine_with_the_nodes_replies() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("state.json"), SEED_STATE).unwrap();

    let texts = drive(dir, 1, &["/where", "/codex", "/mac", "/claude", "/new", "/where"]);

    assert_eq!(
        &texts[1..],
        &[
            // `getSession()` reads gcp:claude — the seeded id, first 8 chars + ellipsis.
            "Engine: Claude\nTarget: ☁️ GCP\nSession: SEED-GCP…\nMac worker: offline\nGCP busy: no",
            // gcp:codex IS seeded, so switching engine resumes...
            "Switched to Codex on ☁️ GCP. (resuming session)",
            // ...but mac:codex is not, so switching target is fresh. Same engine,
            // different target => a different slot. That is the key.
            "Switched to 🖥️ Mac with Codex. (new session)",
            // mac:claude exists only because the bare legacy "mac" key migrated
            // onto it at load.
            "Switched to Claude on 🖥️ Mac. (resuming session)",
            "🆕 Fresh Claude session on 🖥️ Mac.",
            "Engine: Claude\nTarget: 🖥️ Mac\nSession: none (fresh)\nMac worker: offline\nGCP busy: no",
        ],
    );

    let s = state(dir);
    assert_eq!(s["active"], "mac");
    assert_eq!(s["engine"], "claude");
    // /new cleared ONLY the active key, and cleared it to null rather than
    // removing it.
    assert_eq!(s["sessions"]["mac:claude"], Value::Null);
    assert!(
        s["sessions"].get("mac:claude").is_some(),
        "the key was deleted, not nulled"
    );
    // Every other slot is untouched.
    assert_eq!(s["sessions"]["gcp:claude"], "SEED-GCP-CLAUDE");
    assert_eq!(s["sessions"]["gcp:codex"], "SEED-GCP-CODEX");
    assert_eq!(s["sessions"]["mac"], "LEGACY-MAC");
    assert_eq!(s["sessions"]["gcp"], Value::Null);
}

/// The file the Node would have written for that same sequence, byte for byte:
/// 2-space pretty JSON, `offset`/`active`/`sessions`/`engine` in insertion
/// order, no trailing newline. Produced by replaying coordinator.mjs L42-60
/// verbatim under node; pinned here as a literal so the test needs no node.
#[test]
fn the_state_file_is_byte_identical_to_the_nodes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("state.json"), SEED_STATE).unwrap();
    drive(dir, 1, &["/where", "/codex", "/mac", "/claude", "/new", "/where"]);

    const NODE_WROTE: &str = r#"{
  "offset": 7,
  "active": "mac",
  "sessions": {
    "gcp": null,
    "mac": "LEGACY-MAC",
    "gcp:claude": "SEED-GCP-CLAUDE",
    "gcp:codex": "SEED-GCP-CODEX",
    "mac:claude": null
  },
  "engine": "claude"
}"#;
    assert_eq!(
        std::fs::read_to_string(dir.join("state.json")).unwrap(),
        NODE_WROTE
    );
}

/// Kill the daemon, start it again on the same runtime dir: the active
/// target/engine, the poll offset and every session id come back.
#[test]
fn state_survives_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("state.json"), SEED_STATE).unwrap();

    drive(dir, 1, &["/codex", "/mac", "/claude", "/new"]);
    let before = state(dir);

    // Second process, fresh mock, update ids past the persisted offset.
    let texts = drive(dir, 10, &["/where", "/gcp", "/codex", "/where"]);

    // The online banner proves active+engine were reloaded, not re-defaulted
    // to the config's gcp/claude.
    assert_eq!(
        texts[0],
        "🤖 Claude + Codex bridge online. Active: Claude on 🖥️ Mac. Mac worker: offline."
    );
    assert_eq!(
        &texts[1..],
        &[
            // The /new from the PREVIOUS process still holds.
            "Engine: Claude\nTarget: 🖥️ Mac\nSession: none (fresh)\nMac worker: offline\nGCP busy: no",
            "Switched to ☁️ GCP with Claude. (resuming session)",
            "Switched to Codex on ☁️ GCP. (resuming session)",
            // ...and the gcp slots survived the restart intact.
            "Engine: Codex\nTarget: ☁️ GCP\nSession: SEED-GCP…\nMac worker: offline\nGCP busy: no",
        ],
    );

    let after = state(dir);
    assert_eq!(after["sessions"], before["sessions"]);
    assert!(
        after["offset"].as_i64().unwrap() > before["offset"].as_i64().unwrap(),
        "the poll offset did not advance across the restart"
    );
}

/// The one place the Rust deliberately disagrees with the Node.
///
/// Node: `if (s.sessions[t] && !s.sessions[`${t}:claude`])` — `null` is falsy,
/// so a slot cleared by `/new` is re-migrated from the bare legacy key on the
/// next load and the cleared session comes BACK. Verified by replaying those
/// lines under node against this exact seed: `getSession('mac','claude')`
/// returns `LEGACY-MAC` again after the restart.
///
/// Rust keys the migration off PRESENCE, so `/new` sticks. Called out rather
/// than silently fixed.
#[test]
fn divergence_new_survives_restart_unlike_the_node() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("state.json"),
        r#"{"offset":0,"active":"mac","engine":"claude",
            "sessions":{"mac":"LEGACY-MAC","mac:claude":null}}"#,
    )
    .unwrap();

    let texts = drive(dir, 1, &["/where"]);
    assert!(
        texts[1].contains("Session: none (fresh)"),
        "a null session was resurrected from the legacy key: {}",
        texts[1]
    );
    assert_eq!(state(dir)["sessions"]["mac:claude"], Value::Null);
}
