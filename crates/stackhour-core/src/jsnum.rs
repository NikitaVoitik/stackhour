//! JS numeric / string-coercion semantics, centralized.
//!
//! The single defense against parity drift: every place the JS implementation
//! does `Number(x)`, `Math.round(x)`, `String(x || 'unknown')` or clamps a
//! query param goes through these helpers. Golden-tested against values
//! captured from the reference Node v22 implementation (see the `golden_*`
//! tests at the bottom; each expectation was produced by running the exact
//! JS expression under Node 22).

use serde_json::Value;

/// JS whitespace for `Number(string)` trimming: ECMAScript *StrWhiteSpace* is
/// WhiteSpace (TAB VT FF SP NBSP ZWNBSP + Zs) plus LineTerminator (LF CR LS PS).
/// Rust's `char::is_whitespace` matches that set except it also includes
/// U+0085 NEL (which JS does NOT trim) and excludes U+FEFF ZWNBSP (which JS
/// DOES trim).
fn is_js_whitespace(c: char) -> bool {
    c == '\u{FEFF}' || (c.is_whitespace() && c != '\u{0085}')
}

/// Parse the digits after a `0x`/`0o`/`0b` prefix (ES2015 non-decimal integer
/// literals, as accepted by `Number(string)`). Any non-digit char => NaN.
/// Accumulates exactly in u128 while possible (Rust's u128 -> f64 conversion is
/// correctly rounded, matching JS); falls back to float folding for absurdly
/// long literals (> 128 bits), where the last-ulp may differ — irrelevant in
/// practice and beyond any value stackhour handles.
fn parse_radix_digits(digits: &str, radix: u32) -> f64 {
    if digits.is_empty() {
        return f64::NAN;
    }
    let mut acc: u128 = 0;
    let mut f_acc = 0.0f64;
    let mut overflowed = false;
    for c in digits.chars() {
        let d = match c.to_digit(radix) {
            Some(d) => d,
            None => return f64::NAN,
        };
        if !overflowed {
            match acc
                .checked_mul(radix as u128)
                .and_then(|a| a.checked_add(d as u128))
            {
                Some(a) => {
                    acc = a;
                    continue;
                }
                None => {
                    overflowed = true;
                    f_acc = acc as f64;
                }
            }
        }
        f_acc = f_acc * radix as f64 + d as f64;
    }
    if overflowed {
        f_acc
    } else {
        acc as f64
    }
}

/// Validate a candidate against the JS *StrDecimalLiteral* grammar (optional
/// sign, digits with at most one '.', optional e/E exponent with mandatory
/// digits). Every string this accepts is also accepted by Rust's `f64::from_str`
/// with an identical (correctly rounded) result, so after validation we can
/// delegate the actual conversion. Validation is what rejects the extra forms
/// Rust would accept but JS would not ("inf", "nan", "infinity").
fn is_valid_js_decimal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut mantissa_digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        mantissa_digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            mantissa_digits += 1;
        }
    }
    if mantissa_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let mut exp_digits = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            exp_digits += 1;
        }
        if exp_digits == 0 {
            return false;
        }
    }
    i == b.len()
}

