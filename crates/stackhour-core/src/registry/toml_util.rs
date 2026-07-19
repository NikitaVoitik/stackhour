//! Shared TOML accessors for every registry manifest parser.
//!
//! Before this module each of `command.rs`, `skill.rs`, `agent_def.rs`,
//! `prompt.rs` and `engine.rs` carried its own private `opt_string` /
//! `opt_bool` / `opt_string_list`, with five slightly different error
//! messages for the same mistake. These are the canonical versions; all
//! registry parsers must use them so that:
//!
//! - the message wording for "wrong type" is identical everywhere, and
//! - every message is a [`FieldError`] naming the key, so the loader can
//!   prefix the file path exactly once.
//!
//! Every accessor is total: a missing key is `Ok(None)` / the default, and a
//! present-but-wrong-typed key is an `Err` naming the key and the expected
//! type. Unknown keys are ignored by design (forward compatibility).

use indexmap::IndexMap;

use super::error::FieldError;

/// A TOML table (the `toml` crate's map type), spelled once.
pub type Table = toml::value::Table;

/// The document root as a table, or a file-level error.
pub fn root_table<'a>(v: &'a toml::Value, what: &str) -> Result<&'a Table, FieldError> {
    v.as_table()
        .ok_or_else(|| FieldError::file_level(format!("{what} must be a TOML table")))
}

/// `key` as a string; missing -> `None`; wrong type -> error.
pub fn opt_string(table: &Table, key: &str) -> Result<Option<String>, FieldError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(type_error(key, "a string", other)),
    }
}

/// `key` as a NON-EMPTY (after trimming) string; missing -> `None`. A present
/// but blank string is an error, because a blank label/description is always
/// a mistake rather than an intentional value.
pub fn opt_nonempty_string(table: &Table, key: &str) -> Result<Option<String>, FieldError> {
    match opt_string(table, key)? {
        None => Ok(None),
        Some(s) if s.trim().is_empty() => {
            Err(FieldError::key(key, "must be a non-empty string"))
        }
        Some(s) => Ok(Some(s)),
    }
}

/// `key` as a required non-empty string.
pub fn req_string(table: &Table, key: &str) -> Result<String, FieldError> {
    opt_nonempty_string(table, key)?
        .ok_or_else(|| FieldError::key(key, "is required and must be a non-empty string"))
}

/// `key` as a bool; missing -> `default`; wrong type -> error.
pub fn opt_bool(table: &Table, key: &str, default: bool) -> Result<bool, FieldError> {
    match table.get(key) {
        None => Ok(default),
        Some(toml::Value::Boolean(b)) => Ok(*b),
        Some(other) => Err(type_error(key, "a boolean", other)),
    }
}

/// `key` as a non-negative integer; missing -> `default`.
pub fn opt_u64(table: &Table, key: &str, default: u64) -> Result<u64, FieldError> {
    match table.get(key) {
        None => Ok(default),
        Some(toml::Value::Integer(i)) if *i >= 0 => Ok(*i as u64),
        Some(toml::Value::Integer(_)) => {
            Err(FieldError::key(key, "must be a non-negative integer"))
        }
        Some(other) => Err(type_error(key, "a non-negative integer", other)),
    }
}

/// `key` as an array of strings; missing -> `None`; wrong element type ->
/// error naming the offending index.
pub fn opt_string_list(table: &Table, key: &str) -> Result<Option<Vec<String>>, FieldError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item {
                    toml::Value::String(s) => out.push(s.clone()),
                    other => {
                        return Err(type_error(&format!("{key}[{i}]"), "a string", other)
                            .with_msg_prefix(format!("{key} must be an array of strings: ")))
                    }
                }
            }
            Ok(Some(out))
        }
        Some(other) => Err(type_error(key, "an array of strings", other)),
    }
}

/// `key` as an array of strings, defaulting to empty.
pub fn string_list(table: &Table, key: &str) -> Result<Vec<String>, FieldError> {
    Ok(opt_string_list(table, key)?.unwrap_or_default())
}

