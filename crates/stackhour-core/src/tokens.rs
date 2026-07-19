//! Machine-token CRUD over the RAW config Value, plus enrollment codes.
//!
//! All writes preserve key order and unknown keys (`tokens ??= {}` mutation
//! reproduced). Secrets never appear inside error strings.
//!
//! Ports: src/tokens.js (createMachineToken / revokeMachineToken /
//! listMachineTokens) and src/setup.js (generateToken / validUrl /
//! createEnrollment / parseEnrollment / writeConfig's serialization shape).
//!
//! Known documented divergences from Node:
//! - `token list` sorts by UTF-16 code-unit order (matching JS default
//!   `Array.prototype.sort`), which we reproduce exactly — including the
//!   astral-plane quirk where surrogates (0xD800–0xDFFF) sort BEFORE
//!   U+E000–U+FFFF.
//! - A machine name that looks like an array index (e.g. "42") is appended in
//!   file order by serde's preserve_order map; Node's JSON round-trip hoists
//!   integer-like keys to the front of `server.tokens`. Byte-level file
//!   parity diverges for such names; semantics do not.
//! - I/O and JSON error DETAIL strings (after the stable `cannot read
//!   config: ` prefix) come from Rust, not Node.

use base64::Engine as _;
use serde_json::{Map, Value};
use std::path::Path;

use crate::jsnum::{js_display, js_truthy};
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// JS string helpers
// ---------------------------------------------------------------------------

/// ECMAScript `String.prototype.trim`: WhiteSpace (TAB VT FF SP NBSP ZWNBSP +
/// Zs) plus LineTerminator (LF CR LS PS). Rust's `char::is_whitespace`
/// matches except it also strips U+0085 NEL (JS does not) and misses U+FEFF
/// ZWNBSP (JS strips it).
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c == '\u{FEFF}' || (c.is_whitespace() && c != '\u{0085}'))
}

