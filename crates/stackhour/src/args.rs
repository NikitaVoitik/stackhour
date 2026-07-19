//! The ported `optionValues` parser. Quirks are contract:
//!
//! ONLY `--name=value` is recognized; values collected in order; scalars are
//! last-wins; `--project-root` is repeatable and order-preserving; boolean
//! flags by exact membership; unknown args silently ignored; the
//! `--name value` space form is deliberately NOT recognized (test-pinned);
//! `--project-roots` must NOT prefix-match `--project-root`.

/// All values of `--name=…`, in order.
pub fn option_values(args: &[String], name: &str) -> Vec<String> {
    let prefix = format!("--{name}=");
    args.iter()
        .filter_map(|arg| arg.strip_prefix(&prefix).map(str::to_string))
        .collect()
}

/// The last `--name=…` value (`.at(-1)` semantics).
pub fn last_option(args: &[String], name: &str) -> Option<String> {
    option_values(args, name).pop()
}

/// Exact-membership boolean flag (e.g. `--force`, `--json`, `--once`).
pub fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn collects_repeated_values_in_order() {
        let args = argv(&["--project-root=/a", "--force", "--project-root=/b"]);
        assert_eq!(option_values(&args, "project-root"), vec!["/a", "/b"]);
    }

    #[test]
    fn last_option_is_last_wins_and_none_when_absent() {
        let args = argv(&["--port=1", "--port=2"]);
        assert_eq!(last_option(&args, "port").as_deref(), Some("2"));
        assert_eq!(last_option(&args, "host"), None);
    }

    /// `--name value` (space form) is NOT a value — Node only ever splits on
    /// `=`, so `--port 4041` leaves port unset and `4041` is ignored.
    #[test]
    fn space_separated_form_is_not_recognized() {
        let args = argv(&["--port", "4041"]);
        assert!(option_values(&args, "port").is_empty());
        assert_eq!(last_option(&args, "port"), None);
    }

    /// Regression: a longer option name must not be harvested by a shorter
    /// one. `startsWith('--project-root=')` never matches `--project-roots=`.
    #[test]
    fn longer_option_names_do_not_prefix_match() {
        let args = argv(&["--project-roots=/a"]);
        assert!(option_values(&args, "project-root").is_empty());
    }

    /// An explicitly empty value is a real (empty) value, not an absence.
    #[test]
    fn empty_value_is_preserved() {
        let args = argv(&["--host="]);
        assert_eq!(last_option(&args, "host").as_deref(), Some(""));
    }

    #[test]
    fn flags_match_exactly_not_by_prefix() {
        let args = argv(&["--force"]);
        assert!(has_flag(&args, "--force"));
        assert!(!has_flag(&args, "--forced"));
        assert!(!has_flag(&argv(&["--force=1"]), "--force"));
    }
}
