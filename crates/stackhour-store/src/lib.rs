//! stackhour-store — everything that touches stackhour.db: schema/migrations,
//! ingest, summarize math, reattribution, backup/restore, data ops, and the
//! WakaTime import.

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use stackhour_core::json_num;

pub mod backup;
pub mod data;
pub mod db;
pub mod reattribute;
pub mod restore;
pub mod summarize;
pub mod wakatime;

pub use backup::{create_backup, run_backup_cli, verify_backup, BackupInfo};
pub use data::run_data;
pub use db::{
    insert_heartbeats, list_agent_status, open_db, open_immutable, recent_page, rows_in_range,
    upsert_agent_status, upsert_wakatime_day, StatusAck,
};
pub use reattribute::{reattribute_file_saves, reattributed_range};
pub use restore::{maintenance_lock_path, restore_backup, RestoreOutcome};
pub use summarize::{build_segments, compute_credits, day_buckets, totals_by, Credited, GROUP_FIELDS};
pub use wakatime::import_wakatime;

/// The `heartbeats` column names, in DDL order. `SELECT *` in node:sqlite
/// yields object keys in exactly this order, so it is also the JSON key order
/// of every raw row the server echoes (/api/recent, /api/detail `recent`).
pub const HEARTBEAT_COLUMNS: [&str; 16] = [
    "id",
    "time",
    "machine",
    "source",
    "project",
    "entity",
    "entity_type",
    "category",
    "language",
    "branch",
    "is_write",
    "actor",
    "tokens_in",
    "tokens_out",
    "cost",
    "created_at",
];

/// One `heartbeats` row.
///
/// Field order matches the table DDL (see [`HEARTBEAT_COLUMNS`]) and the
/// hand-written [`Serialize`] impl emits the keys in that declaration order
/// under their DB column names — this is the wire shape of a raw row in
/// `/api/recent` and `/api/detail.recent`, where the JS reference does a plain
/// `SELECT *` and hands node:sqlite's row object to `JSON.stringify`.
///
/// Parity notes:
///
/// * Numbers go through [`stackhour_core::json_num`], so integral REAL columns
///   serialize the way `JSON.stringify` prints them (`"time":100`, not
///   `100.0`; `"cost":0`, not `0.0`). Non-integral values keep their
///   shortest-roundtrip form (`0.456`).
/// * `language` / `branch` are the only nullable columns; `None` serializes as
///   `null` (the key is always present, exactly like a SQLite row object).
/// * The agent's `{ machine, ...row }` stamping — where `machine` really is
///   the FIRST serialized key — applies to outbound *ingest* rows, which carry
///   neither `id` nor `created_at` and are built as JSON objects in the agent
///   crate. It is deliberately not this struct: a row that has an `id` came
///   out of `SELECT *` and must keep the DDL key order.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Heartbeat {
    pub id: i64,
    /// Unix epoch seconds.
    pub time: f64,
    pub machine: String,
    /// webstorm | zed | claude-code | claude-desktop | codex-cli | codex-desktop | editor-files | …
    pub source: String,
    pub project: String,
    /// File path or app name.
    pub entity: String,
    /// `file` | `app`.
    pub entity_type: String,
    pub category: String,
    pub language: Option<String>,
    pub branch: Option<String>,
    pub is_write: i64,
    /// `human` | `agent`.
    pub actor: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost: f64,
    /// Unix epoch seconds, stamped at insert time (not event time).
    pub created_at: f64,
}

impl Serialize for Heartbeat {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Heartbeat", HEARTBEAT_COLUMNS.len())?;
        st.serialize_field("id", &self.id)?;
        st.serialize_field("time", &json_num(self.time))?;
        st.serialize_field("machine", &self.machine)?;
        st.serialize_field("source", &self.source)?;
        st.serialize_field("project", &self.project)?;
        st.serialize_field("entity", &self.entity)?;
        st.serialize_field("entity_type", &self.entity_type)?;
        st.serialize_field("category", &self.category)?;
        st.serialize_field("language", &self.language)?;
        st.serialize_field("branch", &self.branch)?;
        st.serialize_field("is_write", &self.is_write)?;
        st.serialize_field("actor", &self.actor)?;
        st.serialize_field("tokens_in", &self.tokens_in)?;
        st.serialize_field("tokens_out", &self.tokens_out)?;
        st.serialize_field("cost", &json_num(self.cost))?;
        st.serialize_field("created_at", &json_num(self.created_at))?;
        st.end()
    }
}

