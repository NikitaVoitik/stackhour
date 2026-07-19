//! File-save reattribution: editor-file saves near an agent edit become
//! agent activity with the edit's source.
//!
//! Kept independently unit-testable (the JS exports it for tests).

use crate::Heartbeat;
use rusqlite::Connection;
use stackhour_core::Result;
use std::collections::HashMap;

/// Candidate agent edits (actor=agent, entity_type=file, is_write) are
/// grouped by (machine, entity) and time-sorted; for each row with
/// source == 'editor-files' ONLY, binary-search the nearest edit within
/// `window_s`; ties go to the EARLIER edit; matched rows get actor=agent and
/// the edit's source.
///
/// Parity notes vs `reattributeFileSaves` in src/server.js:
///
/// * The JS group key is `JSON.stringify([machine, entity])`; here it is the
///   `(machine, entity)` tuple itself — no separator-collision risk either way.
/// * The candidate scan is exactly `[lo - 1, lo]` with a **strict** `<`
///   distance comparison, so at an equal distance the earlier edit (index
///   `lo - 1`) wins.
/// * `is_write` is truthiness-tested (any non-zero), matching JS `r.is_write`.
/// * Every row whose source is not exactly `editor-files`, and every row with
///   no agent edits for its `(machine, entity)`, passes through untouched.
/// * When there are no agent edits at all, the input is returned as-is.
pub fn reattribute_file_saves(rows: Vec<Heartbeat>, window_s: f64) -> Vec<Heartbeat> {
    // (time, source) of every candidate agent edit, grouped by (machine, entity).
    let mut edits_by_entity: HashMap<(&str, &str), Vec<(f64, &str)>> = HashMap::new();
    for r in &rows {
        if r.actor == "agent" && r.entity_type == "file" && r.is_write != 0 {
            edits_by_entity
                .entry((r.machine.as_str(), r.entity.as_str()))
                .or_default()
                .push((r.time, r.source.as_str()));
        }
    }
    if edits_by_entity.is_empty() {
        return rows;
    }
    for edits in edits_by_entity.values_mut() {
        // JS Array#sort is stable, and so is slice::sort_by; NaN times compare
        // as "equal" here, mirroring the JS comparator returning NaN (which
        // V8's sort treats as 0).
        edits.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    }

    // Resolve every match first (this borrows `rows`), then apply the patches.
    let patches: Vec<Option<String>> = rows
        .iter()
        .map(|r| {
            if r.source != "editor-files" {
                return None;
            }
            let edits = edits_by_entity.get(&(r.machine.as_str(), r.entity.as_str()))?;
            // Lower bound: the first index whose time is not < r.time. Spelled
            // out rather than using partition_point so that a NaN comparison
            // behaves like the JS `edits[mid].time < r.time` (false -> hi = mid).
            let (mut lo, mut hi) = (0usize, edits.len());
            while lo < hi {
                let mid = (lo + hi) / 2;
                if edits[mid].0 < r.time {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            let mut best: Option<&str> = None;
            let mut best_distance = f64::INFINITY;
            for index in [lo.checked_sub(1), Some(lo)].into_iter().flatten() {
                let Some(&(time, source)) = edits.get(index) else {
                    continue;
                };
                let distance = (time - r.time).abs();
                // Strict `<` keeps the EARLIER edit on a distance tie.
                if distance <= window_s && distance < best_distance {
                    best = Some(source);
                    best_distance = distance;
                }
            }
            best.map(str::to_string)
        })
        .collect();

    let mut rows = rows;
    for (row, patch) in rows.iter_mut().zip(patches) {
        if let Some(source) = patch {
            row.actor = "agent".to_string();
            row.source = source;
        }
    }
    rows
}

/// Query `[from - window, to + window]`, reattribute, then filter back to
/// `[from, to]` (widening both sides so boundary saves can match).
///
/// The widening is the point: an agent edit slightly *outside* the requested
/// range can still flip an in-range `editor-files` save to actor=agent.
pub fn reattributed_range(db: &Connection, from: f64, to: f64, window_s: f64) -> Result<Vec<Heartbeat>> {
    let raw = crate::db::rows_in_range(db, from - window_s, to + window_s)?;
    Ok(reattribute_file_saves(raw, window_s)
        .into_iter()
        .filter(|r| r.time >= from && r.time <= to)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hb(id: i64, time: f64, source: &str, actor: &str, is_write: i64, entity: &str) -> Heartbeat {
        Heartbeat {
            id,
            time,
            machine: "mac".into(),
            source: source.into(),
            project: "alpha".into(),
            entity: entity.into(),
            entity_type: "file".into(),
            category: "coding".into(),
            language: None,
            branch: None,
            is_write,
            actor: actor.into(),
            tokens_in: 0,
            tokens_out: 0,
            cost: 0.0,
            created_at: 0.0,
        }
    }

    fn save(id: i64, time: f64) -> Heartbeat {
        hb(id, time, "editor-files", "human", 1, "/a.js")
    }

    fn edit(id: i64, time: f64, source: &str) -> Heartbeat {
        hb(id, time, source, "agent", 1, "/a.js")
    }

    #[test]
    fn no_agent_edits_passes_rows_through() {
        let rows = vec![save(1, 100.0), save(2, 200.0)];
        assert_eq!(reattribute_file_saves(rows.clone(), 120.0), rows);
    }

    #[test]
    fn save_within_window_becomes_the_agents() {
        let out = reattribute_file_saves(vec![save(1, 100.0), edit(2, 130.0, "claude-code")], 120.0);
        assert_eq!(out[0].actor, "agent");
        assert_eq!(out[0].source, "claude-code");
        // the edit row itself is untouched
        assert_eq!(out[1].actor, "agent");
        assert_eq!(out[1].source, "claude-code");
    }

    #[test]
    fn save_outside_window_is_untouched() {
        let out = reattribute_file_saves(vec![save(1, 100.0), edit(2, 300.0, "claude-code")], 120.0);
        assert_eq!(out[0].actor, "human");
        assert_eq!(out[0].source, "editor-files");
    }

    /// The window boundary is inclusive (`distance <= windowSeconds`).
    #[test]
    fn window_boundary_is_inclusive() {
        let out = reattribute_file_saves(vec![save(1, 100.0), edit(2, 220.0, "codex-cli")], 120.0);
        assert_eq!(out[0].source, "codex-cli");
        let out = reattribute_file_saves(vec![save(1, 100.0), edit(2, 220.000_001, "codex-cli")], 120.0);
        assert_eq!(out[0].source, "editor-files");
    }

    /// Equal distance either side -> the EARLIER edit wins (strict `<`).
    #[test]
    fn equidistant_tie_keeps_the_earlier_edit() {
        let out = reattribute_file_saves(
            vec![
                edit(1, 90.0, "claude-code"),
                save(2, 100.0),
                edit(3, 110.0, "codex-cli"),
            ],
            120.0,
        );
        assert_eq!(out[1].source, "claude-code");
    }

    /// A strictly nearer later edit still beats a farther earlier one.
    #[test]
    fn nearest_edit_wins_when_not_a_tie() {
        let out = reattribute_file_saves(
            vec![
                edit(1, 50.0, "claude-code"),
                save(2, 100.0),
                edit(3, 105.0, "codex-cli"),
            ],
            120.0,
        );
        assert_eq!(out[1].source, "codex-cli");
    }

    /// An edit at exactly the save's time is found via the `lo` candidate.
    #[test]
    fn exact_time_match_is_found() {
        let out = reattribute_file_saves(vec![save(1, 100.0), edit(2, 100.0, "zed-agent")], 120.0);
        assert_eq!(out[0].source, "zed-agent");
    }

    /// Only `source === 'editor-files'` rows are candidates for rewriting.
    #[test]
    fn only_editor_files_rows_are_rewritten() {
        let out = reattribute_file_saves(
            vec![
                hb(1, 100.0, "webstorm", "human", 1, "/a.js"),
                edit(2, 105.0, "claude-code"),
            ],
            120.0,
        );
        assert_eq!(out[0].actor, "human");
        assert_eq!(out[0].source, "webstorm");
    }

    /// Grouping is per (machine, entity): a nearby edit to another file, or on
    /// another machine, does not match.
    #[test]
    fn grouping_is_per_machine_and_entity() {
        let out = reattribute_file_saves(vec![save(1, 100.0), edit(2, 105.0, "claude-code")], 120.0);
        assert_eq!(out[0].source, "claude-code");

        let mut other_entity = edit(2, 105.0, "claude-code");
        other_entity.entity = "/b.js".into();
        let out = reattribute_file_saves(vec![save(1, 100.0), other_entity], 120.0);
        assert_eq!(out[0].source, "editor-files");

        let mut other_machine = edit(2, 105.0, "claude-code");
        other_machine.machine = "linux".into();
        let out = reattribute_file_saves(vec![save(1, 100.0), other_machine], 120.0);
        assert_eq!(out[0].source, "editor-files");
    }

    /// Candidate edits must be actor=agent AND entity_type=file AND is_write.
    #[test]
    fn candidate_edits_need_all_three_properties() {
        let mut not_a_write = edit(2, 105.0, "claude-code");
        not_a_write.is_write = 0;
        let out = reattribute_file_saves(vec![save(1, 100.0), not_a_write], 120.0);
        assert_eq!(out[0].source, "editor-files");

        let mut an_app = edit(2, 105.0, "claude-code");
        an_app.entity_type = "app".into();
        let out = reattribute_file_saves(vec![save(1, 100.0), an_app], 120.0);
        assert_eq!(out[0].source, "editor-files");

        let mut human = edit(2, 105.0, "webstorm");
        human.actor = "human".into();
        let out = reattribute_file_saves(vec![save(1, 100.0), human], 120.0);
        assert_eq!(out[0].source, "editor-files");
    }

    /// Row order is preserved and non-matching rows come back identical.
    #[test]
    fn preserves_input_order() {
        let rows = vec![
            save(1, 500.0),
            edit(2, 100.0, "claude-code"),
            save(3, 100.0),
            hb(4, 900.0, "webstorm", "human", 0, "/c.js"),
        ];
        let out = reattribute_file_saves(rows.clone(), 120.0);
        let ids: Vec<i64> = out.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert_eq!(out[0], rows[0]); // 400s away from the only edit
        assert_eq!(out[3], rows[3]);
        assert_eq!(out[2].actor, "agent");
    }

    /// Many edits: the binary search must find the true nearest neighbour.
    #[test]
    fn binary_search_finds_the_nearest_of_many() {
        let mut rows: Vec<Heartbeat> = (0..50)
            .map(|i| {
                let source = if i == 30 { "codex-cli" } else { "claude-code" };
                edit(i + 10, (i as f64) * 1000.0, source)
            })
            .collect();
        rows.push(save(1, 30_010.0));
        let out = reattribute_file_saves(rows, 120.0);
        let saved = out.iter().find(|r| r.id == 1).expect("save row");
        assert_eq!(saved.source, "codex-cli");
        assert_eq!(saved.actor, "agent");
    }

    /// Edits are time-sorted per group even when the input arrives unordered.
    #[test]
    fn edits_are_sorted_before_searching() {
        let out = reattribute_file_saves(
            vec![
                edit(1, 300.0, "late"),
                edit(2, 100.0, "early"),
                edit(3, 200.0, "mid"),
                save(4, 205.0),
            ],
            120.0,
        );
        assert_eq!(out[3].source, "mid");
    }

    /// A zero window only matches an exactly simultaneous edit.
    #[test]
    fn zero_window_requires_an_exact_time() {
        let out = reattribute_file_saves(vec![save(1, 100.0), edit(2, 100.5, "claude-code")], 0.0);
        assert_eq!(out[0].source, "editor-files");
        let out = reattribute_file_saves(vec![save(1, 100.0), edit(2, 100.0, "claude-code")], 0.0);
        assert_eq!(out[0].source, "claude-code");
    }
}
