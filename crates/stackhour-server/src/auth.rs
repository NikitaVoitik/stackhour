//! Request authentication.
//!
//! Open mode when no tokens are configured anywhere. Token extraction
//! precedence: Bearer -> Basic (lenient base64 decode, strip exactly ONE
//! trailing ':') -> `?api_key`, with NO fallback past a PRESENT Authorization
//! header. Timing-safe compare via constant_time_eq wrapped so a length
//! mismatch returns false and never panics. A malformed `server.tokens` map
//! is silently ignored by HTTP auth (while the token CLI hard-errors on it —
//! two deliberate behaviours for one field).

use axum::http::HeaderMap;
use serde_json::Value;

/// Who a request is authenticated as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// No tokens configured anywhere: everything allowed.
    Open,
    /// Matched the legacy global `server.token`.
    Global,
    /// Matched `server.tokens[<machine>]`.
    Machine(String),
}

/// JS truthiness of a config value. Missing, `null`, `false`, `0`/`-0`/`NaN`
/// and `""` are falsy; EVERYTHING else — including objects and arrays — is
/// truthy, exactly as `serverConfig.token || ''` sees it in src/server.js.
///
/// This is deliberately separate from [`truthy_string`]: whether auth is
/// ENABLED is a truthiness question, whether a supplied token can MATCH is a
/// stringification question. Conflating them is a fail-open bug — a config
/// with `"token": {}` must lock the server, not open it.
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => !n.as_f64().map(|f| f == 0.0 || f.is_nan()).unwrap_or(false),
        // Objects and arrays are truthy in JS.
        Some(_) => true,
    }
}

/// JS `String(value)` for the scalar shapes a config token can hold, paired
/// with JS truthiness. Returns `None` for anything falsy (missing, null,
/// `false`, `0`, `""`) so callers can mirror `token && safeEqual(...)`.
///
/// Objects and arrays return `None` too, but callers MUST NOT read that as
/// "no token configured" — use [`is_truthy`] for that question. Here `None`
/// only means "nothing a supplied token could usefully equal", so such a
/// value never matches and auth stays closed.
fn truthy_string(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if !is_truthy(Some(value)) {
        return None;
    }
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Bool(true) => Some("true".to_string()),
        // `String(1.0)` is "1" in JS, not "1.0" — go through the shared
        // number formatter rather than serde_json's Display.
        Value::Number(_) => Some(stackhour_core::jsnum::js_display(value)),
        _ => None,
    }
}

/// Node's `Buffer.from(s, 'base64')`: forgiving. Characters outside the
/// alphabet are skipped, `-`/`_` are accepted as `+`/`/`, padding is optional,
/// and a trailing group holding a single character is dropped.
fn decode_base64_lenient(input: &str) -> Vec<u8> {
    fn sextet(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }

    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let Some(value) = sextet(byte) else { continue };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    out
}

/// Extract the supplied token, mirroring `requestToken` in `src/server.js`.
///
/// A present `Authorization: Bearer …` or `Basic …` header wins outright — no
/// fallback to `?api_key` even when the token it yields is empty.
fn request_token(headers: &HeaderMap, query: &str) -> String {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Some(rest) = header.strip_prefix("Bearer ") {
        return rest.to_string();
    }
    if let Some(rest) = header.strip_prefix("Basic ") {
        let decoded = String::from_utf8_lossy(&decode_base64_lenient(rest)).into_owned();
        // WakaTime plugins send base64(api_key) or base64(api_key + ':').
        return match decoded.strip_suffix(':') {
            Some(stripped) => stripped.to_string(),
            None => decoded,
        };
    }

    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "api_key")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// Authenticate a request against the raw `server` config section.
/// `query` is the raw query string (for `?api_key`). None = unauthorized.
pub fn authenticate(headers: &HeaderMap, query: &str, server_cfg: &Value) -> Option<Principal> {
    // A malformed tokens field (array, string, number, …) is silently ignored.
    let tokens = server_cfg
        .get("tokens")
        .and_then(Value::as_object)
        .filter(|map| !map.is_empty());
    let legacy_configured = is_truthy(server_cfg.get("token"));
    let legacy = truthy_string(server_cfg.get("token"));

    // Open mode is decided by TRUTHINESS, not by whether we could turn the
    // value into a comparable string. `"token": {}` is truthy in Node, so it
    // keeps auth on (and simply never matches) instead of opening the server.
    if !legacy_configured && tokens.is_none() {
        return Some(Principal::Open);
    }

    let supplied = request_token(headers, query);

    if let Some(legacy) = &legacy {
        if safe_equal(supplied.as_bytes(), legacy.as_bytes()) {
            return Some(Principal::Global);
        }
    }
    if let Some(tokens) = tokens {
        for (machine, value) in tokens {
            // Empty / falsy token values are skipped, never matched.
            let Some(token) = truthy_string(Some(value)) else {
                continue;
            };
            if safe_equal(supplied.as_bytes(), token.as_bytes()) {
                return Some(Principal::Machine(machine.clone()));
            }
        }
    }
    None
}

