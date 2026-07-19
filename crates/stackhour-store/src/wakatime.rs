//! `stackhour import-wakatime` — 30-day UTC chunks from a once-captured now,
//! Basic base64(apiKey) auth, 4-attempt NETWORK-ONLY retry with 1500*i
//! backoff and exact log lines, HTTP 402 treated as a successful partial
//! stop, per-day-project upserts via db.rs (side effect: opens/creates the
//! DB and runs migrations). Blocking reqwest.
//!
//! Port of `src/import-wakatime.js`.

use crate::db::{open_db, upsert_wakatime_day};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::Value;
use stackhour_core::config::Config;
use stackhour_core::jsnum::{js_display, js_number, js_truthy};
use stackhour_core::timeparse::iso_date_utc;
use stackhour_core::{Error, Result};
use std::time::Duration;

const API: &str = "https://api.wakatime.com/api/v1";
/// `fetchRetry(url, opts, tries = 4)` — 4 total attempts, 3 retries.
const TRIES: u32 = 4;
/// One millisecond-day, the unit of the JS chunk arithmetic.
const DAY_MS: f64 = 86_400_000.0;

/// One `summaries` chunk: the two UTC calendar dates that go into the query.
///
/// `chunkEnd = now - offset days`, `chunkStart = now - min(offset + 29, days - 1)`
/// days — both derived from a `now` captured ONCE before the loop, so a slow
/// import cannot drift its own ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chunk {
    start: String,
    end: String,
}

/// `for (let offset = 0; offset < days; offset += 30)` with the JS date math,
/// pulled out so the clamp and the UTC formatting are unit-testable.
///
/// `days` is whatever `Number(--days=…)` produced, including fractions and
/// NaN: a NaN bound makes `offset < days` false immediately and yields no
/// chunks (JS then prints `done: 0`), which falls out of the f64 comparison
/// for free.
pub(crate) fn chunk_ranges(now_ms: f64, days: f64) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut offset = 0.0_f64;
    while offset < days {
        let chunk_end_ms = now_ms - offset * DAY_MS;
        let back = (offset + 29.0).min(days - 1.0);
        let chunk_start_ms = now_ms - back * DAY_MS;
        out.push(Chunk {
            start: iso_date_utc(chunk_start_ms / 1000.0),
            end: iso_date_utc(chunk_end_ms / 1000.0),
        });
        offset += 30.0;
    }
    out
}

/// `err.cause?.code || err.message` — the label in the retry log line.
///
/// Node surfaces the underlying syscall code (`ECONNREFUSED`, `ENOTFOUND`, …)
/// on `err.cause`; the closest analogue here is the deepest source in the
/// error chain, which for a connect failure is the io error itself.
fn fetch_err_label(err: &reqwest::Error) -> String {
    let mut deepest: &dyn std::error::Error = err;
    while let Some(src) = std::error::Error::source(deepest) {
        deepest = src;
    }
    deepest.to_string()
}

/// `fetchRetry` — retries ONLY when the request itself fails (network), never
/// on an HTTP error status. Linear 1500*i backoff between attempts.
fn fetch_retry(
    client: &reqwest::blocking::Client,
    url: &str,
    auth: &str,
) -> Result<reqwest::blocking::Response> {
    let mut attempt = 1_u32;
    loop {
        match client.get(url).header("authorization", auth).send() {
            Ok(res) => return Ok(res),
            Err(err) => {
                if attempt >= TRIES {
                    return Err(Error::msg(fetch_err_label(&err)));
                }
                println!(
                    "[stackhour] fetch failed ({}), retry {}/{}",
                    fetch_err_label(&err),
                    attempt,
                    TRIES - 1
                );
                std::thread::sleep(Duration::from_millis(1500 * u64::from(attempt)));
                attempt += 1;
            }
        }
    }
}

/// Import `days` of WakaTime summaries into `wakatime_days`.
///
/// Side effect relied on by `stackhour import-wakatime` on a fresh machine:
/// `open_db` creates the data dir, the database and all migrations.
///
/// HTTP 402 is a SUCCESSFUL partial import — the loop breaks, whatever was
/// already upserted stays, and the process exits 0.
pub fn import_wakatime(cfg: &Config, days: f64) -> Result<()> {
    let key = if cfg.wakatime.api_key.is_empty() {
        std::env::var("WAKATIME_API_KEY").unwrap_or_default()
    } else {
        cfg.wakatime.api_key.clone()
    };
    if key.is_empty() {
        return Err(Error::msg(
            "no wakatime.apiKey in config and no WAKATIME_API_KEY set",
        ));
    }
    let auth = format!("Basic {}", B64.encode(key.as_bytes()));
    let db = open_db(&cfg.server.db)?;

    // No timeout: `fetch` has none either, and a slow summaries call must not
    // be turned into a retry storm.
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| Error::msg(e.to_string()))?;

    let now_ms = js_now_ms();
    let mut imported: u64 = 0;

    for chunk in chunk_ranges(now_ms, days) {
        let url = format!(
            "{API}/users/current/summaries?start={}&end={}",
            chunk.start, chunk.end
        );
        let res = fetch_retry(&client, &url, &auth)?;
        let status = res.status().as_u16();
        if status == 402 {
            println!(
                "[stackhour] wakatime: range {}..{} needs a paid plan; stopping (imported what was available)",
                chunk.start, chunk.end
            );
            break;
        }
        if !res.status().is_success() {
            // `await res.text()` — an unreadable body degrades to '' rather
            // than replacing the status-bearing message.
            let text = res.text().unwrap_or_default();
            return Err(Error::msg(format!("wakatime API {status}: {text}")));
        }
        let body: Value = res.json().map_err(|e| Error::msg(fetch_err_label(&e)))?;
        imported += apply_summaries(&db, &body)?;
        println!("[stackhour] imported {}..{}", chunk.start, chunk.end);
    }
    println!("[stackhour] done: {imported} day-project rows in wakatime_days");
    Ok(())
}

