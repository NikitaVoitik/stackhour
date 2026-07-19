//! The credit model: per-stream gap credit, grouped totals, timeline
//! segments, and day buckets.
//!
//! Port of `src/summarize.js`. The credit model is WakaTime-ish: sort a stream
//! of heartbeats by time; each one earns `min(gap-to-next, capSeconds)`; the
//! last one in a run earns `lastEventCreditSeconds`.
//!
//! Stream splitting encodes who can parallelize:
//!
//! * human streams split per `(machine, source)` — attention is
//!   single-threaded, so rapid switching between projects in one tool never
//!   double-counts;
//! * agent streams additionally split per project — three Claude sessions
//!   grinding on three projects at once each accrue real agent-hours.
//!
//! Ordering parity: the JS builds its buckets in `Map`s, which iterate in
//! insertion order, so every grouping here uses [`IndexMap`] rather than a
//! hashed map with arbitrary iteration order. The JS bucket keys are
//! `JSON.stringify([...])` of the tuple; this port uses real composite keys
//! ([`StreamKey`], `Vec<String>`) so no separator can ever be forged by a
//! field value containing the separator.

use crate::Heartbeat;
use chrono::{NaiveDate, TimeDelta};
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value};
use stackhour_core::{js_round, json_num};

/// The 8 groupable fields, in the JS declaration order (`src/server.js`).
/// This order is observable: `/api/detail` emits one `breakdowns` key per
/// field in exactly this sequence.
pub const GROUP_FIELDS: [&str; 8] = [
    "project", "source", "machine", "category", "language", "entity", "actor", "branch",
];

/// A heartbeat with its computed credit seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Credited {
    pub row: Heartbeat,
    pub credit: f64,
}

/// The composite credit-stream key: `JSON.stringify([machine, source, actor,
/// actor === 'agent' ? project : ''])` in the JS, a real struct here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StreamKey {
    machine: String,
    source: String,
    actor: String,
    /// Empty for humans: one stream per `(machine, source)` across projects.
    project: String,
}

impl StreamKey {
    fn of(row: &Heartbeat) -> Self {
        StreamKey {
            machine: row.machine.clone(),
            source: row.source.clone(),
            actor: row.actor.clone(),
            project: if row.actor == "agent" {
                row.project.clone()
            } else {
                String::new()
            },
        }
    }
}

/// `a.time - b.time` as a sort comparator. Non-finite times cannot reach here
/// (ingest rejects them), but a NaN would make the JS comparator return NaN,
/// which V8 treats as "keep order" — `Ordering::Equal` reproduces that.
fn by_time(a: &Heartbeat, b: &Heartbeat) -> std::cmp::Ordering {
    a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal)
}

/// Compute credit per row. Streams are keyed by a real composite key
/// ([`StreamKey`]); rows are time-sorted per stream; heartbeat *i* earns
/// `min(time[i+1] - time[i], cap_s)` and the last heartbeat of a stream earns
/// `last_s`.
///
/// Output order matches the JS: streams in first-seen order, time-ascending
/// (stably) within each stream.
pub fn compute_credits(rows: Vec<Heartbeat>, cap_s: f64, last_s: f64) -> Vec<Credited> {
    let mut streams: IndexMap<StreamKey, Vec<Heartbeat>> = IndexMap::new();
    for row in rows {
        streams.entry(StreamKey::of(&row)).or_default().push(row);
    }

    let mut out = Vec::new();
    for (_, mut stream) in streams {
        stream.sort_by(by_time);
        // Snapshot the sorted times so each row can see its successor's time
        // after the vector is consumed.
        let times: Vec<f64> = stream.iter().map(|r| r.time).collect();
        let len = stream.len();
        for (i, row) in stream.into_iter().enumerate() {
            // The JS uses `Infinity` for the last gap and then
            // `gap === Infinity ? lastEventCreditSeconds : Math.min(gap, cap)`.
            let credit = if i + 1 < len {
                (times[i + 1] - row.time).min(cap_s)
            } else {
                last_s
            };
            out.push(Credited { row, credit });
        }
    }
    out
}

/// The bucket label tuple for a row: `keys.map((k) => r[k] ?? 'unknown')`.
fn labels(row: &Heartbeat, keys: &[&str]) -> Vec<String> {
    keys.iter().map(|k| row.field_label(k)).collect()
}