/// Whether this principal may write rows for `machine` (`'unknown'` default
/// applied to a missing machine).
pub fn allows_machine(p: &Principal, machine: Option<&str>) -> bool {
    match p {
        Principal::Open | Principal::Global => true,
        // JS: String(machine || 'unknown') — an empty string is falsy too.
        Principal::Machine(owner) => {
            let name = machine.filter(|m| !m.is_empty()).unwrap_or("unknown");
            name == owner
        }
    }
}

/// Timing-safe equality: length checked first -> false; never panics.
pub fn safe_equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && constant_time_eq::constant_time_eq(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::AUTHORIZATION;
    use serde_json::json;

    fn headers(auth: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(value) = auth {
            h.insert(AUTHORIZATION, value.parse().expect("valid header value"));
        }
        h
    }

    /// Regression (fail-open): a `server.token` holding an object or array is
    /// TRUTHY in Node, so `legacy = serverConfig.token || ''` keeps auth on and
    /// every request 401s. Rust used to stringify it to `None` and treat that
    /// as "no tokens configured", dropping the whole server into open mode —
    /// ingest, agent-status and the WakaTime bulk endpoints writable by anyone
    /// who could reach the port.
    #[test]
    fn a_non_string_token_keeps_auth_closed_instead_of_opening_the_server() {
        for weird in [json!({}), json!([]), json!({"mac": "abc"}), json!(["abc"])] {
            let cfg = json!({ "token": weird, "tokens": {} });
            assert_eq!(
                authenticate(&headers(None), "", &cfg),
                None,
                "token={weird} must be unauthorized, never Principal::Open"
            );
            // And it can never be matched by guessing its stringification.
            assert_eq!(
                authenticate(&headers(Some("Bearer [object Object]")), "", &cfg),
                None
            );
        }
    }

    /// The open-mode path itself must survive: genuinely falsy token values
    /// still mean "no tokens configured anywhere".
    #[test]
    fn falsy_token_values_still_open_the_server() {
        for falsy in [json!(null), json!(""), json!(false), json!(0)] {
            assert_eq!(
                authenticate(&headers(None), "", &json!({ "token": falsy })),
                Some(Principal::Open),
                "token={falsy} is falsy and must open"
            );
        }
        assert_eq!(
            authenticate(&headers(None), "", &json!({})),
            Some(Principal::Open)
        );
    }

    /// Parity nit: JS `String(1.0)` is "1", not "1.0". A numeric token that
    /// authenticated against Node must authenticate here.
    #[test]
    fn a_numeric_token_stringifies_the_way_js_does() {
        let cfg = json!({ "token": 1.0 });
        assert_eq!(
            authenticate(&headers(Some("Bearer 1")), "", &cfg),
            Some(Principal::Global)
        );
        assert_eq!(authenticate(&headers(Some("Bearer 1.0")), "", &cfg), None);
    }

    fn b64(s: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[test]
    fn safe_equal_length_mismatch_is_false_not_panic() {
        assert!(!safe_equal(b"abc", b"abcd"));
        assert!(!safe_equal(b"", b"a"));
        assert!(safe_equal(b"", b""));
        assert!(safe_equal(b"secret", b"secret"));
        assert!(!safe_equal(b"secret", b"secreT"));
    }

    #[test]
    fn open_mode_when_nothing_configured() {
        let cfg = json!({ "token": "", "tokens": {} });
        assert_eq!(authenticate(&headers(None), "", &cfg), Some(Principal::Open));
        assert_eq!(
            authenticate(&headers(None), "", &json!({})),
            Some(Principal::Open)
        );
    }

    #[test]
    fn malformed_tokens_map_is_ignored() {
        // An array (or any non-object) tokens field does not enable auth.
        let cfg = json!({ "token": "", "tokens": ["a", "b"] });
        assert_eq!(authenticate(&headers(None), "", &cfg), Some(Principal::Open));
        // …but a legacy token alongside it still gates requests.
        let cfg = json!({ "token": "legacy", "tokens": "nonsense" });
        assert_eq!(authenticate(&headers(None), "", &cfg), None);
        assert_eq!(
            authenticate(&headers(Some("Bearer legacy")), "", &cfg),
            Some(Principal::Global)
        );
    }

    #[test]
    fn bearer_and_machine_tokens() {
        let cfg = json!({ "token": "", "tokens": { "mac": "m-tok", "gcp": "g-tok" } });
        assert_eq!(
            authenticate(&headers(Some("Bearer m-tok")), "", &cfg),
            Some(Principal::Machine("mac".into()))
        );
        assert_eq!(
            authenticate(&headers(Some("Bearer g-tok")), "", &cfg),
            Some(Principal::Machine("gcp".into()))
        );
        assert_eq!(authenticate(&headers(Some("Bearer nope")), "", &cfg), None);
    }

    #[test]
    fn empty_token_values_never_match() {
        let cfg = json!({ "token": "", "tokens": { "mac": "", "gcp": "g" } });
        // The supplied empty token must not match the empty map entry.
        assert_eq!(authenticate(&headers(Some("Bearer ")), "", &cfg), None);
        assert_eq!(authenticate(&headers(None), "", &cfg), None);
    }

    #[test]
    fn basic_auth_strips_exactly_one_trailing_colon() {
        let cfg = json!({ "token": "secret", "tokens": {} });
        let with = format!("Basic {}", b64("secret:"));
        let without = format!("Basic {}", b64("secret"));
        assert_eq!(
            authenticate(&headers(Some(&with)), "", &cfg),
            Some(Principal::Global)
        );
        assert_eq!(
            authenticate(&headers(Some(&without)), "", &cfg),
            Some(Principal::Global)
        );

        // Only ONE colon is stripped.
        let cfg2 = json!({ "token": "secret:", "tokens": {} });
        let two = format!("Basic {}", b64("secret::"));
        assert_eq!(
            authenticate(&headers(Some(&two)), "", &cfg2),
            Some(Principal::Global)
        );
    }

    #[test]
    fn basic_decode_is_lenient_and_never_panics() {
        assert_eq!(decode_base64_lenient("c2Vjc mV0"), b"secret".to_vec());
        assert_eq!(decode_base64_lenient("c2VjcmV0"), b"secret".to_vec());
        // Missing padding and url-alphabet characters both decode.
        assert_eq!(decode_base64_lenient("c2VjcmV"), b"secre".to_vec());
        assert_eq!(decode_base64_lenient("Pz8_"), b"???".to_vec());
        // Garbage decodes to something (possibly empty) rather than erroring.
        let cfg = json!({ "token": "secret", "tokens": {} });
        assert_eq!(authenticate(&headers(Some("Basic !!!!")), "", &cfg), None);
        assert_eq!(authenticate(&headers(Some("Basic")), "", &cfg), None);
    }

    #[test]
    fn api_key_query_param_used_only_without_bearer_or_basic() {
        let cfg = json!({ "token": "secret", "tokens": {} });
        assert_eq!(
            authenticate(&headers(None), "api_key=secret", &cfg),
            Some(Principal::Global)
        );
        // A present Bearer header wins: no fallback to ?api_key.
        assert_eq!(
            authenticate(&headers(Some("Bearer wrong")), "api_key=secret", &cfg),
            None
        );
        // 'Basic' without a trailing space is not a recognised prefix, so JS
        // falls through to ?api_key.
        assert_eq!(
            authenticate(&headers(Some("Basic")), "api_key=secret", &cfg),
            Some(Principal::Global)
        );
    }

    #[test]
    fn unrecognised_auth_scheme_falls_through_to_api_key() {
        let cfg = json!({ "token": "secret", "tokens": {} });
        assert_eq!(
            authenticate(&headers(Some("Token secret")), "api_key=secret", &cfg),
            Some(Principal::Global)
        );
    }

    #[test]
    fn query_param_is_url_decoded_and_first_wins() {
        let cfg = json!({ "token": "a b", "tokens": {} });
        assert_eq!(
            authenticate(&headers(None), "x=1&api_key=a%20b&api_key=zz", &cfg),
            Some(Principal::Global)
        );
    }

    #[test]
    fn allows_machine_defaults_to_unknown() {
        assert!(allows_machine(&Principal::Open, None));
        assert!(allows_machine(&Principal::Open, Some("anything")));
        assert!(allows_machine(&Principal::Global, Some("anything")));

        let p = Principal::Machine("mac".into());
        assert!(allows_machine(&p, Some("mac")));
        assert!(!allows_machine(&p, Some("gcp")));
        assert!(!allows_machine(&p, None));
        assert!(!allows_machine(&p, Some("")));

        let unknown = Principal::Machine("unknown".into());
        assert!(allows_machine(&unknown, None));
        assert!(allows_machine(&unknown, Some("")));
        assert!(allows_machine(&unknown, Some("unknown")));
    }
}