impl Heartbeat {
    /// The row as a `serde_json::Value` object with the keys in DDL order
    /// (`serde_json`'s `preserve_order` feature is enabled workspace-wide, so
    /// insertion order survives).
    ///
    /// Used by the endpoints that need to touch a raw row generically
    /// (`/api/detail`'s dimension matching, `/api/summary`'s group fields)
    /// without hand-writing a match arm per column.
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), Value::from(self.id));
        m.insert("time".into(), Value::Number(json_num(self.time)));
        m.insert("machine".into(), Value::from(self.machine.clone()));
        m.insert("source".into(), Value::from(self.source.clone()));
        m.insert("project".into(), Value::from(self.project.clone()));
        m.insert("entity".into(), Value::from(self.entity.clone()));
        m.insert("entity_type".into(), Value::from(self.entity_type.clone()));
        m.insert("category".into(), Value::from(self.category.clone()));
        m.insert("language".into(), str_or_null(self.language.as_deref()));
        m.insert("branch".into(), str_or_null(self.branch.as_deref()));
        m.insert("is_write".into(), Value::from(self.is_write));
        m.insert("actor".into(), Value::from(self.actor.clone()));
        m.insert("tokens_in".into(), Value::from(self.tokens_in));
        m.insert("tokens_out".into(), Value::from(self.tokens_out));
        m.insert("cost".into(), Value::Number(json_num(self.cost)));
        m.insert("created_at".into(), Value::Number(json_num(self.created_at)));
        Value::Object(m)
    }

    /// The value of a groupable/dimension field by column name, as JS's
    /// `row[field]` would produce it: `None` for an unknown field name (JS
    /// `undefined`), `Some(Value::Null)` for a NULL `language`/`branch`.
    ///
    /// `/api/detail` relies on the null-vs-absent distinction: `value=''`
    /// selects rows where `row[dimension] == null`, while `value='unknown'`
    /// matches `String(row[dimension] ?? 'unknown')` — i.e. NULL *and* the
    /// literal string `'unknown'`.
    pub fn field(&self, name: &str) -> Option<Value> {
        Some(match name {
            "project" => Value::from(self.project.clone()),
            "source" => Value::from(self.source.clone()),
            "machine" => Value::from(self.machine.clone()),
            "category" => Value::from(self.category.clone()),
            "language" => str_or_null(self.language.as_deref()),
            "entity" => Value::from(self.entity.clone()),
            "actor" => Value::from(self.actor.clone()),
            "branch" => str_or_null(self.branch.as_deref()),
            "entity_type" => Value::from(self.entity_type.clone()),
            _ => return None,
        })
    }

    /// `String(row[field] ?? 'unknown')` — the bucket label used by
    /// `totalsBy` / `dayBuckets` and by `/api/detail`'s non-empty value match.
    /// Unknown field names bucket as `'unknown'` too (JS `undefined ?? …`).
    pub fn field_label(&self, name: &str) -> String {
        match self.field(name) {
            None | Some(Value::Null) => "unknown".to_string(),
            Some(Value::String(s)) => s,
            Some(other) => stackhour_core::js_display(&other),
        }
    }
}

fn str_or_null(s: Option<&str>) -> Value {
    match s {
        Some(v) => Value::from(v),
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Heartbeat {
        Heartbeat {
            id: 1,
            time: 100.0,
            machine: "mac".into(),
            source: "editor-files".into(),
            project: "alpha".into(),
            entity: "/a.js".into(),
            entity_type: "file".into(),
            category: "coding".into(),
            language: Some("JavaScript".into()),
            branch: None,
            is_write: 1,
            actor: "human".into(),
            tokens_in: 0,
            tokens_out: 0,
            cost: 0.0,
            created_at: 1_784_378_096.789,
        }
    }

    /// The exact JSON a `SELECT *` row produces in the JS reference:
    /// DDL key order, DB column names, integral REALs printed as integers.
    #[test]
    fn serializes_in_ddl_order_with_js_numbers() {
        let json = serde_json::to_string(&sample()).expect("serialize");
        assert_eq!(
            json,
            r#"{"id":1,"time":100,"machine":"mac","source":"editor-files","project":"alpha","entity":"/a.js","entity_type":"file","category":"coding","language":"JavaScript","branch":null,"is_write":1,"actor":"human","tokens_in":0,"tokens_out":0,"cost":0,"created_at":1784378096.789}"#
        );
    }

    #[test]
    fn key_order_matches_the_column_list() {
        let v = sample().to_value();
        let keys: Vec<&str> = v
            .as_object()
            .expect("object")
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(keys, HEARTBEAT_COLUMNS.to_vec());
        // …and the struct's own Serialize agrees with to_value().
        assert_eq!(serde_json::to_value(sample()).expect("to_value"), v);
    }

    /// Non-integral costs keep their shortest-roundtrip form, integral ones
    /// lose the `.0` — matching `JSON.stringify`.
    #[test]
    fn cost_and_time_number_formatting() {
        let mut hb = sample();
        hb.cost = 0.456;
        hb.time = 1_700_000_000.5;
        let json = serde_json::to_string(&hb).expect("serialize");
        assert!(json.contains(r#""cost":0.456"#), "{json}");
        assert!(json.contains(r#""time":1700000000.5"#), "{json}");
        hb.cost = 12.0;
        assert!(serde_json::to_string(&hb)
            .expect("serialize")
            .contains(r#""cost":12"#));
    }

    #[test]
    fn round_trips_through_json() {
        let hb = sample();
        let back: Heartbeat = serde_json::from_str(&serde_json::to_string(&hb).expect("ser")).expect("de");
        assert_eq!(back, hb);
    }

    /// `row[dimension]`: NULL columns are `Some(Null)` (selectable via
    /// `value=''`), unknown names are `None` (JS `undefined`).
    #[test]
    fn field_distinguishes_null_from_absent() {
        let hb = sample();
        assert_eq!(hb.field("branch"), Some(Value::Null));
        assert_eq!(hb.field("language"), Some(Value::from("JavaScript")));
        assert_eq!(hb.field("nope"), None);
        for name in GROUP_FIELDS {
            assert!(hb.field(name).is_some(), "{name} must be addressable");
        }
    }

    /// `String(row[field] ?? 'unknown')` — NULL and unknown names both bucket
    /// as 'unknown', which is why `value='unknown'` matches NULL rows too.
    #[test]
    fn field_label_falls_back_to_unknown() {
        let hb = sample();
        assert_eq!(hb.field_label("branch"), "unknown");
        assert_eq!(hb.field_label("nope"), "unknown");
        assert_eq!(hb.field_label("project"), "alpha");
        assert_eq!(hb.field_label("actor"), "human");
    }
}