/// `Math.round(x * 100) / 100` — the 2-decimal cost rounding, reproduced with
/// JS `Math.round` semantics (ties toward +infinity) rather than Rust's
/// half-away-from-zero `f64::round`.
fn round2(x: f64) -> f64 {
    js_round(x * 100.0) as f64 / 100.0
}

#[derive(Default)]
struct Totals {
    seconds: f64,
    tokens: f64,
    cost: f64,
}

/// Grouped totals: `?? 'unknown'` tuple buckets, `seconds` via
/// [`js_round`], `cost` via `Math.round(x*100)/100`, `tokens` raw, sorted by
/// the *rounded* seconds descending (stably, like `Array.prototype.sort`).
///
/// Key order of each object is `{seconds, tokens, cost, <key1>, <key2>, …}`,
/// matching the JS object-literal + `forEach` assignment order.
pub fn totals_by(rows: &[Credited], keys: &[&str]) -> Vec<Value> {
    let mut buckets: IndexMap<Vec<String>, Totals> = IndexMap::new();
    for c in rows {
        let t = buckets.entry(labels(&c.row, keys)).or_default();
        t.seconds += c.credit;
        // Token counts accumulate as f64 exactly like JS numbers do, so the
        // (astronomically unlikely) >2^53 overflow behaviour matches too.
        t.tokens += c.row.tokens_in as f64 + c.row.tokens_out as f64;
        t.cost += c.row.cost;
    }

    let mut out: Vec<(i64, Value)> = buckets
        .into_iter()
        .map(|(parts, t)| {
            let seconds = js_round(t.seconds);
            let mut obj = Map::new();
            obj.insert("seconds".into(), Value::Number(json_num(seconds as f64)));
            obj.insert("tokens".into(), Value::Number(json_num(t.tokens)));
            obj.insert("cost".into(), Value::Number(json_num(round2(t.cost))));
            for (i, k) in keys.iter().enumerate() {
                obj.insert((*k).to_string(), Value::from(parts[i].clone()));
            }
            (seconds, Value::Object(obj))
        })
        .collect();
    // `sort((a, b) => b.seconds - a.seconds)` — stable, on the rounded value.
    out.sort_by_key(|(seconds, _)| std::cmp::Reverse(*seconds));
    out.into_iter().map(|(_, v)| v).collect()
}

/// One in-progress timeline segment.
struct Segment {
    project: String,
    actor: String,
    start: f64,
    end: f64,
    seconds: f64,
    sources: IndexSet<String>,
}

impl Segment {
    /// `{project, actor, start, end, seconds: Math.round(s), sources: [...]}` —
    /// the JS spreads `seg` (whose literal declares the keys in this order),
    /// replacing `sources` and `seconds` in place, so the key order is stable.
    fn to_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("project".into(), Value::from(self.project.clone()));
        obj.insert("actor".into(), Value::from(self.actor.clone()));
        obj.insert("start".into(), Value::Number(json_num(self.start)));
        obj.insert("end".into(), Value::Number(json_num(self.end)));
        obj.insert(
            "seconds".into(),
            Value::Number(json_num(js_round(self.seconds) as f64)),
        );
        obj.insert(
            "sources".into(),
            Value::Array(self.sources.iter().map(|s| Value::from(s.clone())).collect()),
        );
        Value::Object(obj)
    }
}

/// Timeline segments per `(project, actor)`: a new segment starts when
/// `row.time - segment.end > join_gap`, otherwise the segment is extended.
/// `sources` is an insertion-ordered unique set. The result is sorted by
/// `start` ascending (stably).
///
/// A single-heartbeat segment has `start == end` and `seconds` equal to that
/// row's credit.
pub fn build_segments(rows: &[Credited], join_gap: f64) -> Vec<Value> {
    let mut groups: IndexMap<(String, String), Vec<&Credited>> = IndexMap::new();
    for c in rows {
        groups
            .entry((c.row.project.clone(), c.row.actor.clone()))
            .or_default()
            .push(c);
    }

    let mut segments: Vec<Segment> = Vec::new();
    for ((project, actor), mut group) in groups {
        group.sort_by(|a, b| by_time(&a.row, &b.row));
        let mut seg: Option<Segment> = None;
        for c in group {
            match seg.as_mut() {
                Some(s) if c.row.time - s.end <= join_gap => {
                    s.end = c.row.time;
                    s.seconds += c.credit;
                    s.sources.insert(c.row.source.clone());
                }
                _ => {
                    if let Some(prev) = seg.take() {
                        segments.push(prev);
                    }
                    let mut sources = IndexSet::new();
                    sources.insert(c.row.source.clone());
                    seg = Some(Segment {
                        project: project.clone(),
                        actor: actor.clone(),
                        start: c.row.time,
                        end: c.row.time,
                        seconds: c.credit,
                        sources,
                    });
                }
            }
        }
        if let Some(last) = seg {
            segments.push(last);
        }
    }

    segments.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    segments.iter().map(Segment::to_value).collect()
}

