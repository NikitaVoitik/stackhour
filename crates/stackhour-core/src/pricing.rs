//! Model pricing: the built-in table, longest-substring model matching, and
//! per-1M-token USD cost arithmetic.
//!
//! Quirk kept: a user `pricing` section in config.json REPLACES the whole
//! built-in table (deep-merge treats it atomically because the user object
//! replaces per-key, and lookup falls back user-'default' -> built-in default).

use std::sync::OnceLock;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Per-1M-token USD prices for one model (substring) key.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    /// Input tokens, USD per 1M. (JSON key: "in".)
    #[serde(rename = "in")]
    pub in_: f64,
    /// Output tokens, USD per 1M.
    pub out: f64,
    /// Cache-read tokens, USD per 1M. (JSON key: "cacheRead".)
    #[serde(rename = "cacheRead", default)]
    pub cache_read: f64,
    /// Cache-write tokens, USD per 1M. (JSON key: "cacheWrite".)
    #[serde(rename = "cacheWrite", default)]
    pub cache_write: f64,
}

/// Insertion-ordered pricing table: model-substring -> price. The key
/// `"default"` is the fallback entry.
pub type PricingTable = IndexMap<String, Price>;

/// Token counts used for a cost computation.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// The built-in `default` entry (`DEFAULT_PRICING.default` in src/pricing.js).
/// Ultimate fallback when neither the user table nor a substring key matches.
const BUILT_IN_DEFAULT: Price = Price {
    in_: 3.0,
    out: 15.0,
    cache_read: 0.3,
    cache_write: 3.75,
};

/// The built-in pricing table (exact values from src/pricing.js).
pub fn default_pricing() -> &'static PricingTable {
    static TABLE: OnceLock<PricingTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = PricingTable::new();
        t.insert(
            "claude-fable".to_string(),
            Price {
                in_: 10.0,
                out: 50.0,
                cache_read: 1.0,
                cache_write: 12.5,
            },
        );
        t.insert(
            "claude-opus".to_string(),
            Price {
                in_: 5.0,
                out: 25.0,
                cache_read: 0.5,
                cache_write: 6.25,
            },
        );
        t.insert(
            "claude-sonnet".to_string(),
            Price {
                in_: 3.0,
                out: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
        );
        t.insert(
            "claude-haiku".to_string(),
            Price {
                in_: 1.0,
                out: 5.0,
                cache_read: 0.1,
                cache_write: 1.25,
            },
        );
        t.insert(
            "gpt-5".to_string(),
            Price {
                in_: 1.25,
                out: 10.0,
                cache_read: 0.125,
                cache_write: 0.0,
            },
        );
        t.insert("default".to_string(), BUILT_IN_DEFAULT);
        t
    })
}

/// Longest-substring key match over the lowercased model id, excluding the
/// literal key `"default"`. Fallback chain: user-table `default` -> built-in
/// default.
///
/// `table = None` means "use the built-in table" (JS default parameter).
/// A supplied table REPLACES the built-in one entirely: built-in model keys
/// are never consulted, only the built-in `default` entry as last resort.
pub fn price_for(model: &str, table: Option<&PricingTable>) -> Price {
    let id = model.to_lowercase();
    let pricing = table.unwrap_or_else(|| default_pricing());
    // JS: let best = pricing.default || DEFAULT_PRICING.default;
    let mut best = pricing.get("default").copied().unwrap_or(BUILT_IN_DEFAULT);
    let mut best_len = 0usize;
    for (key, p) in pricing.iter() {
        if key != "default" && id.contains(key.as_str()) && key.len() > best_len {
            best = *p;
            best_len = key.len();
        }
    }
    best
}

/// USD cost of `usage` at the matched price (per-1M-token arithmetic).
///
/// Mirrors JS `(usage.input || 0)` etc.: NaN token counts are treated as 0
/// (NaN is falsy in JS).
pub fn cost_of(model: &str, usage: &Usage, table: Option<&PricingTable>) -> f64 {
    let p = price_for(model, table);
    (orz(usage.input) * p.in_
        + orz(usage.cache_read) * p.cache_read
        + orz(usage.cache_write) * p.cache_write
        + orz(usage.output) * p.out)
        / 1e6
}

