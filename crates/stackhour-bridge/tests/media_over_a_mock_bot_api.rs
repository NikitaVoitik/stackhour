//! Media download driven through the REAL [`Tg`] client against a local mock
//! that speaks the Bot API shape.
//!
//! This exists because `media::download_media`'s unit tests stub the
//! transport, so nothing else proves the two URL shapes are right:
//!
//! * `POST /bot<token>/getFile` with a JSON `{"file_id":...}` body, and the
//!   `result` — not the envelope — is what reaches the media code.
//! * `GET /file/bot<token>/<file_path>` — the SECOND derived base, which is
//!   easy to forget and impossible to notice without an end-to-end check.
//!
//! The token here is obviously fake and the server is a throwaway listener on
//! 127.0.0.1. Nothing in this file may ever point at api.telegram.org: the
//! owner's coordinator is long-polling the real bot right now, and a second
//! caller on that token would steal his messages.

use serde_json::json;
use stackhour_bridge::media::{self, Attachment};
use stackhour_bridge::telegram::{Tg, TgConfig};
use stackhour_core::registry::PromptStore;
use std::io::{Read, Write};
use std::net::TcpListener;

const FAKE_TOKEN: &str = "123456:FAKE-TOKEN-FOR-TESTS-ONLY";

/// One canned reply.
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

/// Serves `replies` in order, recording each request line, then stops.
/// Returns the api root and a handle yielding the recorded request lines.
fn mock_api(replies: Vec<Reply>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for reply in replies {
            let (mut sock, _) = match listener.accept() {
                Ok(v) => v,
                Err(_) => break,
            };
            let mut buf = vec![0u8; 65536];
            let n = sock.read(&mut buf).unwrap_or(0);
            let raw = String::from_utf8_lossy(&buf[..n]).to_string();
            seen.push(raw.lines().next().unwrap_or("").to_string());
            let head = format!(
                "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.status,
                reply.content_type,
                reply.body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(&reply.body);
            let _ = sock.flush();
        }
        seen
    });
    (format!("http://{addr}"), handle)
}

fn json_reply(status: u16, v: serde_json::Value) -> Reply {
    Reply {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&v).unwrap(),
    }
}

fn client(api_root: &str) -> Tg {
    Tg::with_config(TgConfig::new(FAKE_TOKEN, 4242).with_api_root(api_root))
}

fn photo() -> Attachment {
    Attachment {
        kind: "image".into(),
        file_id: "AgACAgIAAx0-fake".into(),
        mime: Some("image/jpeg".into()),
        name: Some("telegram-photo.jpg".into()),
        size: None,
    }
}

#[test]
fn a_photo_round_trips_through_getfile_and_the_file_api() {
    let bytes = b"\xff\xd8\xff\xe0-not-really-a-jpeg".to_vec();
    let (root, server) = mock_api(vec![
        json_reply(
            200,
            json!({ "ok": true, "result": { "file_id": "AgACAgIAAx0-fake", "file_path": "photos/file_31.jpg", "file_size": 24 } }),
        ),
        Reply {
            status: 200,
            content_type: "image/jpeg",
            body: bytes.clone(),
        },
    ]);

    let dir = tempfile::tempdir().unwrap();
    let saved = media::download_media(
        &client(&root),
        &photo(),
        dir.path(),
        512 * 1024 * 1024,
        &PromptStore::new(None),
    )
    .expect("download");

    assert_eq!(std::fs::read(&saved.path).unwrap(), bytes);
    assert_eq!(saved.size, 24, "the getFile file_size is carried through");
    assert_eq!(saved.kind, "image");
    assert!(saved.path.extension().unwrap() == "jpg");

    let requests = server.join().unwrap();
    assert_eq!(
        requests,
        vec![
            format!("POST /bot{FAKE_TOKEN}/getFile HTTP/1.1"),
            format!("GET /file/bot{FAKE_TOKEN}/photos/file_31.jpg HTTP/1.1"),
        ],
        "the file API is a SEPARATE base from the bot API"
    );
}

/// Telegram refuses getFile for anything over 20 MB with a 400, the retry
/// ladder swallows it to `None` with no retry, and the user sees the generic
/// message rather than either MB error. Unhelpful, and the contract.
#[test]
fn an_oversized_file_dies_at_getfile_with_the_generic_message_and_no_retry() {
    let (root, server) = mock_api(vec![json_reply(
        400,
        json!({ "ok": false, "error_code": 400, "description": "Bad Request: file is too big" }),
    )]);

    let dir = tempfile::tempdir().unwrap();
    let err = media::download_media(
        &client(&root),
        &photo(),
        dir.path(),
        512 * 1024 * 1024,
        &PromptStore::new(None),
    )
    .unwrap_err();

    assert_eq!(err, "Telegram did not return a downloadable file path.");
    assert_eq!(
        server.join().unwrap().len(),
        1,
        "400 is terminal: exactly one getFile, no retry ladder"
    );
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[test]
fn a_non_ok_file_api_response_reports_the_status_and_writes_nothing() {
    let (root, server) = mock_api(vec![
        json_reply(
            200,
            json!({ "ok": true, "result": { "file_path": "photos/gone.jpg" } }),
        ),
        Reply {
            status: 404,
            content_type: "text/plain",
            body: b"Not Found".to_vec(),
        },
    ]);

    let dir = tempfile::tempdir().unwrap();
    let err = media::download_media(
        &client(&root),
        &photo(),
        dir.path(),
        u64::MAX,
        &PromptStore::new(None),
    )
    .unwrap_err();

    assert_eq!(err, "Telegram download failed (HTTP 404).");
    assert_eq!(server.join().unwrap().len(), 2);
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

/// A voice note takes the same download path but with the `.ogg` fallback
/// extension when the file_path carries no usable one.
#[test]
fn a_voice_note_lands_as_an_ogg_and_feeds_the_transcriber() {
    let (root, server) = mock_api(vec![
        json_reply(
            200,
            json!({ "ok": true, "result": { "file_path": "voice/file_9" } }),
        ),
        Reply {
            status: 200,
            content_type: "audio/ogg",
            body: b"OggS-fake".to_vec(),
        },
    ]);

    let dir = tempfile::tempdir().unwrap();
    let voice = Attachment {
        kind: "audio".into(),
        file_id: "AwACAgIAAx0-fake".into(),
        mime: Some("audio/ogg".into()),
        name: Some("telegram-voice.ogg".into()),
        size: Some(9),
    };
    let saved = media::download_media(
        &client(&root),
        &voice,
        dir.path(),
        u64::MAX,
        &PromptStore::new(None),
    )
    .expect("download");

    assert_eq!(
        saved.path.extension().unwrap(),
        "ogg",
        "no extension on the file_path -> the audio fallback"
    );
    assert_eq!(saved.name, "telegram-voice.ogg");
    assert_eq!(saved.size, 9);
    server.join().unwrap();
}