/// The JS day comparator, ported verbatim: `(a, b) => (a.date < b.date ? -1 : 1)`.
/// It NEVER returns 0 — equal dates report `1`, i.e. "a comes after b".
///
/// V8's TimSort only ever moves an element when the comparator reports a
/// strict `< 0`, so an inconsistent `1` on ties still leaves same-date entries
/// in Map-insertion order. [`day_buckets`] therefore feeds Rust's stable sort
/// `Ordering::Equal` on ties (see [`day_sort_ordering`]) to reproduce that
/// observed output; Rust's merge would otherwise *reverse* tied runs.
fn js_day_compare(a: &str, b: &str) -> i32 {
    if a < b {
        -1
    } else {
        1
    }
}

/// [`js_day_compare`] mapped onto the ordering that reproduces V8's actual
/// output: strictly ascending by date, ties left in insertion order.
fn day_sort_ordering(a: &str, b: &str) -> std::cmp::Ordering {
    if js_day_compare(a, b) < 0 {
        std::cmp::Ordering::Less
    } else if a == b {
        std::cmp::Ordering::Equal
    } else {
        std::cmp::Ordering::Greater
    }
}

/// `new Date((time - tzOffsetMinutes * 60) * 1000).toISOString().slice(0, 10)`.
///
/// `tz_min` follows the `Date.getTimezoneOffset()` convention (positive =
/// behind UTC) and is SUBTRACTED, after which the plain UTC date is taken.
/// The `Date` constructor applies ToInteger (truncation toward zero) to a
/// fractional millisecond value, hence the `trunc()` before the calendar math.
///
/// Out-of-range instants (JS `Invalid Date`, |ms| > 8.64e15, where
/// `toISOString` throws) and years beyond chrono's calendar yield
/// `"Invalid Date"` rather than panicking — unreachable for real heartbeats,
/// whose times are finite epoch seconds.
fn day_string(time: f64, tz_min: i64) -> String {
    let ms = (time - (tz_min as f64) * 60.0) * 1000.0;
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return "Invalid Date".to_string();
    }
    let days = (ms.trunc() as i64).div_euclid(86_400_000);
    NaiveDate::from_ymd_opt(1970, 1, 1)
        .and_then(|epoch| epoch.checked_add_signed(TimeDelta::days(days)))
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "Invalid Date".to_string())
}