/// JS `Number(string)` — the *StringToNumber* abstract operation.
/// Trimmed empty string -> 0; `[+-]?Infinity`; unsigned `0x`/`0o`/`0b`
/// literals; StrDecimalLiteral; everything else NaN. Overflow -> ±Infinity
/// (e.g. "1e309"), like JS.
fn parse_js_number_str(s: &str) -> f64 {
    let t = s.trim_matches(is_js_whitespace);
    if t.is_empty() {
        return 0.0;
    }
    // Sign is only legal on decimal literals and Infinity, not on 0x/0o/0b.
    let (sign, unsigned) = match t.as_bytes()[0] {
        b'+' => (1.0, &t[1..]),
        b'-' => (-1.0, &t[1..]),
        _ => (1.0, t),
    };
    if unsigned == "Infinity" {
        return sign * f64::INFINITY;
    }
    if let Some(prefix) = t.get(0..2) {
        match prefix {
            "0x" | "0X" => return parse_radix_digits(&t[2..], 16),
            "0o" | "0O" => return parse_radix_digits(&t[2..], 8),
            "0b" | "0B" => return parse_radix_digits(&t[2..], 2),
            _ => {}
        }
    }
    if is_valid_js_decimal(t) {
        t.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// JS truthiness of a JSON value: `null`, `false`, `0` (incl. `-0`) and `''`
/// are falsy; every string with content, every non-zero number, `true`, and
/// EVERY array/object (even empty ones) are truthy.
pub fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// ECMAScript `Number::toString(x, 10)` — the exact spec algorithm, built on
/// the shortest-roundtrip digits (which Rust's float formatting also
/// produces): fixed notation for 1e-6 <= |x| < 1e21 with the shortest digit
/// string zero-padded (so `String(123456789012345680000)` prints
/// "123456789012345680000", not the exact binary value ...683968), exponential
/// notation with explicit `+` outside that range, "0" for ±0, "NaN"/"±Infinity"
/// for non-finite.
fn js_f64_to_string(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f == 0.0 {
        return "0".to_string();
    }
    if f.is_infinite() {
        return (if f > 0.0 { "Infinity" } else { "-Infinity" }).to_string();
    }
    let neg = f < 0.0;
    // Shortest-roundtrip decomposition: {:e} prints "d[.ddd]e<exp>".
    let sci = format!("{:e}", f.abs());
    let (mant, exp_str) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let digits: String = mant.chars().filter(|&c| c != '.').collect();
    let exp: i64 = exp_str.parse().unwrap_or(0);
    let k = digits.len() as i64; // number of significant digits
    let n = exp + 1; // position of the decimal point relative to the digits
    let body = if k <= n && n <= 21 {
        // Integral: digits followed by n-k zeros.
        let mut s = digits;
        for _ in 0..(n - k) {
            s.push('0');
        }
        s
    } else if 0 < n && n <= 21 {
        // Decimal point inside the digit string.
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        // Leading "0." plus -n zeros.
        let mut s = String::from("0.");
        for _ in 0..(-n) {
            s.push('0');
        }
        s.push_str(&digits);
        s
    } else {
        // Exponential notation, exponent always signed.
        let e = n - 1;
        let mant_out = if k == 1 {
            digits
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{}e{}{}", mant_out, if e >= 0 { "+" } else { "-" }, e.abs())
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// `JSON.stringify(v)` for a JSON value.
///
/// serde_json cannot be used directly for this: it formats floats with ryu,
/// whose decimal/exponential switchover is NOT the one ECMA-262 specifies.
/// The two disagree over the ranges `[1e-6, 1e-5)` and `[1e20, 1e21)`, e.g.
///   6.155428829675274e-6 -> Node "0.000006155428829675274", ryu "6.155428829675274e-6"
///   1e20                 -> Node "100000000000000000000",   ryu "1e+20"
/// Numbers therefore go through [`js_f64_to_string`], which implements the
/// spec's `Number::toString`. Everything else (string escaping, key order,
/// separators) already matches, so it is delegated to serde_json.
pub fn to_js_json(v: &Value) -> String {
    let mut out = String::new();
    write_js_json(v, &mut out);
    out
}

fn write_js_json(v: &Value, out: &mut String) {
    match v {
        // Integral values are already exact as JSON integers; only the f64
        // arm can hit the ryu/ECMA divergence.
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => out.push_str(&js_f64_to_string(f)),
            _ => out.push_str(&n.to_string()),
        },
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_js_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // Key escaping is identical between the two, so reuse it.
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_js_json(val, out);
            }
            out.push('}');
        }
        // null / bool / string: serde_json already matches JSON.stringify.
        other => out.push_str(&other.to_string()),
    }
}

/// JS `String(v)` for a JSON value: numbers via [`js_f64_to_string`], arrays
/// join their elements with ',' (null elements become '', per
/// `Array.prototype.toString`), plain objects become "[object Object]",
/// top-level `null` prints "null".
pub fn js_display(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_f64_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|e| if e.is_null() { String::new() } else { js_display(e) })
                .collect();
            parts.join(",")
        }
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// `Number(v)` coercion for a JSON value: numbers pass through, strings parse
/// with full JS rules (trimmed, '' -> 0, 0x/0o/0b, Infinity), `true`/`false`
/// -> 1/0, `null` -> 0, arrays coerce via their string form (`[]` -> 0,
/// `[5]` -> 5, `[1,2]` -> NaN), objects -> NaN.
pub fn js_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => parse_js_number_str(s),
        Value::Array(_) => parse_js_number_str(&js_display(v)),
        Value::Object(_) => f64::NAN,
    }
}

