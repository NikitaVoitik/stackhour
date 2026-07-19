//! Blocking HTTP posts to the server.
//!
//! /api/ingest: 10s timeout, Bearer header only when the token is truthy,
//! non-2xx -> error `/api/ingest failed: HTTP N`, returns the inserted
//! count. /api/agent-status: 3s timeout, failures logged only, never fatal.
//! The health report retains the JSON key `nodeVersion` =
//! `stackhour_core::build_info()` (flagged parity decision; the dashboard
//! only displays it).

use serde_json::Value;
use stackhour_core::{Error, Result};
use std::time::Duration;

const INGEST_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_TIMEOUT: Duration = Duration::from_secs(3);

/// One POST with the shared header shape. Returns the parsed JSON body.
fn post(
    server_url: &str,
    token: &str,
    endpoint: &str,
    body: &Value,
    timeout: Duration,
) -> Result<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| Error::msg(e.to_string()))?;
    let mut req = client
        .post(format!("{server_url}{endpoint}"))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(body)?);
    // The Bearer header is sent ONLY for a non-empty token; an empty one must
    // not become `Bearer `, which the server would reject as a bad token
    // rather than treat as anonymous.
    if !token.is_empty() {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let res = req.send().map_err(|e| Error::msg(e.to_string()))?;
    let status = res.status();
    if !status.is_success() {
        return Err(Error::msg(format!(
            "{endpoint} failed: HTTP {}",
            status.as_u16()
        )));
    }
    res.json().map_err(|e| Error::msg(e.to_string()))
}

/// POST rows to /api/ingest; returns the server's inserted count.
pub fn post_ingest(server_url: &str, token: &str, rows: &[Value]) -> Result<i64> {
    let body = Value::Array(rows.to_vec());
    let res = post(server_url, token, "/api/ingest", &body, INGEST_TIMEOUT)?;
    Ok(res.get("inserted").and_then(Value::as_i64).unwrap_or(0))
}

/// POST the health report to /api/agent-status (best-effort; logs failures).
///
/// Never returns an error: a lost health report is cosmetic, and letting it
/// fail a tick would drop real heartbeats on the floor.
pub fn post_status(server_url: &str, token: &str, report: &Value) {
    if let Err(e) = post(
        server_url,
        token,
        "/api/agent-status",
        report,
        STATUS_TIMEOUT,
    ) {
        eprintln!("[stackhour] health report failed: {}", e.message());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// A one-shot HTTP server that captures the request and replies with a
    /// canned response. Returns (base_url, receiver of the raw request).
    fn one_shot(response: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut raw = String::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                let done = line == "\r\n";
                raw.push_str(&line);
                if done {
                    break;
                }
            }
            let mut body = vec![0u8; len];
            use std::io::Read;
            let _ = reader.read_exact(&mut body);
            raw.push_str(&String::from_utf8_lossy(&body));
            let _ = tx.send(raw);
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    fn ok_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn ingest_posts_the_rows_and_returns_the_inserted_count() {
        let (url, rx) = one_shot(Box::leak(ok_response(r#"{"inserted":2}"#).into_boxed_str()));
        let rows = vec![json!({ "time": 1 }), json!({ "time": 2 })];
        assert_eq!(post_ingest(&url, "sekrit", &rows).unwrap(), 2);

        let raw = rx.recv().unwrap();
        assert!(raw.starts_with("POST /api/ingest "), "got: {raw}");
        assert!(raw.to_lowercase().contains("content-type: application/json"));
        assert!(raw.contains("authorization: Bearer sekrit"));
        // The body is a JSON ARRAY of the rows, in order.
        let body = raw.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!([{ "time": 1 }, { "time": 2 }])
        );
    }

    /// An empty token means "anonymous"; sending `Bearer ` would be read as a
    /// wrong token and rejected.
    #[test]
    fn an_empty_token_sends_no_authorization_header() {
        let (url, rx) = one_shot(Box::leak(ok_response(r#"{"inserted":0}"#).into_boxed_str()));
        post_ingest(&url, "", &[json!({ "time": 1 })]).unwrap();
        let raw = rx.recv().unwrap();
        assert!(
            !raw.to_lowercase().contains("authorization"),
            "got: {raw}"
        );
    }

    /// A rejected batch must surface as an error so the caller KEEPS the
    /// queue rather than dropping it.
    #[test]
    fn a_non_2xx_response_is_an_error_naming_the_status() {
        let (url, _rx) = one_shot(
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
        let err = post_ingest(&url, "bad", &[json!({ "time": 1 })]).unwrap_err();
        assert_eq!(err.message(), "/api/ingest failed: HTTP 401");
    }

    #[test]
    fn an_unreachable_server_is_an_error_not_a_panic() {
        // Port 1 on loopback refuses connections.
        let err = post_ingest("http://127.0.0.1:1", "t", &[json!({})]).unwrap_err();
        assert!(!err.message().is_empty());
    }

    /// A missing `inserted` field degrades to 0 rather than failing the tick.
    #[test]
    fn a_response_without_inserted_counts_as_zero() {
        let (url, _rx) = one_shot(Box::leak(ok_response("{}").into_boxed_str()));
        assert_eq!(post_ingest(&url, "t", &[json!({})]).unwrap(), 0);
    }

    /// post_status must swallow every failure — a health report is cosmetic.
    #[test]
    fn status_failures_are_swallowed() {
        post_status("http://127.0.0.1:1", "t", &json!({ "machine": "box" }));
        let (url, _rx) = one_shot(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
        post_status(&url, "t", &json!({ "machine": "box" }));
    }

    #[test]
    fn status_posts_the_report_to_the_agent_status_endpoint() {
        let (url, rx) = one_shot(Box::leak(ok_response("{}").into_boxed_str()));
        post_status(&url, "t", &json!({ "machine": "box", "queueDepth": 3 }));
        let raw = rx.recv().unwrap();
        assert!(raw.starts_with("POST /api/agent-status "), "got: {raw}");
        let body = raw.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap()["queueDepth"],
            3
        );
    }
}
