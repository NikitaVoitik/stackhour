//! `stackhour status` — split from main (line budget).
//!
//! Unauthenticated GET `/api/summary?days=1&groupBy=project,source` with a
//! 5s timeout; on failure prints `server unreachable at <url>: <msg>` and
//! exits 1 IMMEDIATELY (unlike other verbs' deferred exit code). `h()`
//! duration formatting ('XhYm' i.e. `${floor(s/3600)}h ${round((s%3600)/60)}m`),
//! padEnd(9) columns, top 15 rows.

use serde_json::Value;
use stackhour_core::config::Config;
use stackhour_core::jsnum::{js_number, js_round_f64};
use std::io::Write;
use std::time::Duration;

/// The JS `h()` helper: `${Math.floor(sec/3600)}h ${Math.round((sec%3600)/60)}m`.
/// Note the minutes ROUND, so 59m30s reads as "0h 60m" rather than "1h 0m" —
/// a Node quirk we reproduce rather than fix.
fn h(sec: f64) -> String {
    let hours = (sec / 3600.0).floor();
    let minutes = js_round_f64((sec % 3600.0) / 60.0);
    format!("{}h {}m", js_display_num(hours), js_display_num(minutes))
}

/// Template-literal number formatting (integral f64s render without `.0`).
fn js_display_num(f: f64) -> String {
    stackhour_core::jsnum::to_js_json(&Value::from(f))
}

/// `String.prototype.padEnd(width)` — pads by UTF-16 code units.
fn pad_end(s: &str, width: usize) -> String {
    let len = s.chars().map(char::len_utf16).sum::<usize>();
    if len >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - len))
    }
}

/// Render the `status` report from an already-fetched `/api/summary` body.
/// Split out so the formatting contract is testable without a live server.
pub fn render_status(summary: &Value, out: &mut dyn Write) -> std::io::Result<()> {
    let total = js_number(summary.get("total").unwrap_or(&Value::Null));
    writeln!(out, "today: {} total", h(total))?;
    let empty = Vec::new();
    let totals = summary
        .get("totals")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for t in totals.iter().take(15) {
        let seconds = js_number(t.get("seconds").unwrap_or(&Value::Null));
        let project = js_str(t.get("project"));
        let source = js_str(t.get("source"));
        writeln!(out, "  {} {project} ({source})", pad_end(&h(seconds), 9))?;
    }
    Ok(())
}

/// Template-literal string coercion for the values we interpolate.
fn js_str(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => stackhour_core::jsnum::js_display(other),
    }
}

/// Run the status verb; returns the exit code (may also exit directly on
/// unreachable-server, parity permitting).
pub fn run_status(cfg: &Config) -> i32 {
    let base = cfg.agent.server_url.clone();
    let url = format!("{base}/api/summary?days=1&groupBy=project,source");
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("server unreachable at {base}: {e}");
            return 1;
        }
    };
    let summary: Value = match client.get(&url).send().and_then(reqwest::blocking::Response::json)
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("server unreachable at {base}: {e}");
            return 1;
        }
    };
    let mut out = std::io::stdout();
    if render_status(&summary, &mut out).is_err() {
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rendered(summary: &Value) -> String {
        let mut out = Vec::new();
        render_status(summary, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn renders_the_node_status_report() {
        let summary = json!({
            "total": 7325,
            "totals": [
                { "project": "stackhour", "source": "files", "seconds": 3600 },
                { "project": "notes", "source": "zed", "seconds": 90 },
            ],
        });
        assert_eq!(
            rendered(&summary),
            "today: 2h 2m total\n  1h 0m     stackhour (files)\n  0h 2m     notes (zed)\n"
        );
    }

    /// Only the first 15 rows are printed, in server order.
    #[test]
    fn caps_the_table_at_fifteen_rows() {
        let totals: Vec<Value> = (0..20)
            .map(|i| json!({ "project": format!("p{i}"), "source": "files", "seconds": 60 }))
            .collect();
        let out = rendered(&json!({ "total": 1200, "totals": totals }));
        assert_eq!(out.lines().count(), 16, "1 header + 15 rows");
        assert!(out.contains("p14 (files)"));
        assert!(!out.contains("p15 (files)"));
    }

    /// `padEnd(9)` aligns the project column; a longer duration is NOT
    /// truncated, it just pushes the column right.
    #[test]
    fn duration_column_is_padded_to_nine_and_never_truncated() {
        assert_eq!(pad_end("1h 0m", 9), "1h 0m    ");
        assert_eq!(pad_end("1000h 59m", 9), "1000h 59m");
        assert_eq!(pad_end("12345h 59m", 9), "12345h 59m");
    }

    /// Minutes ROUND rather than truncate — Node's `Math.round((s%3600)/60)`.
    /// 59m30s therefore reads "0h 60m", which we reproduce deliberately.
    #[test]
    fn minutes_round_including_the_sixty_minute_quirk() {
        assert_eq!(h(0.0), "0h 0m");
        assert_eq!(h(29.0), "0h 0m");
        assert_eq!(h(30.0), "0h 1m");
        assert_eq!(h(3570.0), "0h 60m");
        assert_eq!(h(3600.0), "1h 0m");
    }

    /// Durations render as integers even when the server sends a float —
    /// `${}` on an integral Number never prints a `.0` suffix.
    #[test]
    fn integral_durations_render_without_a_decimal_point() {
        assert_eq!(h(3600.4), "1h 0m");
        assert!(!h(3660.0).contains('.'));
    }

    #[test]
    fn missing_totals_renders_just_the_header() {
        assert_eq!(rendered(&json!({ "total": 0 })), "today: 0h 0m total\n");
    }
}