/// `for (const day of body.data || [])` — upsert every `(day.range.date,
/// project.name)` pair and return how many rows were touched.
///
/// Days without a `range.date` are skipped silently; `total_seconds || 0`
/// keeps the JS falsy-to-zero coercion.
fn apply_summaries(db: &rusqlite::Connection, body: &Value) -> Result<u64> {
    let mut imported = 0_u64;
    let days = match body.get("data").and_then(Value::as_array) {
        Some(a) => a,
        None => return Ok(0),
    };
    for day in days {
        let date = match day.get("range").and_then(|r| r.get("date")) {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            // `if (!date) continue` — anything falsy (absent, null, '') skips.
            Some(v) if js_truthy(v) => js_display(v),
            _ => continue,
        };
        let projects = match day.get("projects").and_then(Value::as_array) {
            Some(a) => a,
            None => continue,
        };
        for p in projects {
            let name = match p.get("name") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => js_display(v),
                // node:sqlite would throw on an undefined bind; a nameless
                // project is not worth aborting a whole backfill for.
                None => "undefined".to_string(),
            };
            let secs = match p.get("total_seconds") {
                Some(v) if js_truthy(v) => js_number(v),
                _ => 0.0,
            };
            upsert_wakatime_day(db, &date, &name, secs)?;
            imported += 1;
        }
    }
    Ok(imported)
}