/// `key` as a NON-EMPTY array of strings (required).
pub fn req_string_list(table: &Table, key: &str) -> Result<Vec<String>, FieldError> {
    match opt_string_list(table, key)? {
        Some(v) if !v.is_empty() => Ok(v),
        Some(_) => Err(FieldError::key(key, "must not be empty")),
        None => Err(FieldError::key(key, "is required and must be a non-empty array of strings")),
    }
}

/// `key` as a sub-table; missing -> `None`; wrong type -> error.
pub fn opt_table<'a>(table: &'a Table, key: &str) -> Result<Option<&'a Table>, FieldError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Table(t)) => Ok(Some(t)),
        Some(other) => Err(type_error(key, "a table", other)),
    }
}

/// `key` as an array of sub-tables (`[[key]]`); missing -> empty.
pub fn table_array<'a>(table: &'a Table, key: &str) -> Result<Vec<&'a Table>, FieldError> {
    match table.get(key) {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item {
                    toml::Value::Table(t) => out.push(t),
                    other => return Err(type_error(&format!("{key}[{i}]"), "a table", other)),
                }
            }
            Ok(out)
        }
        Some(other) => Err(type_error(key, "an array of tables", other)),
    }
}

/// A `[key]` table of string values; missing -> empty. Used for `[env]` in
/// engines, agents and skills.
///
/// NOTE the ordering: the `toml` crate's map is a `BTreeMap`, so keys come
/// back SORTED, not in file order. That is fine for `[env]` (env vars are a
/// set) and it makes the result deterministic, which is what the registry
/// actually needs. Do not use this for anything order-sensitive.
pub fn string_map(table: &Table, key: &str) -> Result<IndexMap<String, String>, FieldError> {
    let mut out = IndexMap::new();
    let Some(sub) = opt_table(table, key)? else {
        return Ok(out);
    };
    for (k, val) in sub {
        match val {
            toml::Value::String(s) => {
                out.insert(k.clone(), s.clone());
            }
            other => return Err(type_error(&format!("{key}.{k}"), "a string", other)),
        }
    }
    Ok(out)
}