/// `Math.round(f)` — round to nearest, ties toward +infinity. NOT
/// `f64::round`, which is half-away-from-zero and differs on negative halves
/// (JS `Math.round(-2.5)` is -2; Rust `(-2.5f64).round()` is -3). Also NOT
/// `floor(f + 0.5)`, which is wrong for e.g. 0.49999999999999994 (the addition
/// rounds up to exactly 1.0; JS returns 0).
///
/// This is the faithful form: JS `Math.round` yields a Number (f64), so any
/// call site whose input can exceed i64 range MUST use this rather than
/// [`js_round`]. Rounding cents (`Math.round(x*100)/100`) is exactly such a
/// site — a cost sum of 1e20 makes `x*100` 1e22, far past i64::MAX.
pub fn js_round_f64(f: f64) -> f64 {
    if f.is_nan() {
        return 0.0;
    }
    // Values this large are already integral, and floor/compare on them is a
    // no-op; return early so infinities pass through as JS leaves them.
    if !f.is_finite() {
        return f;
    }
    let floor = f.floor();
    // f - floor(f) is exact (Sterbenz), so the tie comparison is reliable.
    if f - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

/// The i64 form, for call sites whose magnitude is structurally bounded
/// (credit-second sums, token counts). NaN maps to 0; out-of-i64-range input
/// saturates, so do NOT use this where the input can be large — see
/// [`js_round_f64`].
pub fn js_round(f: f64) -> i64 {
    js_round_f64(f) as i64
}

/// `nonnegativeNumber(value, {integer})` from src/db.js:
/// `Number(value || 0)`; non-finite or negative -> 0; `integer: true` applies
/// `Math.round` ([`js_round`]).
pub fn nonneg(v: &Value, integer: bool) -> f64 {
    let n = if js_truthy(v) { js_number(v) } else { 0.0 };
    if !n.is_finite() || n < 0.0 {
        return 0.0;
    }
    if integer {
        js_round(n) as f64
    } else {
        n
    }
}

/// `String(v || default)`: 0, '', null, false (and absent/undefined) are falsy
/// and yield `default`; anything else is stringified the way JS would
/// ([`js_display`]) — note `String([] || 'x')` is `''` because an empty array
/// is truthy but stringifies to the empty string.
pub fn js_string_or(v: Option<&Value>, default: &str) -> String {
    match v {
        Some(val) if js_truthy(val) => js_display(val),
        _ => default.to_string(),
    }
}

/// `numberParam(url, name, fallback, {min, max})` from src/server.js:
/// missing param -> default; present param -> `Number(raw)` (JS string rules,
/// so `?days=` is 0, not the default); non-finite result -> default; then
/// clamped, with each bound applied ONLY when given (JS defaults them to
/// ±Infinity). The clamp applies to the default too, exactly like the JS
/// `Math.min(max, Math.max(min, value))`.
///
/// The JS `integer: true` variant (`Math.floor`) is used by exactly one call
/// site (/api/recent's `limit`); the server module applies `.floor()` itself.
pub fn number_param(raw: Option<&str>, default: f64, min: Option<f64>, max: Option<f64>) -> f64 {
    let mut value = match raw {
        None => default,
        Some(s) => parse_js_number_str(s),
    };
    if !value.is_finite() {
        value = default;
    }
    if let Some(mn) = min {
        value = value.max(mn);
    }
    if let Some(mx) = max {
        value = value.min(mx);
    }
    value
}

/// Convert an f64 into a `serde_json::Number` the way `JSON.stringify` would
/// print it: integral values become JSON integers (70, not 70.0).
///
/// Non-integral values still need [`to_js_json`] to reach the wire unchanged:
/// serde_json would format them with ryu, whose decimal/exponential switchover
/// is not the one ECMA-262 specifies. This only normalises the integral case.
///
/// Integer emission is limited to |x| <= 2^53: above that, JS prints the
/// shortest-roundtrip digits zero-padded (e.g. `2**63` prints
/// "9223372036854776000"), which an exact i64/u64 would NOT match — so such
/// values go through the float path instead. Non-finite input (JS would emit
/// `null`; no call site can produce it) maps to 0.
pub fn json_num(f: f64) -> serde_json::Number {
    const MAX_SAFE: f64 = 9007199254740992.0; // 2^53
    if f.is_finite() && f == f.trunc() && f.abs() <= MAX_SAFE {
        serde_json::Number::from(f as i64)
    } else {
        serde_json::Number::from_f64(f).unwrap_or_else(|| serde_json::Number::from(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn num_str(s: &str) -> f64 {
        js_number(&Value::String(s.to_string()))
    }

    /// Golden: `for (const s of strs) console.log(JSON.stringify(s), Number(s))`
    /// captured under Node v22.22.3.
    #[test]
    fn golden_js_number_strings() {
        assert_eq!(num_str(""), 0.0);
        assert_eq!(num_str("  "), 0.0);
        assert_eq!(num_str("42"), 42.0);
        assert_eq!(num_str(" 42 "), 42.0);
        assert_eq!(num_str("-3.5"), -3.5);
        assert_eq!(num_str("+3.5"), 3.5);
        assert_eq!(num_str("3."), 3.0);
        assert_eq!(num_str(".5"), 0.5);
        assert_eq!(num_str("1e3"), 1000.0);
        assert_eq!(num_str("1E3"), 1000.0);
        assert_eq!(num_str("1.5e-3"), 0.0015);
        assert_eq!(num_str("1.e3"), 1000.0);
        assert_eq!(num_str("0x1f"), 31.0);
        assert_eq!(num_str("0X1F"), 31.0);
        assert_eq!(num_str("0xff"), 255.0);
        assert_eq!(num_str("0b101"), 5.0);
        assert_eq!(num_str("0o17"), 15.0);
        assert_eq!(num_str("Infinity"), f64::INFINITY);
        assert_eq!(num_str("+Infinity"), f64::INFINITY);
        assert_eq!(num_str("-Infinity"), f64::NEG_INFINITY);
        assert_eq!(num_str("1e309"), f64::INFINITY);
        assert_eq!(num_str("-1e309"), f64::NEG_INFINITY);
        assert_eq!(num_str("5e-324"), 5e-324);
        assert_eq!(num_str("1e21"), 1e21);
        assert_eq!(num_str("8.64e12"), 8_640_000_000_000.0);
        assert_eq!(num_str("0.1"), 0.1);
        assert_eq!(num_str("1700000000000"), 1_700_000_000_000.0);
        // Node: Number("0xDEADBEEFCAFEBABE12345") === 1.682512646976632e+25
        assert_eq!(num_str("0xDEADBEEFCAFEBABE12345"), 1.682512646976632e25);
        // -0 keeps its sign.
        assert_eq!(num_str("-0"), 0.0);
        assert!(num_str("-0").is_sign_negative());
        // JS whitespace trimming: BOM and line terminators are trimmed.
        assert_eq!(num_str("\u{FEFF}8\u{FEFF}"), 8.0);
        assert_eq!(num_str("\n\t 9 \r\n"), 9.0);
        // NaN cases.
        for bad in [
            "-0x1f", "infinity", "inf", "nan", "NaN", "1_000", "12abc", "1.2.3", "--5", "1e", "1e+", "0x",
            "abc", ".e3", "+0x10", "-",
        ] {
            assert!(num_str(bad).is_nan(), "expected NaN for {bad:?}");
        }
    }

    /// Golden output captured from real Node:
    ///   node -e 'console.log(JSON.stringify({v:[...]}))'
    /// The 1e-6..1e-5 and 1e20..1e21 entries are the ones serde_json's ryu
    /// formatting gets wrong; the rest guard against regressing the cases it
    /// happens to agree on.
    #[test]
    fn to_js_json_matches_node_json_stringify_for_floats() {
        let v = json!({ "v": [
            6.155428829675274e-6f64, 1e-6, 9.9e-7, 1e-7, 1e20,
            1.2345678901234568e20, 1e21, 0.1, 1.0 / 3.0,
            -6.155428829675274e-6f64, 5e-324, 1e308, 0.0000123, 1e-5,
        ]});
        assert_eq!(
            to_js_json(&v),
            "{\"v\":[0.000006155428829675274,0.000001,9.9e-7,1e-7,\
             100000000000000000000,123456789012345680000,1e+21,0.1,\
             0.3333333333333333,-0.000006155428829675274,5e-324,1e+308,\
             0.0000123,0.00001]}"
        );
    }

    /// Non-number shapes must still round-trip exactly as serde_json emits
    /// them: key order (preserve_order), escaping, and separators.
    #[test]
    fn to_js_json_leaves_non_numeric_shapes_untouched() {
        let v = json!({
            "z": "quote\" back\\ nl\n tab\t ctrl\u{1}",
            "a": [null, true, false, {}, []],
            "unicode": "héllo → 世界",
            "int": 70,
            "neg": -3,
        });
        assert_eq!(to_js_json(&v), serde_json::to_string(&v).expect("serialize"));
        // and key insertion order is preserved, not sorted
        assert!(to_js_json(&v).starts_with("{\"z\":"));
    }

    /// Golden: Number([]) = 0, Number([5]) = 5, Number([1,2]) = NaN,
    /// Number({}) = NaN, Number(true) = 1, Number(false) = 0, Number(null) = 0.
    #[test]
    fn golden_js_number_values() {
        assert_eq!(js_number(&json!(null)), 0.0);
        assert_eq!(js_number(&json!(true)), 1.0);
        assert_eq!(js_number(&json!(false)), 0.0);
        assert_eq!(js_number(&json!([])), 0.0);
        assert_eq!(js_number(&json!([5])), 5.0);
        assert_eq!(js_number(&json!([[5]])), 5.0);
        assert!(js_number(&json!([1, 2])).is_nan());
        assert!(js_number(&json!({})).is_nan());
        assert!(js_number(&json!({"a": 1})).is_nan());
        assert_eq!(js_number(&json!(70)), 70.0);
        assert_eq!(js_number(&json!(70.5)), 70.5);
        assert_eq!(js_number(&json!(-3)), -3.0);
    }

    /// Cost sums can exceed i64 range, where the old `js_round -> i64` path
    /// saturated and pinned every large total to i64::MAX/100
    /// (92233720368547760). Golden values from real Node:
    ///   node -e 'const r2=x=>Math.round(x*100)/100; console.log(r2(1e20))'
    #[test]
    fn js_round_f64_does_not_saturate_on_large_cost_sums() {
        let round2 = |x: f64| js_round_f64(x * 100.0) / 100.0;
        assert_eq!(round2(1e20), 1e20);
        assert_eq!(round2(9.5e20), 9.5e20);
        assert_eq!(round2(3.860653024027428e22), 3.860653024027428e22);
        assert_eq!(round2(1e18), 1e18);
        // and the ordinary cases, including JS's ties-toward-+inf on negatives
        assert_eq!(round2(0.615), 0.62);
        assert_eq!(round2(-0.615), -0.61);
        // the saturating i64 form is what used to corrupt these
        assert_eq!(js_round(1e20 * 100.0), i64::MAX);
        assert_eq!(js_round_f64(1e20 * 100.0), 1e22);
    }

    /// Golden: `Math.round` under Node v22 — ties toward +infinity, and the
    /// famous 0.49999999999999994 case that floor(x+0.5) gets wrong.
    #[test]
    fn golden_js_round() {
        assert_eq!(js_round(0.5), 1);
        assert_eq!(js_round(1.5), 2);
        assert_eq!(js_round(2.5), 3);
        assert_eq!(js_round(-0.5), 0);
        assert_eq!(js_round(-1.5), -1);
        assert_eq!(js_round(-2.5), -2);
        assert_eq!(js_round(-2.4), -2);
        assert_eq!(js_round(-2.6), -3);
        assert_eq!(js_round(2.4), 2);
        assert_eq!(js_round(2.6), 3);
        assert_eq!(js_round(0.0), 0);
        assert_eq!(js_round(-0.3), 0);
        assert_eq!(js_round(0.49999999999999994), 0);
        assert_eq!(js_round(-0.49999999999999994), 0);
        assert_eq!(js_round(1e15 + 0.5), 1_000_000_000_000_001);
        assert_eq!(js_round(4503599627370495.5), 4_503_599_627_370_496);
        // Divergence from f64::round on negative halves, as documented.
        assert_eq!((-2.5f64).round(), -3.0);
    }

    /// Golden: nonnegativeNumber(v) / nonnegativeNumber(v, {integer:true})
    /// from src/db.js, run under Node v22.
    #[test]
    fn golden_nonneg() {
        // (value, plain, integer)
        let cases: Vec<(Value, f64, f64)> = vec![
            (json!(null), 0.0, 0.0),
            (json!(false), 0.0, 0.0),
            (json!(true), 1.0, 1.0),
            (json!(0), 0.0, 0.0),
            (json!(""), 0.0, 0.0),
            (json!("abc"), 0.0, 0.0),
            (json!("5.7"), 5.7, 6.0),
            (json!(5.7), 5.7, 6.0),
            (json!(-3), 0.0, 0.0),
            (json!(-0.0001), 0.0, 0.0),
            (json!(2.5), 2.5, 3.0),
            (json!("2.5"), 2.5, 3.0),
            (json!("Infinity"), 0.0, 0.0),
            (json!("1e3"), 1000.0, 1000.0),
            (json!([]), 0.0, 0.0),
            (json!([7]), 7.0, 7.0),
            (json!({}), 0.0, 0.0),
        ];
        for (v, plain, int) in cases {
            assert_eq!(nonneg(&v, false), plain, "nonneg({v}, false)");
            assert_eq!(nonneg(&v, true), int, "nonneg({v}, true)");
        }
    }

    /// Golden: `String(v || 'unknown')` under Node v22.
    #[test]
    fn golden_js_string_or() {
        assert_eq!(js_string_or(None, "unknown"), "unknown");
        assert_eq!(js_string_or(Some(&json!(null)), "unknown"), "unknown");
        assert_eq!(js_string_or(Some(&json!(0)), "unknown"), "unknown");
        assert_eq!(js_string_or(Some(&json!(-0.0)), "unknown"), "unknown");
        assert_eq!(js_string_or(Some(&json!("")), "unknown"), "unknown");
        assert_eq!(js_string_or(Some(&json!(false)), "unknown"), "unknown");
        assert_eq!(js_string_or(Some(&json!("x")), "unknown"), "x");
        assert_eq!(js_string_or(Some(&json!("0")), "unknown"), "0");
        assert_eq!(js_string_or(Some(&json!(5)), "unknown"), "5");
        assert_eq!(js_string_or(Some(&json!(5.5)), "unknown"), "5.5");
        assert_eq!(js_string_or(Some(&json!(true)), "unknown"), "true");
        // Empty array is TRUTHY but stringifies to '' — String([] || 'x') === ''.
        assert_eq!(js_string_or(Some(&json!([])), "unknown"), "");
        assert_eq!(js_string_or(Some(&json!([1, 2])), "unknown"), "1,2");
        assert_eq!(js_string_or(Some(&json!({"a": 1})), "unknown"), "[object Object]");
        assert_eq!(js_string_or(Some(&json!("coding")), "coding"), "coding");
    }

    /// Golden: JS Number-to-string formatting (`String(x)`) under Node v22,
    /// including the >2^53 zero-padding of shortest digits and the fixed/
    /// exponential thresholds at 1e21 and 1e-6.
    #[test]
    fn golden_js_f64_to_string() {
        assert_eq!(js_f64_to_string(70.0), "70");
        assert_eq!(js_f64_to_string(70.5), "70.5");
        assert_eq!(js_f64_to_string(0.1), "0.1");
        assert_eq!(js_f64_to_string(2.55), "2.55");
        assert_eq!(js_f64_to_string(-0.0), "0");
        assert_eq!(js_f64_to_string(0.0), "0");
        assert_eq!(js_f64_to_string(-3.5), "-3.5");
        assert_eq!(js_f64_to_string(8.64e12), "8640000000000");
        assert_eq!(js_f64_to_string(1e-6), "0.000001");
        assert_eq!(js_f64_to_string(1e-7), "1e-7");
        assert_eq!(js_f64_to_string(1.5e-7), "1.5e-7");
        assert_eq!(js_f64_to_string(1e20), "100000000000000000000");
        assert_eq!(js_f64_to_string(1e21), "1e+21");
        assert_eq!(js_f64_to_string(-1.5e21), "-1.5e+21");
        // Shortest digits zero-padded, NOT the exact binary value (...683968).
        assert_eq!(js_f64_to_string(123456789012345680000.0), "123456789012345680000");
        assert_eq!(js_f64_to_string(999999999999999900000.0), "999999999999999900000");
        assert_eq!(js_f64_to_string(f64::NAN), "NaN");
        assert_eq!(js_f64_to_string(f64::INFINITY), "Infinity");
        assert_eq!(js_f64_to_string(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(js_f64_to_string(5e-324), "5e-324");
        assert_eq!(js_f64_to_string(0.30000000000000004), "0.30000000000000004");
    }

    /// Golden: `String([1,[2,3],null,'x',{a:1},true])` under Node v22.
    #[test]
    fn golden_js_display_array() {
        assert_eq!(
            js_display(&json!([1, [2, 3], null, "x", {"a": 1}, true])),
            "1,2,3,,x,[object Object],true"
        );
        assert_eq!(js_display(&json!(null)), "null");
    }

    /// Golden: numberParam from src/server.js under Node v22, at the real call
    /// sites' bounds.
    #[test]
    fn golden_number_param() {
        let d = |raw: Option<&str>| number_param(raw, 1.0, Some(1.0), Some(366.0));
        assert_eq!(d(None), 1.0);
        // `?days=` — empty string is 0 in JS, then clamped up to min, NOT the default.
        assert_eq!(d(Some("")), 1.0);
        assert_eq!(d(Some("0.5")), 1.0);
        assert_eq!(d(Some("400")), 366.0);
        assert_eq!(d(Some("2.5")), 2.5); // fractional days allowed
        assert_eq!(number_param(Some("abc"), 7.0, Some(1.0), Some(366.0)), 7.0);
        // tz clamp is symmetric and applies to negatives.
        assert_eq!(
            number_param(Some("-100"), 0.0, Some(-1440.0), Some(1440.0)),
            -100.0
        );
        assert_eq!(
            number_param(Some("-99999"), 0.0, Some(-1440.0), Some(1440.0)),
            -1440.0
        );
        // No bounds -> no clamping at all.
        assert_eq!(number_param(None, 1234.5, None, None), 1234.5);
        assert_eq!(number_param(Some("-500"), 0.0, None, None), -500.0);
        // "Infinity" parses but is non-finite -> default.
        assert_eq!(number_param(Some("Infinity"), 3.0, Some(1.0), Some(10.0)), 3.0);
        // /api/recent limit (server floors separately): 49.9 clamps to 49.9 here.
        assert_eq!(number_param(Some("49.9"), 50.0, Some(1.0), Some(500.0)), 49.9);
        // /api/timeline fractional min bound.
        assert_eq!(
            number_param(Some("0.001"), 24.0, Some(1.0 / 60.0), Some(336.0)),
            1.0 / 60.0
        );
    }

    /// Golden: `JSON.stringify([70, 70.5, -0, 2.55, 0.30000000000000004])`
    /// === "[70,70.5,0,2.55,0.30000000000000004]" under Node v22.
    #[test]
    fn golden_json_num() {
        assert_eq!(json_num(70.0).to_string(), "70");
        assert_eq!(json_num(70.5).to_string(), "70.5");
        assert_eq!(json_num(-0.0).to_string(), "0");
        assert_eq!(json_num(2.55).to_string(), "2.55");
        assert_eq!(json_num(0.30000000000000004).to_string(), "0.30000000000000004");
        assert_eq!(json_num(-42.0).to_string(), "-42");
        assert_eq!(json_num(9007199254740992.0).to_string(), "9007199254740992");
        // 2dp cost rounding path: Math.round(x*100)/100 then serialization.
        // Node: Math.round(1.005*100)/100 === 1 (1.005*100 is 100.49999999999999)
        // and Math.round(1.006*100)/100 === 1.01 — both reproduced exactly.
        assert_eq!(json_num(js_round(1.005 * 100.0) as f64 / 100.0).to_string(), "1");
        assert_eq!(
            json_num(js_round(1.006 * 100.0) as f64 / 100.0).to_string(),
            "1.01"
        );
        // Non-finite guard (unreachable through real call sites).
        assert_eq!(json_num(f64::NAN).to_string(), "0");
        // Embeds as a bare integer in a serialized Value, like JS emits 70 not 70.0.
        let v = json!({ "seconds": json_num(70.0) });
        assert_eq!(v.to_string(), r#"{"seconds":70}"#);
    }

    #[test]
    fn truthiness() {
        assert!(!js_truthy(&json!(null)));
        assert!(!js_truthy(&json!(false)));
        assert!(!js_truthy(&json!(0)));
        assert!(!js_truthy(&json!(0.0)));
        assert!(!js_truthy(&json!(-0.0)));
        assert!(!js_truthy(&json!("")));
        assert!(js_truthy(&json!(true)));
        assert!(js_truthy(&json!(1)));
        assert!(js_truthy(&json!(-1)));
        assert!(js_truthy(&json!("0")));
        assert!(js_truthy(&json!([])));
        assert!(js_truthy(&json!({})));
    }
}