/// `new Date().getTime()` — wall-clock epoch milliseconds as an f64.
fn js_now_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64() * 1000.0,
        Err(e) => -(e.duration().as_secs_f64() * 1000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 2026-07-19T12:00:00Z, so a chunk boundary never lands on midnight.
    const NOW_MS: f64 = 1_784_462_400_000.0;

    #[test]
    fn default_365_days_chunks_by_30_with_a_clamped_tail() {
        let chunks = chunk_ranges(NOW_MS, 365.0);
        // ceil(365 / 30) = 13 chunks (offsets 0, 30, …, 360).
        assert_eq!(chunks.len(), 13);
        assert_eq!(chunks[0].end, "2026-07-19");
        // offset 0 -> start = now - 29 days.
        assert_eq!(chunks[0].start, "2026-06-20");
        // offset 30 -> end = now - 30 days, start = now - 59 days.
        assert_eq!(chunks[1].end, "2026-06-19");
        assert_eq!(chunks[1].start, "2026-05-21");
        // Final chunk: offset 360, min(389, 364) = 364 days back — exactly
        // `days` of total coverage, not 390.
        let last = chunks.last().expect("chunk");
        assert_eq!(last.end, "2025-07-24");
        assert_eq!(last.start, "2025-07-20");
    }

    /// The `min(offset + 29, days - 1)` clamp: a short window never reaches
    /// back further than `days` in total.
    #[test]
    fn short_ranges_clamp_to_days_minus_one() {
        let one = chunk_ranges(NOW_MS, 1.0);
        assert_eq!(one.len(), 1);
        // days - 1 == 0 -> start and end are the same UTC day.
        assert_eq!(one[0].start, one[0].end);
        assert_eq!(one[0].start, "2026-07-19");

        let seven = chunk_ranges(NOW_MS, 7.0);
        assert_eq!(seven.len(), 1);
        assert_eq!(seven[0].end, "2026-07-19");
        assert_eq!(seven[0].start, "2026-07-13"); // 6 days back
    }

    /// Fractional and non-finite `--days` values behave as in JS: the loop
    /// bound is a plain `<` on a Number.
    #[test]
    fn odd_day_counts() {
        assert!(chunk_ranges(NOW_MS, f64::NAN).is_empty());
        assert!(chunk_ranges(NOW_MS, 0.0).is_empty());
        assert!(chunk_ranges(NOW_MS, -5.0).is_empty());
        // 30.5 days -> two chunks (offsets 0 and 30).
        assert_eq!(chunk_ranges(NOW_MS, 30.5).len(), 2);
        // 0.5 days: one chunk, and the clamp goes NEGATIVE (days - 1 = -0.5),
        // so the start is half a day in the FUTURE relative to now.
        let half = chunk_ranges(NOW_MS, 0.5);
        assert_eq!(half.len(), 1);
        assert_eq!(half[0].start, "2026-07-20");
        assert_eq!(half[0].end, "2026-07-19");
    }

    /// Dates are UTC calendar days regardless of the host timezone: a `now`
    /// just after UTC midnight formats as the new day.
    #[test]
    fn dates_are_utc() {
        let just_after_midnight = 1_784_419_200_000.0 + 60_000.0; // 2026-07-19T00:01Z
        let c = chunk_ranges(just_after_midnight, 1.0);
        assert_eq!(c[0].end, "2026-07-19");
    }

    fn open_mem() -> rusqlite::Connection {
        let db = rusqlite::Connection::open_in_memory().expect("open");
        db.execute_batch(
            "CREATE TABLE wakatime_days (date TEXT NOT NULL, project TEXT NOT NULL,
             seconds REAL NOT NULL, UNIQUE (date, project));",
        )
        .expect("ddl");
        db
    }

    fn rows(db: &rusqlite::Connection) -> Vec<(String, String, f64)> {
        let mut stmt = db
            .prepare("SELECT date, project, seconds FROM wakatime_days ORDER BY date, project")
            .expect("prepare");
        let out = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows");
        out
    }

    #[test]
    fn upserts_every_day_project_pair() {
        let db = open_mem();
        let body = json!({
            "data": [
                { "range": { "date": "2026-07-18" }, "projects": [
                    { "name": "alpha", "total_seconds": 120.5 },
                    { "name": "beta", "total_seconds": 0 },
                ]},
                { "range": { "date": "2026-07-19" }, "projects": [
                    { "name": "alpha", "total_seconds": 60 },
                ]},
            ]
        });
        let n = apply_summaries(&db, &body).expect("apply");
        assert_eq!(n, 3);
        assert_eq!(
            rows(&db),
            vec![
                ("2026-07-18".to_string(), "alpha".to_string(), 120.5),
                ("2026-07-18".to_string(), "beta".to_string(), 0.0),
                ("2026-07-19".to_string(), "alpha".to_string(), 60.0),
            ]
        );
    }

    /// A re-import overwrites the seconds for an existing (date, project).
    #[test]
    fn reimport_updates_seconds() {
        let db = open_mem();
        let first = json!({"data":[{"range":{"date":"2026-07-18"},
            "projects":[{"name":"alpha","total_seconds":10}]}]});
        let second = json!({"data":[{"range":{"date":"2026-07-18"},
            "projects":[{"name":"alpha","total_seconds":99}]}]});
        apply_summaries(&db, &first).expect("first");
        apply_summaries(&db, &second).expect("second");
        assert_eq!(
            rows(&db),
            vec![("2026-07-18".to_string(), "alpha".to_string(), 99.0)]
        );
    }

    /// `day.range?.date` guards: days without a usable date are skipped
    /// silently, and a missing `projects` array contributes nothing.
    #[test]
    fn skips_days_without_a_date_or_projects() {
        let db = open_mem();
        let body = json!({
            "data": [
                { "projects": [{ "name": "alpha", "total_seconds": 5 }] },
                { "range": {}, "projects": [{ "name": "beta", "total_seconds": 5 }] },
                { "range": { "date": null }, "projects": [{ "name": "c", "total_seconds": 5 }] },
                { "range": { "date": "" }, "projects": [{ "name": "d", "total_seconds": 5 }] },
                { "range": { "date": "2026-07-18" } },
                { "range": { "date": "2026-07-18" }, "projects": [] },
            ]
        });
        assert_eq!(apply_summaries(&db, &body).expect("apply"), 0);
        assert!(rows(&db).is_empty());
    }

    /// `p.total_seconds || 0` — every falsy value becomes 0 rather than an
    /// error or a NULL.
    #[test]
    fn missing_total_seconds_becomes_zero() {
        let db = open_mem();
        let body = json!({
            "data": [{ "range": { "date": "2026-07-18" }, "projects": [
                { "name": "a" },
                { "name": "b", "total_seconds": null },
                { "name": "c", "total_seconds": "" },
            ]}]
        });
        assert_eq!(apply_summaries(&db, &body).expect("apply"), 3);
        assert!(rows(&db).iter().all(|r| r.2 == 0.0));
    }

    /// `body.data || []` — a response without `data` is not an error.
    #[test]
    fn empty_body_imports_nothing() {
        let db = open_mem();
        assert_eq!(apply_summaries(&db, &json!({})).expect("apply"), 0);
        assert_eq!(
            apply_summaries(&db, &json!({"data": null})).expect("apply"),
            0
        );
    }

    /// The Authorization header is base64 of the raw key with NO trailing
    /// colon (WakaTime accepts `base64(api_key)`).
    #[test]
    fn auth_header_is_basic_base64_of_the_key() {
        let auth = format!("Basic {}", B64.encode("waka_secret".as_bytes()));
        assert_eq!(auth, "Basic d2FrYV9zZWNyZXQ=");
    }
}