/// A string key constrained to a closed set of allowed values.
/// Missing -> `None`; not a member -> an error listing the allowed values.
pub fn opt_enum(
    table: &Table,
    key: &str,
    allowed: &[&str],
) -> Result<Option<String>, FieldError> {
    let Some(s) = opt_string(table, key)? else {
        return Ok(None);
    };
    if allowed.contains(&s.as_str()) {
        Ok(Some(s))
    } else {
        Err(FieldError::key(
            key,
            format!(
                "must be one of {} (got '{s}')",
                allowed
                    .iter()
                    .map(|a| format!("\"{a}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ))
    }
}

/// The TOML type name as it appears in error messages.
pub fn type_name(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

fn type_error(key: &str, expected: &str, got: &toml::Value) -> FieldError {
    FieldError::key(key, format!("must be {expected}, got {}", type_name(got)))
}

impl FieldError {
    /// Internal: prepend context to the message (used by `opt_string_list` so
    /// a bad element still reads as an array-shaped complaint).
    fn with_msg_prefix(mut self, prefix: String) -> Self {
        self.msg = format!("{prefix}{}", self.msg);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(src: &str) -> Table {
        src.parse::<toml::Value>()
            .expect("test toml")
            .as_table()
            .expect("table")
            .clone()
    }

    #[test]
    fn strings_missing_present_and_wrong_typed() {
        let table = t("a = \"x\"\nb = 3\n");
        assert_eq!(opt_string(&table, "a").unwrap(), Some("x".into()));
        assert_eq!(opt_string(&table, "missing").unwrap(), None);
        let e = opt_string(&table, "b").unwrap_err();
        assert_eq!(e.key, "b");
        assert_eq!(e.msg, "must be a string, got an integer");
    }

    #[test]
    fn blank_strings_are_rejected_by_the_nonempty_accessors() {
        let table = t("label = \"   \"\n");
        let e = opt_nonempty_string(&table, "label").unwrap_err();
        assert_eq!(e.to_string(), "key `label`: must be a non-empty string");
        // ...but plain opt_string still returns them verbatim.
        assert_eq!(opt_string(&table, "label").unwrap(), Some("   ".into()));
    }

    #[test]
    fn req_string_names_the_missing_key() {
        let e = req_string(&t(""), "description").unwrap_err();
        assert_eq!(
            e.to_string(),
            "key `description`: is required and must be a non-empty string"
        );
    }

    #[test]
    fn bools_and_ints() {
        let table = t("on = true\nn = 5\nneg = -1\n");
        assert!(opt_bool(&table, "on", false).unwrap());
        assert!(!opt_bool(&table, "absent", false).unwrap());
        assert_eq!(opt_u64(&table, "n", 60).unwrap(), 5);
        assert_eq!(opt_u64(&table, "absent", 60).unwrap(), 60);
        assert_eq!(
            opt_u64(&table, "neg", 60).unwrap_err().msg,
            "must be a non-negative integer"
        );
        assert_eq!(
            opt_bool(&table, "n", false).unwrap_err().msg,
            "must be a boolean, got an integer"
        );
    }

    #[test]
    fn string_lists_report_the_offending_index() {
        let table = t("argv = [\"git\", 3]\nok = [\"a\", \"b\"]\n");
        assert_eq!(string_list(&table, "ok").unwrap(), vec!["a", "b"]);
        assert!(string_list(&table, "absent").unwrap().is_empty());
        let e = opt_string_list(&table, "argv").unwrap_err();
        assert_eq!(e.key, "argv[1]");
        assert_eq!(
            e.msg,
            "argv must be an array of strings: must be a string, got an integer"
        );
    }

    #[test]
    fn req_string_list_rejects_missing_and_empty() {
        assert_eq!(
            req_string_list(&t("argv = []\n"), "argv").unwrap_err().msg,
            "must not be empty"
        );
        assert_eq!(
            req_string_list(&t(""), "argv").unwrap_err().msg,
            "is required and must be a non-empty array of strings"
        );
    }

    #[test]
    fn tables_and_table_arrays() {
        let table = t("[tools]\nallow = [\"Bash\"]\n\n[[args]]\nname = \"a\"\n\n[[args]]\nname = \"b\"\n");
        assert!(opt_table(&table, "tools").unwrap().is_some());
        assert!(opt_table(&table, "absent").unwrap().is_none());
        let args = table_array(&table, "args").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[1].get("name").unwrap().as_str(), Some("b"));
        assert!(table_array(&table, "absent").unwrap().is_empty());
        assert_eq!(
            table_array(&t("args = [1]\n"), "args").unwrap_err().key,
            "args[0]"
        );
    }

    #[test]
    fn string_map_is_deterministic_and_type_checked() {
        // Sorted, not file order — see the note on `string_map`.
        let table = t("[env]\nZ = \"1\"\nA = \"2\"\n");
        let env = string_map(&table, "env").unwrap();
        assert_eq!(env.keys().collect::<Vec<_>>(), vec!["A", "Z"]);
        assert_eq!(env["Z"], "1");
        assert!(string_map(&t(""), "env").unwrap().is_empty());
        let e = string_map(&t("[env]\nX = 1\n"), "env").unwrap_err();
        assert_eq!(e.key, "env.X");
    }

    #[test]
    fn opt_enum_lists_the_allowed_values() {
        let allowed = &["low", "medium", "high"];
        assert_eq!(
            opt_enum(&t("effort = \"high\"\n"), "effort", allowed).unwrap(),
            Some("high".into())
        );
        assert_eq!(opt_enum(&t(""), "effort", allowed).unwrap(), None);
        let e = opt_enum(&t("effort = \"max\"\n"), "effort", allowed).unwrap_err();
        assert_eq!(
            e.to_string(),
            "key `effort`: must be one of \"low\", \"medium\", \"high\" (got 'max')"
        );
    }

    #[test]
    fn root_table_rejects_a_non_table_document() {
        let v: toml::Value = toml::Value::Integer(3);
        let e = root_table(&v, "engine file").unwrap_err();
        assert_eq!(e.to_string(), "engine file must be a TOML table");
        assert!(e.key.is_empty());
    }

    #[test]
    fn unknown_keys_are_ignored_for_forward_compatibility() {
        let table = t("description = \"x\"\nfuture_key = { deeply = \"nested\" }\n");
        assert_eq!(req_string(&table, "description").unwrap(), "x");
    }
}