/// JS `v || 0` for numbers: NaN (falsy) becomes 0; everything else passes
/// through (0 and -0 map to 0, numerically identical anyway).
fn orz(v: f64) -> f64 {
    if v.is_nan() {
        0.0
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_table_has_exact_js_values() {
        let t = default_pricing();
        assert_eq!(t.len(), 6);
        // Insertion order matters for equal-length-key ties.
        let keys: Vec<&str> = t.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "claude-fable",
                "claude-opus",
                "claude-sonnet",
                "claude-haiku",
                "gpt-5",
                "default"
            ]
        );
        let fable = t["claude-fable"];
        assert_eq!(fable.in_, 10.0);
        assert_eq!(fable.out, 50.0);
        assert_eq!(fable.cache_read, 1.0);
        assert_eq!(fable.cache_write, 12.5);
        let gpt = t["gpt-5"];
        assert_eq!(gpt.in_, 1.25);
        assert_eq!(gpt.out, 10.0);
        assert_eq!(gpt.cache_read, 0.125);
        assert_eq!(gpt.cache_write, 0.0);
        assert_eq!(t["default"], BUILT_IN_DEFAULT);
    }

    #[test]
    fn substring_match_is_case_insensitive() {
        let p = price_for("Claude-Fable-5-20260101", None);
        assert_eq!(p.in_, 10.0);
        assert_eq!(p.out, 50.0);
    }

    #[test]
    fn unknown_model_gets_default() {
        let p = price_for("gemini-2.5-pro", None);
        assert_eq!(p, BUILT_IN_DEFAULT);
    }

    #[test]
    fn empty_model_gets_default() {
        let p = price_for("", None);
        assert_eq!(p, BUILT_IN_DEFAULT);
    }

    #[test]
    fn longest_key_wins() {
        // "claude-opus-4-1" contains "claude-opus"; add a longer overlapping
        // key to a custom table and check it wins regardless of order.
        let mut t = PricingTable::new();
        t.insert(
            "claude".to_string(),
            Price {
                in_: 1.0,
                out: 1.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
        t.insert(
            "claude-opus-4".to_string(),
            Price {
                in_: 99.0,
                out: 99.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
        t.insert(
            "claude-opus".to_string(),
            Price {
                in_: 2.0,
                out: 2.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
        let p = price_for("claude-opus-4-1-20250805", Some(&t));
        assert_eq!(p.in_, 99.0);
    }

    #[test]
    fn equal_length_tie_keeps_first_inserted() {
        // JS uses strict `key.length > bestLen`, so the FIRST equal-length
        // matching key (Object.entries order) wins.
        let mut t = PricingTable::new();
        t.insert(
            "ab".to_string(),
            Price {
                in_: 1.0,
                out: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
        t.insert(
            "bc".to_string(),
            Price {
                in_: 2.0,
                out: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
        let p = price_for("abc", Some(&t));
        assert_eq!(p.in_, 1.0);
    }

    #[test]
    fn default_key_is_excluded_from_substring_match() {
        // A model id literally containing "default" must not match the
        // default entry as a substring key; it falls back to it anyway,
        // but a shorter real key must still win.
        let mut t = PricingTable::new();
        t.insert(
            "default".to_string(),
            Price {
                in_: 7.0,
                out: 7.0,
                cache_read: 7.0,
                cache_write: 7.0,
            },
        );
        t.insert(
            "def".to_string(),
            Price {
                in_: 1.0,
                out: 1.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
        let p = price_for("my-default-model", Some(&t));
        // "def" (len 3) matches; "default" (len 7) is skipped despite being
        // longer and a substring of the id.
        assert_eq!(p.in_, 1.0);
    }

    #[test]
    fn user_table_replaces_whole_table() {
        // A user table without any matching key and WITHOUT a 'default'
        // entry falls back to the BUILT-IN default, not to built-in model
        // keys (whole-table replacement quirk).
        let mut t = PricingTable::new();
        t.insert(
            "gpt-5".to_string(),
            Price {
                in_: 42.0,
                out: 42.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
        // "claude-fable" is in the built-in table but not the user table:
        let p = price_for("claude-fable-5", Some(&t));
        assert_eq!(p, BUILT_IN_DEFAULT);
        // and the user key applies:
        let p = price_for("gpt-5-codex", Some(&t));
        assert_eq!(p.in_, 42.0);
    }

    #[test]
    fn user_default_beats_built_in_default() {
        let mut t = PricingTable::new();
        t.insert(
            "default".to_string(),
            Price {
                in_: 9.0,
                out: 9.0,
                cache_read: 9.0,
                cache_write: 9.0,
            },
        );
        let p = price_for("whatever", Some(&t));
        assert_eq!(p.in_, 9.0);
        assert_eq!(p.cache_write, 9.0);
    }

    #[test]
    fn cost_arithmetic_per_million() {
        // claude-fable: in 10, out 50, cacheRead 1, cacheWrite 12.5
        let usage = Usage {
            input: 1_000_000.0,
            output: 1_000_000.0,
            cache_read: 1_000_000.0,
            cache_write: 1_000_000.0,
        };
        let c = cost_of("claude-fable-5", &usage, None);
        assert!((c - (10.0 + 50.0 + 1.0 + 12.5)).abs() < 1e-9);
    }

    #[test]
    fn cost_matches_js_reference_example() {
        // 1000 in, 2000 out, 500 cacheRead, 100 cacheWrite on claude-sonnet:
        // (1000*3 + 500*0.3 + 100*3.75 + 2000*15) / 1e6 = 0.033525
        let usage = Usage {
            input: 1000.0,
            output: 2000.0,
            cache_read: 500.0,
            cache_write: 100.0,
        };
        let c = cost_of("claude-sonnet-4-5", &usage, None);
        assert!((c - 0.033525).abs() < 1e-12);
    }

    #[test]
    fn cost_of_zero_usage_is_zero() {
        let c = cost_of("claude-opus", &Usage::default(), None);
        assert_eq!(c, 0.0);
    }

    #[test]
    fn cost_of_treats_nan_as_zero() {
        // JS: (usage.input || 0) — NaN is falsy.
        let usage = Usage {
            input: f64::NAN,
            output: 2000.0,
            cache_read: f64::NAN,
            cache_write: f64::NAN,
        };
        let c = cost_of("claude-sonnet", &usage, None);
        assert!((c - (2000.0 * 15.0) / 1e6).abs() < 1e-12);
    }

    #[test]
    fn price_serde_uses_js_key_names() {
        let p: Price = serde_json::from_str(r#"{"in":1.25,"out":10,"cacheRead":0.125,"cacheWrite":0}"#)
            .expect("price json");
        assert_eq!(p.in_, 1.25);
        assert_eq!(p.out, 10.0);
        assert_eq!(p.cache_read, 0.125);
        assert_eq!(p.cache_write, 0.0);

        // cache fields default to 0 when omitted (user configs may skip them)
        let p: Price = serde_json::from_str(r#"{"in":2,"out":4}"#).expect("partial price json");
        assert_eq!(p.cache_read, 0.0);
        assert_eq!(p.cache_write, 0.0);

        let out = serde_json::to_value(p).expect("serialize");
        assert_eq!(out["in"], 2.0);
        assert_eq!(out["cacheRead"], 0.0);
        assert_eq!(out["cacheWrite"], 0.0);
    }

    #[test]
    fn pricing_table_deserializes_preserving_order() {
        let json = r#"{
            "zzz-model": {"in":1,"out":2},
            "aaa-model": {"in":3,"out":4},
            "default": {"in":5,"out":6}
        }"#;
        let t: PricingTable = serde_json::from_str(json).expect("table json");
        let keys: Vec<&str> = t.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, ["zzz-model", "aaa-model", "default"]);
    }
}