/// JS `string.length` (UTF-16 code units).
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// JS default sort-comparator ordering: lexicographic over UTF-16 code units.
fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `String(payload.field || '').trim()`: absent or falsy values become '',
/// anything else is String()-coerced then trimmed.
fn field_trim(v: Option<&Value>) -> String {
    match v {
        Some(v) if js_truthy(v) => js_trim(&js_display(v)).to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Token generation
// ---------------------------------------------------------------------------

/// 32 random bytes, base64url no-pad => 43-char token.
/// (JS: `crypto.randomBytes(32).toString('base64url')`.)
///
/// Panics only if the OS random source is unavailable — the moral equivalent
/// of `crypto.randomBytes` throwing uncaught in the JS implementation.
pub fn generate_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("OS random source unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

// ---------------------------------------------------------------------------
// Raw-config plumbing
// ---------------------------------------------------------------------------

/// `machineName(value)`: trim, then 1–200 UTF-16 units, no C0 controls or
/// DEL. Never echoes the value into the error (secret hygiene).
fn machine_name(value: &str) -> Result<String> {
    let machine = js_trim(value);
    let bad_char = machine
        .chars()
        .any(|c| ('\u{0000}'..='\u{001f}').contains(&c) || c == '\u{007f}');
    if machine.is_empty() || utf16_len(machine) > 200 || bad_char {
        return Err(Error::msg("machine must be 1-200 printable characters"));
    }
    Ok(machine.to_string())
}

/// The loaded raw config plus the working view of `server.tokens`.
///
/// The JS code does `config.server.tokens ??= {}` in place; when
/// `config.server` is an ARRAY that property attaches to the array object and
/// is silently dropped by `JSON.stringify` — mutations succeed in memory but
/// never reach disk. `phantom` reproduces that: a detached map that is read
/// and written by the CRUD ops but not serialized.
struct LoadedConfig {
    config: Value,
    phantom: Option<Map<String, Value>>,
}

impl LoadedConfig {
    fn tokens(&self) -> &Map<String, Value> {
        match &self.phantom {
            Some(m) => m,
            None => self.config["server"]["tokens"]
                .as_object()
                .expect("read_server_config guarantees an object"),
        }
    }

    fn tokens_mut(&mut self) -> &mut Map<String, Value> {
        match &mut self.phantom {
            Some(m) => m,
            None => self.config["server"]["tokens"]
                .as_object_mut()
                .expect("read_server_config guarantees an object"),
        }
    }
}

/// `readServerConfig(configPath)` from src/tokens.js, over the raw Value.
///
/// - unreadable / unparsable file -> `cannot read config: <detail>`
/// - root `null` -> Node's uncaught TypeError message (caught by the CLI
///   wrapper just like any Error)
/// - falsy `server` -> `server is not initialized`
/// - `tokens ??= {}` (null/absent replaced in place, order preserved)
/// - non-object tokens -> `server.tokens must be an object keyed by machine
///   name`
fn read_server_config(cfg_path: &Path) -> Result<LoadedConfig> {
    let text =
        std::fs::read_to_string(cfg_path).map_err(|e| Error::msg(format!("cannot read config: {e}")))?;
    let mut config: Value =
        serde_json::from_str(&text).map_err(|e| Error::msg(format!("cannot read config: {e}")))?;

    if config.is_null() {
        // JS: `config.server` on null throws outside the try/catch.
        return Err(Error::msg("Cannot read properties of null (reading 'server')"));
    }
    if !config.get("server").map(js_truthy).unwrap_or(false) {
        return Err(Error::msg("server is not initialized"));
    }

    let phantom = match config.get_mut("server").expect("server presence checked above") {
        Value::Object(server) => {
            if matches!(server.get("tokens"), None | Some(Value::Null)) {
                // `??=`: replaces null in place (position kept), appends when
                // absent — indexmap insert matches JS object semantics.
                server.insert("tokens".to_string(), Value::Object(Map::new()));
            }
            match server.get("tokens") {
                Some(Value::Object(_)) => None,
                // arrays and scalars: typeof !== 'object' / Array.isArray
                _ => {
                    return Err(Error::msg(
                        "server.tokens must be an object keyed by machine name",
                    ));
                }
            }
        }
        // Array: `tokens` attaches to the array object in JS and is lost on
        // stringify. Work against a detached empty map.
        Value::Array(_) => Some(Map::new()),
        // Truthy primitive: strict-mode `??=` assignment throws TypeError.
        other => {
            let type_name = match other {
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Bool(_) => "boolean",
                _ => unreachable!("null/object/array handled above"),
            };
            return Err(Error::msg(format!(
                "Cannot create property 'tokens' on {type_name} '{}'",
                js_display(other)
            )));
        }
    };

    Ok(LoadedConfig { config, phantom })
}

/// `writeConfig(configPath, config)`: `JSON.stringify(config, null, 2)+'\n'`,
/// written via the atomic 0600 ritual (wx tmp, fsync, rename, chmod,
/// best-effort dir fsync).
pub fn write_raw_config(cfg_path: &Path, config: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(config)?;
    text.push('\n');
    crate::fsutil::atomic_write_0600(cfg_path, text.as_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Machine-token CRUD
// ---------------------------------------------------------------------------

/// `stackhour token create`: validates the machine name (1–200 printable
/// chars), rejects duplicate secrets across machines, requires `--force` to
/// rotate an existing machine's token. Returns `(machine, token)`.
///
/// `token: None` means "generate one" (JS default parameter); `Some(s)` is
/// trimmed and must be non-empty — an explicitly-passed empty string does NOT
/// fall back to a generated token (`String(token || '')`).
pub fn create_machine_token(
    cfg_path: &Path,
    machine: &str,
    token: Option<String>,
    force: bool,
) -> Result<(String, String)> {
    let name = machine_name(machine)?;
    let mut loaded = read_server_config(cfg_path)?;

    if loaded.tokens().contains_key(&name) && !force {
        return Err(Error::msg(format!(
            "token for {name} already exists; pass --force to rotate it"
        )));
    }
    let secret = match token {
        None => generate_token(),
        Some(s) => js_trim(&s).to_string(),
    };
    if secret.is_empty() {
        return Err(Error::msg("token cannot be empty"));
    }
    let duplicate = loaded
        .tokens()
        .iter()
        .any(|(other, value)| other != &name && matches!(value, Value::String(s) if s == &secret));
    if duplicate {
        return Err(Error::msg("token is already assigned to another machine"));
    }

    loaded
        .tokens_mut()
        .insert(name.clone(), Value::String(secret.clone()));
    write_raw_config(cfg_path, &loaded.config)?;
    Ok((name, secret))
}

/// `stackhour token revoke`.
pub fn revoke_machine_token(cfg_path: &Path, machine: &str) -> Result<()> {
    let name = machine_name(machine)?;
    let mut loaded = read_server_config(cfg_path)?;
    if !loaded.tokens().contains_key(&name) {
        return Err(Error::msg(format!("no token exists for {name}")));
    }
    // shift_remove: JS `delete` keeps the relative order of remaining keys.
    loaded.tokens_mut().shift_remove(&name);
    write_raw_config(cfg_path, &loaded.config)?;
    Ok(())
}

/// `stackhour token list`: machine names sorted by UTF-16 code-unit order
/// (JS default sort; divergence risk only for astral-plane characters, which
/// we reproduce). Never returns secrets. Does NOT rewrite the config file
/// even when it normalizes a null `tokens` in memory.
pub fn list_machine_tokens(cfg_path: &Path) -> Result<Vec<String>> {
    let loaded = read_server_config(cfg_path)?;
    let mut names: Vec<String> = loaded.tokens().keys().cloned().collect();
    names.sort_by(|a, b| utf16_cmp(a, b));
    Ok(names)
}

// ---------------------------------------------------------------------------
// Enrollment codes
// ---------------------------------------------------------------------------

/// A parsed enrollment code (`v:1` payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrollment {
    pub server_url: String,
    pub machine: String,
    pub token: String,
}

/// Build an enrollment code: JSON `{v:1, serverUrl, machine, token}` encoded
/// base64url no-pad. Validation order matches JS: URL first, then machine,
/// then token.
pub fn create_enrollment(server_url: &str, machine: &str, token: &str) -> Result<String> {
    let clean_url = valid_url(server_url)?;
    let machine = js_trim(machine);
    let token = js_trim(token);
    if machine.is_empty() {
        return Err(Error::msg("enrollment machine cannot be empty"));
    }
    if token.is_empty() {
        return Err(Error::msg("enrollment token cannot be empty"));
    }
    // preserve_order keeps this literal key order; compact stringify matches
    // JSON.stringify.
    let payload = serde_json::json!({
        "v": 1,
        "serverUrl": clean_url,
        "machine": machine,
        "token": token,
    });
    let text = serde_json::to_string(&payload)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text))
}

/// Parse an enrollment code: charset + <=16384 length cap; decoding is
/// LENIENT (Node Buffer.from forgiveness). Errors: `invalid enrollment code`
/// (malformed) vs `unsupported enrollment code` (not an object with v === 1).
pub fn parse_enrollment(code: &str) -> Result<Enrollment> {
    let invalid = || Error::msg("invalid enrollment code");

    // /^[A-Za-z0-9_-]+$/ plus the length cap (the charset is ASCII, so byte
    // length == UTF-16 length for anything that can pass; over-long
    // non-ASCII strings fail the charset check with the same error).
    let charset_ok = !code.is_empty()
        && code.len() <= 16_384
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !charset_ok {
        return Err(invalid());
    }

    let bytes = lenient_base64_decode(code);
    // Buffer#toString('utf8') is lossy (U+FFFD replacement), then JSON.parse.
    let text = String::from_utf8_lossy(&bytes);
    let payload: Value = serde_json::from_str(&text).map_err(|_| invalid())?;

    // JS: !payload || payload.v !== 1 || typeof payload !== 'object' ||
    // Array.isArray(payload). Everything that is not an object with v === 1
    // (strict numeric equality, so 1.0 passes and "1" does not) is
    // "unsupported".
    let obj = match &payload {
        Value::Object(o) => o,
        _ => return Err(Error::msg("unsupported enrollment code")),
    };
    let v_is_one = matches!(obj.get("v"), Some(Value::Number(n)) if n.as_f64() == Some(1.0));
    if !v_is_one {
        return Err(Error::msg("unsupported enrollment code"));
    }

    // Missing serverUrl -> `new URL(undefined)` -> "must be a valid" error.
    let raw_url = match obj.get("serverUrl") {
        Some(v) => js_display(v),
        None => "undefined".to_string(),
    };
    Ok(Enrollment {
        server_url: valid_url(&raw_url)?,
        machine: field_trim(obj.get("machine")),
        token: field_trim(obj.get("token")),
    })
}

/// WHATWG URL validation for server URLs: http/https only, WHATWG
/// normalisation (host lowercasing, default-port dropping, path
/// normalisation), then exactly one trailing slash stripped from the href.
pub fn valid_url(u: &str) -> Result<String> {
    let parsed = url::Url::parse(u).map_err(|_| Error::msg("server URL must be a valid http(s) URL"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(Error::msg("server URL must use http or https"));
    }
    let href = parsed.as_str();
    Ok(href.strip_suffix('/').unwrap_or(href).to_string())
}

/// Lenient base64 / base64url decode matching Node `Buffer.from(s, 'base64')`
/// forgiveness (verified against Node v22):
/// - both alphabets accepted interchangeably (`+`/`-` = 62, `/`/`_` = 63)
/// - any invalid character (including all whitespace) is SKIPPED
/// - `=` terminates decoding entirely
/// - a trailing partial group yields its complete bytes: 2 chars -> 1 byte,
///   3 chars -> 2 bytes, 1 char -> 0 bytes.
pub fn lenient_base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 2);
    let mut acc: u32 = 0;
    let mut have: u32 = 0;
    for b in s.bytes() {
        let sextet = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue, // skipped, not rejected
        };
        acc = (acc << 6) | sextet;
        have += 1;
        if have == 4 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            acc = 0;
            have = 0;
        }
    }
    match have {
        2 => out.push((acc >> 4) as u8),
        3 => {
            out.push((acc >> 10) as u8);
            out.push((acc >> 2) as u8);
        }
        _ => {} // 0 or 1 leftover chars: no bytes
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg_file(dir: &tempfile::TempDir, value: &Value) -> std::path::PathBuf {
        let path = dir.path().join("nested").join("config.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = serde_json::to_string_pretty(value).unwrap();
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        path
    }

    fn read_raw(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    // ---- generate_token ------------------------------------------------

    #[test]
    fn generate_token_is_43_char_base64url() {
        let t = generate_token();
        assert_eq!(t.len(), 43);
        assert!(t
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
        assert_ne!(generate_token(), generate_token());
    }

    // ---- valid_url -----------------------------------------------------

    #[test]
    fn valid_url_normalizes_and_strips_one_slash() {
        assert_eq!(valid_url("http://h").unwrap(), "http://h");
        assert_eq!(valid_url("http://h/").unwrap(), "http://h");
        assert_eq!(valid_url("http://h/x/").unwrap(), "http://h/x");
        // Only ONE trailing slash stripped.
        assert_eq!(valid_url("http://h/x//").unwrap(), "http://h/x/");
        // WHATWG: host lowercased, default port dropped.
        assert_eq!(valid_url("http://Host:80/").unwrap(), "http://host");
        assert_eq!(valid_url("https://Host:443/a").unwrap(), "https://host/a");
        assert_eq!(valid_url("HTTP://H").unwrap(), "http://h");
        // Non-default port kept.
        assert_eq!(valid_url("http://h:4040/").unwrap(), "http://h:4040");
    }

    #[test]
    fn valid_url_errors() {
        assert_eq!(
            valid_url("not a url").unwrap_err().message(),
            "server URL must be a valid http(s) URL"
        );
        assert_eq!(
            valid_url("").unwrap_err().message(),
            "server URL must be a valid http(s) URL"
        );
        assert_eq!(
            valid_url("ftp://h").unwrap_err().message(),
            "server URL must use http or https"
        );
        assert_eq!(
            valid_url("file:///x").unwrap_err().message(),
            "server URL must use http or https"
        );
    }

    // ---- lenient_base64_decode (golden values from Node v22) -----------

    #[test]
    fn lenient_decode_matches_node() {
        assert_eq!(lenient_base64_decode("aGVsbG8"), b"hello");
        assert_eq!(lenient_base64_decode("aGVsbG8="), b"hello");
        assert_eq!(lenient_base64_decode("aGVsbG8h"), b"hello!");
        assert_eq!(lenient_base64_decode("aGVsbG8hIQ"), b"hello!!");
        // Whitespace and invalid chars skipped.
        assert_eq!(lenient_base64_decode("aGV sbG8="), b"hello");
        assert_eq!(lenient_base64_decode("aGV\tsbG8"), b"hello");
        assert_eq!(lenient_base64_decode("aGV!sbG8"), b"hello");
        // '=' terminates.
        assert_eq!(lenient_base64_decode("aGVs=bG8"), b"hel");
        assert_eq!(lenient_base64_decode("aGVs==bG8"), b"hel");
        // Partial trailing groups.
        assert_eq!(lenient_base64_decode("a"), b"");
        assert_eq!(lenient_base64_decode("ab"), &[0x69]);
        assert_eq!(lenient_base64_decode("abc"), &[0x69, 0xb7]);
        assert_eq!(lenient_base64_decode("abcd"), &[0x69, 0xb7, 0x1d]);
        // Mixed alphabets.
        assert_eq!(lenient_base64_decode("-_-_"), &[0xfb, 0xff, 0xbf]);
        assert_eq!(lenient_base64_decode("++//"), &[0xfb, 0xef, 0xff]);
        assert_eq!(lenient_base64_decode(""), b"");
    }

    // ---- token CRUD ----------------------------------------------------

    #[test]
    fn lifecycle_preserves_config_and_rotates_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let file = cfg_file(
            &dir,
            &json!({
                "server": {
                    "host": "127.0.0.1",
                    "port": 4242,
                    "db": "/preserve/stackhour.db",
                    "tokens": { "zed": "zed-secret" },
                    "customServerSetting": true
                },
                "agent": { "machine": "local", "token": "agent-secret" },
                "pricing": { "privateModel": { "input": 12 } }
            }),
        );

        let (machine, token) =
            create_machine_token(&file, "macbook", Some("mac-secret".into()), false).unwrap();
        assert_eq!((machine.as_str(), token.as_str()), ("macbook", "mac-secret"));

        let saved = read_raw(&file);
        assert_eq!(
            saved["server"]["tokens"],
            json!({ "zed": "zed-secret", "macbook": "mac-secret" })
        );
        assert_eq!(saved["server"]["customServerSetting"], json!(true));
        assert_eq!(
            saved["agent"],
            json!({ "machine": "local", "token": "agent-secret" })
        );
        assert_eq!(saved["pricing"]["privateModel"]["input"], json!(12));

        // 0600 mode on the written file.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Duplicate without --force.
        let err = create_machine_token(&file, "macbook", Some("x".into()), false).unwrap_err();
        assert_eq!(
            err.message(),
            "token for macbook already exists; pass --force to rotate it"
        );
        assert_eq!(read_raw(&file)["server"]["tokens"]["macbook"], "mac-secret");

        // --force rotates only the named credential.
        let (m, t) = create_machine_token(&file, "macbook", Some("rotated-secret".into()), true).unwrap();
        assert_eq!((m.as_str(), t.as_str()), ("macbook", "rotated-secret"));
        let saved = read_raw(&file);
        assert_eq!(saved["server"]["tokens"]["macbook"], "rotated-secret");
        assert_eq!(saved["server"]["tokens"]["zed"], "zed-secret");

        // List never returns secrets, sorted.
        let names = list_machine_tokens(&file).unwrap();
        assert_eq!(names, vec!["macbook".to_string(), "zed".to_string()]);

        // Revoke removes exactly one entry.
        revoke_machine_token(&file, "macbook").unwrap();
        assert_eq!(
            read_raw(&file)["server"]["tokens"],
            json!({ "zed": "zed-secret" })
        );
        let err = revoke_machine_token(&file, "macbook").unwrap_err();
        assert_eq!(err.message(), "no token exists for macbook");
    }

    #[test]
    fn create_preserves_top_level_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // Deliberately odd order: zebra first, server last.
        std::fs::write(
            &path,
            "{\"zebra\": 1, \"agent\": {\"token\": \"a\"}, \"server\": {\"tokens\": {}}}",
        )
        .unwrap();
        create_machine_token(&path, "m1", Some("s1".into()), false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let zebra = text.find("\"zebra\"").unwrap();
        let agent = text.find("\"agent\"").unwrap();
        let server = text.find("\"server\"").unwrap();
        assert!(zebra < agent && agent < server, "key order not preserved");
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn tokens_null_is_initialized_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let file = cfg_file(&dir, &json!({ "server": { "tokens": null, "keep": 1 } }));
        // list normalizes in memory but must NOT rewrite the file.
        let before = std::fs::read_to_string(&file).unwrap();
        assert_eq!(list_machine_tokens(&file).unwrap(), Vec::<String>::new());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        // create writes tokens as {} + entry, keeping tokens' position.
        create_machine_token(&file, "m", Some("s".into()), false).unwrap();
        let saved = read_raw(&file);
        assert_eq!(saved["server"]["tokens"], json!({ "m": "s" }));
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.find("\"tokens\"").unwrap() < text.find("\"keep\"").unwrap());
    }

    #[test]
    fn rejects_duplicate_secret_and_unsafe_names_without_leaks() {
        let dir = tempfile::tempdir().unwrap();
        let file = cfg_file(
            &dir,
            &json!({
                "server": { "tokens": { "linux": "same-private-secret" }, "keep": "server-setting" },
                "unrelated": { "token": "unrelated-private-secret" }
            }),
        );
        let before = std::fs::read_to_string(&file).unwrap();

        let long = "x".repeat(201);
        let cases: Vec<(&str, Option<String>, &str)> = vec![
            (
                "macbook",
                Some("same-private-secret".into()),
                "token is already assigned to another machine",
            ),
            (
                "",
                Some("new".into()),
                "machine must be 1-200 printable characters",
            ),
            (
                "   ",
                Some("new".into()),
                "machine must be 1-200 printable characters",
            ),
            (
                "bad\nmachine",
                Some("new".into()),
                "machine must be 1-200 printable characters",
            ),
            (
                long.as_str(),
                Some("new".into()),
                "machine must be 1-200 printable characters",
            ),
            ("macbook", Some(String::new()), "token cannot be empty"),
            ("macbook", Some("   ".into()), "token cannot be empty"),
        ];
        for (machine, token, expected) in cases {
            let err = create_machine_token(&file, machine, token, false).unwrap_err();
            assert_eq!(err.message(), expected);
            assert!(!err.message().contains("same-private-secret"));
            assert!(!err.message().contains("unrelated-private-secret"));
            assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        }

        // Same secret on the SAME machine with --force is fine.
        create_machine_token(&file, "linux", Some("same-private-secret".into()), true).unwrap();
    }

    #[test]
    fn machine_name_length_is_utf16_units() {
        // 100 astral chars = 200 UTF-16 units: allowed.
        let dir = tempfile::tempdir().unwrap();
        let file = cfg_file(&dir, &json!({ "server": { "tokens": {} } }));
        let name_200 = "\u{1F600}".repeat(100);
        create_machine_token(&file, &name_200, Some("s1".into()), false).unwrap();
        // 101 astral chars = 202 units: rejected.
        let name_202 = "\u{1F600}".repeat(101);
        let err = create_machine_token(&file, &name_202, Some("s2".into()), false).unwrap_err();
        assert_eq!(err.message(), "machine must be 1-200 printable characters");
    }

    #[test]
    fn missing_file_and_uninitialized_server() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        let err = list_machine_tokens(&missing).unwrap_err();
        assert!(err.message().starts_with("cannot read config: "));

        for server in [json!(null), json!(false), json!(0), json!("")] {
            let file = cfg_file(&dir, &json!({ "server": server, "private": "do-not-print" }));
            let err = list_machine_tokens(&file).unwrap_err();
            assert_eq!(err.message(), "server is not initialized");
            assert!(!err.message().contains("do-not-print"));
        }
        // Missing server key entirely.
        let file = cfg_file(&dir, &json!({ "private": "do-not-print" }));
        assert_eq!(
            list_machine_tokens(&file).unwrap_err().message(),
            "server is not initialized"
        );
    }

    #[test]
    fn null_root_matches_node_typeerror() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "null").unwrap();
        assert_eq!(
            list_machine_tokens(&path).unwrap_err().message(),
            "Cannot read properties of null (reading 'server')"
        );
    }

    #[test]
    fn truthy_primitive_server_matches_node_typeerror() {
        let dir = tempfile::tempdir().unwrap();
        for (server, msg) in [
            (json!(5), "Cannot create property 'tokens' on number '5'"),
            (json!("abc"), "Cannot create property 'tokens' on string 'abc'"),
            (json!(true), "Cannot create property 'tokens' on boolean 'true'"),
        ] {
            let file = cfg_file(&dir, &json!({ "server": server }));
            assert_eq!(list_machine_tokens(&file).unwrap_err().message(), msg);
        }
    }

    #[test]
    fn rejects_array_and_string_token_maps_without_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        for tokens in [json!(["array-secret"]), json!("string-secret")] {
            let file = cfg_file(
                &dir,
                &json!({
                    "server": { "tokens": tokens, "keep": "server-setting" },
                    "unrelated": { "token": "never-print-unrelated" }
                }),
            );
            let before = std::fs::read_to_string(&file).unwrap();
            let errors = [
                list_machine_tokens(&file).unwrap_err(),
                create_machine_token(&file, "macbook", Some("new-secret".into()), false).unwrap_err(),
                revoke_machine_token(&file, "macbook").unwrap_err(),
            ];
            for err in errors {
                assert_eq!(
                    err.message(),
                    "server.tokens must be an object keyed by machine name"
                );
                assert!(!err.message().contains("array-secret"));
                assert!(!err.message().contains("string-secret"));
                assert!(!err.message().contains("never-print-unrelated"));
            }
            assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        }
    }

    #[test]
    fn array_server_reproduces_js_phantom_tokens() {
        // JS attaches `tokens` to the array object; JSON.stringify drops it.
        let dir = tempfile::tempdir().unwrap();
        let file = cfg_file(&dir, &json!({ "server": [1, 2] }));
        let (m, t) = create_machine_token(&file, "ghost", Some("s".into()), false).unwrap();
        assert_eq!((m.as_str(), t.as_str()), ("ghost", "s"));
        // Token silently lost, as in JS.
        assert_eq!(read_raw(&file), json!({ "server": [1, 2] }));
        assert_eq!(list_machine_tokens(&file).unwrap(), Vec::<String>::new());
        assert_eq!(
            revoke_machine_token(&file, "ghost").unwrap_err().message(),
            "no token exists for ghost"
        );
    }

    #[test]
    fn generated_token_used_when_none_passed() {
        let dir = tempfile::tempdir().unwrap();
        let file = cfg_file(&dir, &json!({ "server": { "tokens": {} } }));
        let (_, token) = create_machine_token(&file, "gen", None, false).unwrap();
        assert_eq!(token.len(), 43);
        assert_eq!(read_raw(&file)["server"]["tokens"]["gen"], json!(token));
    }

    #[test]
    fn list_sorts_by_utf16_code_units() {
        let dir = tempfile::tempdir().unwrap();
        // U+1F600 encodes as surrogates D83D DE00 (< FF5A in UTF-16), while
        // its code point 0x1F600 is > 0xFF5A. UTF-16 order puts the astral
        // name FIRST; naive Rust string order would put it last.
        let file = cfg_file(
            &dir,
            &json!({ "server": { "tokens": { "\u{FF5A}": "a", "\u{1F600}": "b", "b": "c" } } }),
        );
        let names = list_machine_tokens(&file).unwrap();
        assert_eq!(
            names,
            vec!["b".to_string(), "\u{1F600}".to_string(), "\u{FF5A}".to_string()]
        );
    }

    #[test]
    fn revoke_preserves_remaining_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            "{\"server\":{\"tokens\":{\"a\":\"1\",\"b\":\"2\",\"c\":\"3\"}}}",
        )
        .unwrap();
        revoke_machine_token(&path, "b").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let a = text.find("\"a\"").unwrap();
        let c = text.find("\"c\"").unwrap();
        assert!(a < c, "shift_remove must keep remaining order");
        assert!(!text.contains("\"b\""));
    }

    // ---- enrollment ----------------------------------------------------

    #[test]
    fn enrollment_round_trip() {
        let code = create_enrollment("http://Host:80/x/", "  mac  ", " tok ").unwrap();
        assert!(code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
        // Payload is exactly JSON.stringify({v:1,serverUrl,machine,token}).
        let decoded = lenient_base64_decode(&code);
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "{\"v\":1,\"serverUrl\":\"http://host/x\",\"machine\":\"mac\",\"token\":\"tok\"}"
        );
        let parsed = parse_enrollment(&code).unwrap();
        assert_eq!(
            parsed,
            Enrollment {
                server_url: "http://host/x".to_string(),
                machine: "mac".to_string(),
                token: "tok".to_string(),
            }
        );
    }

    #[test]
    fn create_enrollment_errors_in_js_order() {
        // URL validated first.
        assert_eq!(
            create_enrollment("nope", "", "").unwrap_err().message(),
            "server URL must be a valid http(s) URL"
        );
        assert_eq!(
            create_enrollment("http://h", "  ", "tok").unwrap_err().message(),
            "enrollment machine cannot be empty"
        );
        assert_eq!(
            create_enrollment("http://h", "mac", "  ").unwrap_err().message(),
            "enrollment token cannot be empty"
        );
    }

    fn b64url(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
    }

    #[test]
    fn parse_enrollment_invalid_vs_unsupported() {
        // Malformed stage -> 'invalid enrollment code'.
        for bad in [
            "",
            "not base64!",
            "has space ",
            "césar",
            "abc.def",
            "QUJD", // "ABC" — passes charset, not JSON
        ] {
            assert_eq!(
                parse_enrollment(bad).unwrap_err().message(),
                "invalid enrollment code",
                "case: {bad:?}"
            );
        }
        // Over the 16384 cap (valid charset).
        let long = "A".repeat(16_385);
        assert_eq!(
            parse_enrollment(&long).unwrap_err().message(),
            "invalid enrollment code"
        );
        // Exactly at the cap: passes the charset stage, fails later (garbage).
        let at_cap = "A".repeat(16_384);
        assert_eq!(
            parse_enrollment(&at_cap).unwrap_err().message(),
            "invalid enrollment code"
        );

        // Well-formed JSON that is not an object with v === 1 -> 'unsupported'.
        for payload in [
            "null",
            "0",
            "false",
            "\"x\"",
            "[1,2]",
            "{}",
            "{\"v\":2}",
            "{\"v\":\"1\"}",
        ] {
            assert_eq!(
                parse_enrollment(&b64url(payload)).unwrap_err().message(),
                "unsupported enrollment code",
                "payload: {payload}"
            );
        }
        // v as float 1.0 satisfies === 1.
        let code = b64url("{\"v\":1.0,\"serverUrl\":\"http://h\",\"machine\":\"m\",\"token\":\"t\"}");
        assert_eq!(parse_enrollment(&code).unwrap().machine, "m");
    }

    #[test]
    fn parse_enrollment_coerces_fields_like_js() {
        // Missing serverUrl -> new URL(undefined) -> valid-url error.
        assert_eq!(
            parse_enrollment(&b64url("{\"v\":1}")).unwrap_err().message(),
            "server URL must be a valid http(s) URL"
        );
        // serverUrl present but wrong scheme.
        assert_eq!(
            parse_enrollment(&b64url("{\"v\":1,\"serverUrl\":\"ftp://h\"}"))
                .unwrap_err()
                .message(),
            "server URL must use http or https"
        );
        // Falsy machine/token -> ''; numbers are String()-coerced.
        let e = parse_enrollment(&b64url(
            "{\"v\":1,\"serverUrl\":\"http://h\",\"machine\":null,\"token\":42}",
        ))
        .unwrap();
        assert_eq!(e.machine, "");
        assert_eq!(e.token, "42");
        // Single-element array coerces to its element (JS String([x])).
        let e = parse_enrollment(&b64url(
            "{\"v\":1,\"serverUrl\":[\"http://h\"],\"machine\":\"m\",\"token\":\"t\"}",
        ))
        .unwrap();
        assert_eq!(e.server_url, "http://h");
    }

    #[test]
    fn parse_enrollment_lenient_length() {
        // 4n+1 chars: Node drops the dangling char; still decodes.
        let payload = "{\"v\":1,\"serverUrl\":\"http://h\",\"machine\":\"m\",\"token\":\"t\"}";
        let mut code = b64url(payload);
        while code.len() % 4 != 1 {
            // Reach a length ≡ 1 (mod 4) by appending harmless chars that the
            // lenient decoder drops as an incomplete final group.
            code.push('A');
        }
        // Bytes beyond the JSON text are only the dangling group; if we added
        // 1 char it contributes nothing. Only assert when that is the case.
        if code.len() == b64url(payload).len() + 1 {
            let parsed = parse_enrollment(&code).unwrap();
            assert_eq!(parsed.machine, "m");
        }
    }

    // ---- js string helpers ---------------------------------------------

    #[test]
    fn js_trim_matches_js_whitespace_set() {
        assert_eq!(js_trim("  x  "), "x");
        assert_eq!(js_trim("\u{FEFF}x\u{00A0}"), "x");
        assert_eq!(js_trim("\u{3000}x\u{2028}"), "x");
        // NEL is NOT trimmed by JS.
        assert_eq!(js_trim("\u{0085}x"), "\u{0085}x");
        assert_eq!(js_trim(""), "");
    }

    #[test]
    fn machine_name_control_chars_and_del() {
        assert!(machine_name("ok name").is_ok());
        assert!(machine_name("tab\tinside").is_err());
        assert!(machine_name("del\u{7f}").is_err());
        assert!(machine_name("\u{1f}x").is_err());
        // Unicode letters are fine.
        assert_eq!(machine_name("  máquina  ").unwrap(), "máquina");
    }
}
