//! A local mock of the Telegram Bot API, used by every transport test.
//!
//! SAFETY: the owner's real coordinator is long-polling `getUpdates` against
//! the live bot token right now. A second poller on that token would steal his
//! messages and silently break the bridge, so no test in this crate is ever
//! allowed to touch the real API. Everything runs against this in-process
//! HTTP server instead: it speaks the Bot API request/response SHAPE
//! (`POST /bot<token>/<method>` with a JSON body, `{ok, result}` or
//! `{ok:false, description, parameters}` back) and nothing else.
//!
//! Responses are scripted: push the exact `(status, body)` sequence a test
//! needs, and every request is recorded so the test can assert on the method
//! name and the request body the transport actually produced.

#![allow(dead_code)] // each test binary uses a different subset

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// One recorded inbound request.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// The Bot API method name, e.g. `sendMessage`.
    pub method: String,
    /// The full request path, for the file-download endpoint.
    pub path: String,
    pub body: Value,
}

/// One scripted response.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    /// `{ "ok": true, "result": <result> }` with HTTP 200.
    pub fn ok(result: Value) -> Reply {
        Reply {
            status: 200,
            body: json!({ "ok": true, "result": result }).to_string(),
        }
    }

    /// A Bot API error envelope.
    pub fn err(status: u16, description: &str) -> Reply {
        Reply {
            status,
            body: json!({ "ok": false, "error_code": status, "description": description })
                .to_string(),
        }
    }

    /// A 429 carrying `parameters.retry_after`.
    pub fn rate_limited(retry_after: u64) -> Reply {
        Reply {
            status: 429,
            body: json!({
                "ok": false,
                "error_code": 429,
                "description": "Too Many Requests: retry later",
                "parameters": { "retry_after": retry_after },
            })
            .to_string(),
        }
    }

    /// A body that is not JSON at all, to exercise the parse-failure path.
    pub fn garbage(status: u16) -> Reply {
        Reply {
            status,
            body: "<html>gateway</html>".to_string(),
        }
    }

    /// Raw bytes with a `content-type` of octet-stream, for file downloads.
    pub fn raw(status: u16, body: &str) -> Reply {
        Reply {
            status,
            body: body.to_string(),
        }
    }
}

struct Shared {
    script: Mutex<VecDeque<Reply>>,
    default: Mutex<Reply>,
    seen: Mutex<Vec<Recorded>>,
}

/// A running mock server. Dropping it leaves the thread parked on `accept`,
/// which is fine for the lifetime of a test binary.
pub struct MockApi {
    pub base: String,
    shared: Arc<Shared>,
}

impl MockApi {
    /// Bind to an ephemeral loopback port and start serving.
    pub fn start() -> MockApi {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let shared = Arc::new(Shared {
            script: Mutex::new(VecDeque::new()),
            default: Mutex::new(Reply::ok(json!({ "message_id": 1 }))),
            seen: Mutex::new(Vec::new()),
        });
        let worker = Arc::clone(&shared);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let shared = Arc::clone(&worker);
                std::thread::spawn(move || {
                    let _ = handle(stream, &shared);
                });
            }
        });
        MockApi {
            base: format!("http://127.0.0.1:{}", addr.port()),
            shared,
        }
    }

    /// Queue one scripted response. Consumed in order; once the queue is
    /// empty the default reply is used.
    pub fn push(&self, reply: Reply) -> &MockApi {
        self.shared.script.lock().unwrap().push_back(reply);
        self
    }

    /// Queue the same reply `n` times.
    pub fn push_n(&self, n: usize, reply: Reply) -> &MockApi {
        for _ in 0..n {
            self.push(reply.clone());
        }
        self
    }

    /// Replace the reply used once the script runs out.
    pub fn set_default(&self, reply: Reply) {
        *self.shared.default.lock().unwrap() = reply;
    }

    /// Every request the server has seen, in order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.shared.seen.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.shared.seen.lock().unwrap().len()
    }

    /// The method names seen so far.
    pub fn methods(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.method).collect()
    }

    /// The nth recorded request, panicking with a useful message if absent.
    pub fn nth(&self, i: usize) -> Recorded {
        self.requests()
            .get(i)
            .cloned()
            .unwrap_or_else(|| panic!("no request #{i}; saw {:?}", self.methods()))
    }

    pub fn last(&self) -> Recorded {
        let all = self.requests();
        all.last().cloned().expect("at least one request")
    }

    pub fn clear(&self) {
        self.shared.seen.lock().unwrap().clear();
        self.shared.script.lock().unwrap().clear();
    }
}

fn handle(stream: TcpStream, shared: &Shared) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    let mut raw = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut raw)?;
    }
    let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);

    // `/bot<token>/<method>` -> `<method>`; `/file/bot<token>/<file_path>`
    // keeps the whole path.
    let method = path.rsplit('/').next().unwrap_or_default().to_string();
    shared.seen.lock().unwrap().push(Recorded {
        method,
        path: path.clone(),
        body,
    });

    let reply = shared
        .script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| shared.default.lock().unwrap().clone());

    let mut out = stream;
    write!(
        out,
        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.status,
        reply.body.len()
    )?;
    out.write_all(reply.body.as_bytes())?;
    out.flush()
}