/// Per-local-day totals grouped by `keys`. Bucket key is
/// `[day, ...keys.map((k) => r[k] ?? 'unknown')]`; output objects are
/// `{date, seconds, <key1>, …}` sorted date-ascending.
pub fn day_buckets(rows: &[Credited], keys: &[&str], tz_min: i64) -> Vec<Value> {
    let mut buckets: IndexMap<Vec<String>, f64> = IndexMap::new();
    for c in rows {
        let mut key = Vec::with_capacity(keys.len() + 1);
        key.push(day_string(c.row.time, tz_min));
        key.extend(labels(&c.row, keys));
        *buckets.entry(key).or_insert(0.0) += c.credit;
    }

    let mut out: Vec<(String, Value)> = buckets
        .into_iter()
        .map(|(parts, seconds)| {
            let mut obj = Map::new();
            obj.insert("date".into(), Value::from(parts[0].clone()));
            obj.insert(
                "seconds".into(),
                Value::Number(json_num(js_round(seconds) as f64)),
            );
            for (i, k) in keys.iter().enumerate() {
                obj.insert((*k).to_string(), Value::from(parts[i + 1].clone()));
            }
            (parts[0].clone(), Value::Object(obj))
        })
        .collect();
    out.sort_by(|a, b| day_sort_ordering(&a.0, &b.0));
    out.into_iter().map(|(_, v)| v).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hb(id: i64, time: f64, machine: &str, source: &str, project: &str, actor: &str) -> Heartbeat {
        Heartbeat {
            id,
            time,
            machine: machine.into(),
            source: source.into(),
            project: project.into(),
            entity: format!("/f{id}.rs"),
            entity_type: "file".into(),
            category: "coding".into(),
            language: Some("Rust".into()),
            branch: None,
            is_write: 0,
            actor: actor.into(),
            tokens_in: 0,
            tokens_out: 0,
            cost: 0.0,
            created_at: 0.0,
        }
    }

    fn credited(rows: Vec<Heartbeat>) -> Vec<Credited> {
        compute_credits(rows, 120.0, 60.0)
    }

    #[test]
    fn group_fields_match_the_js_declaration_order() {
        assert_eq!(
            GROUP_FIELDS,
            ["project", "source", "machine", "category", "language", "entity", "actor", "branch"]
        );
    }

    /// Gap credit is `min(gap, cap)`; the last event earns `last_s`.
    #[test]
    fn credits_are_capped_gaps_with_a_last_event_bonus() {
        let rows = vec![
            hb(1, 0.0, "mac", "zed", "a", "human"),
            hb(2, 30.0, "mac", "zed", "a", "human"),
            hb(3, 1000.0, "mac", "zed", "a", "human"),
        ];
        let out = credited(rows);
        assert_eq!(out.iter().map(|c| c.credit).collect::<Vec<_>>(), vec![30.0, 120.0, 60.0]);
    }

    /// A human bouncing between projects in one editor is ONE stream (no
    /// double counting): 3 rows -> 2 gaps + one 60s tail.
    #[test]
    fn humans_share_one_stream_across_projects() {
        let rows = vec![
            hb(1, 0.0, "mac", "zed", "a", "human"),
            hb(2, 10.0, "mac", "zed", "b", "human"),
            hb(3, 20.0, "mac", "zed", "a", "human"),
        ];
        let out = credited(rows);
        assert_eq!(out.len(), 3);
        assert_eq!(out.iter().map(|c| c.credit).sum::<f64>(), 10.0 + 10.0 + 60.0);
    }

    /// Agents split per project, so two parallel sessions each get their own
    /// stream — and each earns its own last-event bonus.
    #[test]
    fn agents_split_per_project() {
        let rows = vec![
            hb(1, 0.0, "mac", "claude-code", "a", "agent"),
            hb(2, 10.0, "mac", "claude-code", "b", "agent"),
            hb(3, 20.0, "mac", "claude-code", "a", "agent"),
        ];
        let out = credited(rows);
        // stream a: 20-0=20 then tail 60; stream b: single row -> tail 60.
        assert_eq!(out.iter().map(|c| c.row.id).collect::<Vec<_>>(), vec![1, 3, 2]);
        assert_eq!(out.iter().map(|c| c.credit).collect::<Vec<_>>(), vec![20.0, 60.0, 60.0]);
    }

    /// The composite key must not collide the way a naive string join would:
    /// `("a|b", "c")` and `("a", "b|c")` are different streams.
    #[test]
    fn composite_stream_key_cannot_be_forged_by_separators() {
        let rows = vec![
            hb(1, 0.0, "a|b", "c", "p", "human"),
            hb(2, 10.0, "a", "b|c", "p", "human"),
        ];
        let out = credited(rows);
        // Two separate streams, each a lone row earning the tail credit.
        assert_eq!(out.iter().map(|c| c.credit).collect::<Vec<_>>(), vec![60.0, 60.0]);
    }

    /// Rows arrive unsorted; each stream is time-sorted before crediting.
    #[test]
    fn streams_are_time_sorted() {
        let rows = vec![
            hb(3, 50.0, "mac", "zed", "a", "human"),
            hb(1, 0.0, "mac", "zed", "a", "human"),
            hb(2, 10.0, "mac", "zed", "a", "human"),
        ];
        let out = credited(rows);
        assert_eq!(out.iter().map(|c| c.row.id).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(out.iter().map(|c| c.credit).collect::<Vec<_>>(), vec![10.0, 40.0, 60.0]);
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(credited(vec![]).is_empty());
        assert!(totals_by(&[], &["project"]).is_empty());
        assert!(build_segments(&[], 300.0).is_empty());
        assert!(day_buckets(&[], &["project"], 0).is_empty());
    }

    #[test]
    fn totals_are_bucketed_rounded_and_sorted_desc() {
        let mut a = hb(1, 0.0, "mac", "zed", "alpha", "human");
        a.tokens_in = 10;
        a.tokens_out = 5;
        a.cost = 1.005;
        let mut b = hb(2, 10.0, "mac", "zed", "alpha", "human");
        b.cost = 0.004;
        let rows = credited(vec![a, b, hb(3, 0.0, "mac", "codex-cli", "beta", "agent")]);
        let totals = totals_by(&rows, &["project"]);
        assert_eq!(totals.len(), 2);
        // alpha: 10 + 60 = 70s; beta: 60s.
        assert_eq!(
            serde_json::to_string(&totals[0]).expect("ser"),
            r#"{"seconds":70,"tokens":15,"cost":1.01,"project":"alpha"}"#
        );
        assert_eq!(
            serde_json::to_string(&totals[1]).expect("ser"),
            r#"{"seconds":60,"tokens":0,"cost":0,"project":"beta"}"#
        );
    }

    /// NULL columns bucket as the literal string 'unknown' (`?? 'unknown'`).
    #[test]
    fn null_fields_bucket_as_unknown() {
        let rows = credited(vec![hb(1, 0.0, "mac", "zed", "alpha", "human")]);
        let totals = totals_by(&rows, &["branch"]);
        assert_eq!(totals[0]["branch"], Value::from("unknown"));
    }

    /// Multi-key buckets keep the requested key order after seconds/tokens/cost.
    #[test]
    fn multi_key_totals_keep_key_order() {
        let rows = credited(vec![hb(1, 0.0, "mac", "zed", "alpha", "human")]);
        let totals = totals_by(&rows, &["project", "source"]);
        let keys: Vec<&str> = totals[0]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["seconds", "tokens", "cost", "project", "source"]);
    }

    /// `Math.round(x*100)/100` with JS tie-toward-+infinity: 1.005 -> 1
    /// (because 1.005*100 is 100.49999999999999), 1.006 -> 1.01.
    #[test]
    fn cost_rounds_with_js_semantics() {
        assert_eq!(round2(1.005), 1.0);
        assert_eq!(round2(1.006), 1.01);
        assert_eq!(round2(2.555), 2.56);
        assert_eq!(round2(0.0), 0.0);
    }

    #[test]
    fn segments_merge_within_the_join_gap_and_split_beyond_it() {
        let mut r2 = hb(2, 100.0, "mac", "editor-files", "alpha", "human");
        r2.source = "webstorm".into();
        let rows = credited(vec![
            hb(1, 0.0, "mac", "editor-files", "alpha", "human"),
            r2,
            hb(3, 5000.0, "mac", "editor-files", "alpha", "human"),
        ]);
        let segs = build_segments(&rows, 300.0);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0]["start"], Value::from(0));
        assert_eq!(segs[0]["end"], Value::from(100));
        // r1 and r2 are in DIFFERENT credit streams (different sources), so
        // r1's gap is the capped 5000s hop to r3 (120) and r2 is a lone
        // stream earning the 60s tail: the merged segment is 180, not 160.
        assert_eq!(segs[0]["seconds"], Value::from(180));
        assert_eq!(
            segs[0]["sources"],
            Value::Array(vec![Value::from("editor-files"), Value::from("webstorm")])
        );
        // Lone trailing row: start == end, seconds == its credit.
        assert_eq!(segs[1]["start"], Value::from(5000));
        assert_eq!(segs[1]["end"], Value::from(5000));
        assert_eq!(segs[1]["seconds"], Value::from(60));
    }

    #[test]
    fn segments_split_by_actor_and_sort_by_start() {
        let rows = credited(vec![
            hb(1, 500.0, "mac", "zed", "alpha", "human"),
            hb(2, 0.0, "mac", "claude-code", "alpha", "agent"),
        ]);
        let segs = build_segments(&rows, 300.0);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0]["actor"], Value::from("agent"));
        assert_eq!(segs[1]["actor"], Value::from("human"));
        let keys: Vec<&str> = segs[0]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["project", "actor", "start", "end", "seconds", "sources"]);
    }

    /// The boundary is inclusive: exactly `join_gap` still extends.
    #[test]
    fn join_gap_boundary_is_inclusive() {
        let rows = credited(vec![
            hb(1, 0.0, "mac", "zed", "alpha", "human"),
            hb(2, 300.0, "mac", "zed", "alpha", "human"),
        ]);
        assert_eq!(build_segments(&rows, 300.0).len(), 1);
        assert_eq!(build_segments(&rows, 299.0).len(), 2);
    }

    /// tz follows getTimezoneOffset(): positive = behind UTC, and is
    /// SUBTRACTED before taking the UTC date.
    #[test]
    fn day_string_shifts_then_takes_the_utc_date() {
        // 2024-01-02T00:30:00Z
        let t = 1_704_155_400.0;
        assert_eq!(day_string(t, 0), "2024-01-02");
        // UTC-5 (offset +300): local time is 2024-01-01T19:30 -> previous day.
        assert_eq!(day_string(t, 300), "2024-01-01");
        // UTC+2 (offset -120): still 2024-01-02.
        assert_eq!(day_string(t, -120), "2024-01-02");
        assert_eq!(day_string(0.0, 0), "1970-01-01");
        // Pre-epoch times floor to the previous day, not toward zero.
        assert_eq!(day_string(-1.0, 0), "1969-12-31");
        // Fractional epoch seconds truncate like the Date constructor.
        assert_eq!(day_string(86_399.999_5, 0), "1970-01-01");
    }

    #[test]
    fn day_string_guards_invalid_instants() {
        assert_eq!(day_string(f64::NAN, 0), "Invalid Date");
        assert_eq!(day_string(f64::INFINITY, 0), "Invalid Date");
        assert_eq!(day_string(1e14, 0), "Invalid Date");
    }

    #[test]
    fn day_buckets_group_and_sort_ascending() {
        let rows = credited(vec![
            hb(1, 1_704_155_400.0, "mac", "zed", "alpha", "human"), // 2024-01-02
            hb(2, 1_704_070_800.0, "mac", "webstorm", "beta", "human"), // 2024-01-01T01:00Z
        ]);
        let days = day_buckets(&rows, &["project"], 0);
        assert_eq!(days.len(), 2);
        assert_eq!(
            serde_json::to_string(&days[0]).expect("ser"),
            r#"{"date":"2024-01-01","seconds":60,"project":"beta"}"#
        );
        assert_eq!(days[1]["date"], Value::from("2024-01-02"));
    }

    /// With no group keys the output is just `{date, seconds}`.
    #[test]
    fn day_buckets_without_keys() {
        let rows = credited(vec![hb(1, 1_704_155_400.0, "mac", "zed", "alpha", "human")]);
        let days = day_buckets(&rows, &[], 0);
        assert_eq!(
            serde_json::to_string(&days[0]).expect("ser"),
            r#"{"date":"2024-01-02","seconds":60}"#
        );
    }

    /// The ported comparator never reports equality...
    #[test]
    fn js_day_comparator_never_returns_zero() {
        assert_eq!(js_day_compare("2024-01-01", "2024-01-02"), -1);
        assert_eq!(js_day_compare("2024-01-02", "2024-01-01"), 1);
        assert_eq!(js_day_compare("2024-01-01", "2024-01-01"), 1);
    }

    /// ...but same-date entries still come out in insertion order, as they do
    /// under V8's TimSort.
    #[test]
    fn same_date_buckets_keep_insertion_order() {
        let rows = credited(vec![
            hb(1, 1_704_155_400.0, "mac", "zed", "zeta", "human"),
            hb(2, 1_704_155_500.0, "mac", "zed", "alpha", "human"),
        ]);
        let days = day_buckets(&rows, &["project"], 0);
        assert_eq!(days.len(), 2);
        assert_eq!(days[0]["project"], Value::from("zeta"));
        assert_eq!(days[1]["project"], Value::from("alpha"));
    }
}
