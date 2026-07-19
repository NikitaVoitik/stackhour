//! `ArgSpec` — the argument schema shared by commands (`commands/*.toml`) and
//! skills (`skills/<name>/skill.toml`), plus the binder that turns a raw
//! Telegram argument string into named values.
//!
//! Schema (`[[args]]`, in declaration order):
//!
//! ```toml
//! [[args]]
//! name        = "env"        # required; placeholder key -> {{env}}
//! required    = true         # default false
//! default     = "staging"    # default ""
//! choices     = ["staging", "prod"]   # default [] = unconstrained
//! rest        = false        # consumes all remaining words; at most one,
//!                            # and it must be the LAST arg
//! description = "target environment"  # used in the generated usage line
//! ```
//!
//! Binding is POSITIONAL and whitespace-split, which is what a Telegram user
//! actually types (`/deploy prod skip smoke tests`). A `rest` argument takes
//! the untouched remainder of the line INCLUDING internal whitespace, so
//! prose survives. `{{args}}` — the raw, untouched argument string — remains
//! available to every template regardless of the spec, which is what keeps
//! spec-less commands byte-compatible with today's behaviour.
//!
//! Binding never panics and never partially applies: it returns either a
//! complete map (every declared name present, defaults filled in) or a
//! [`FieldError`] whose key is the offending argument name.

use indexmap::IndexMap;

use super::error::FieldError;
use super::toml_util::{self, Table};

/// One positional argument of a command or skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgSpec {
    pub name: String,
    pub required: bool,
    pub default: String,
    pub choices: Vec<String>,
    /// Consumes the remainder of the line verbatim.
    pub rest: bool,
    pub description: String,
}

impl ArgSpec {
    /// A plain optional argument with no constraints (test/builder helper).
    pub fn new(name: impl Into<String>) -> Self {
        ArgSpec {
            name: name.into(),
            required: false,
            default: String::new(),
            choices: Vec::new(),
            rest: false,
            description: String::new(),
        }
    }

    /// Parse one `[[args]]` entry. `index` only appears in error keys.
    pub fn from_toml(table: &Table, index: usize) -> Result<Self, FieldError> {
        let at = |e: FieldError| e.under(&format!("args[{index}]"));

        let name = toml_util::req_string(table, "name").map_err(at)?;
        if !valid_arg_name(&name) {
            return Err(at(FieldError::key(
                "name",
                format!(
                    "'{name}' is not a valid argument name (lowercase letters, digits, '_' and '-'; must start with a letter)"
                ),
            )));
        }
        let required = toml_util::opt_bool(table, "required", false).map_err(at)?;
        let default = toml_util::opt_string(table, "default")
            .map_err(at)?
            .unwrap_or_default();
        let choices = toml_util::string_list(table, "choices").map_err(at)?;
        let rest = toml_util::opt_bool(table, "rest", false).map_err(at)?;
        let description = toml_util::opt_string(table, "description")
            .map_err(at)?
            .unwrap_or_default();

        if required && !default.is_empty() {
            return Err(at(FieldError::key(
                "default",
                "must not be set on a required argument",
            )));
        }
        if !choices.is_empty() && !default.is_empty() && !choices.contains(&default) {
            return Err(at(FieldError::key(
                "default",
                format!("'{default}' is not one of choices ({})", choices.join(", ")),
            )));
        }
        if rest && !choices.is_empty() {
            return Err(at(FieldError::key(
                "choices",
                "cannot be combined with rest = true (a rest argument takes free text)",
            )));
        }

        Ok(ArgSpec {
            name,
            required,
            default,
            choices,
            rest,
            description,
        })
    }
}

/// Parse the whole `[[args]]` array of a manifest and validate the spec as a
/// whole: unique names, at most one `rest`, `rest` last, and no required
/// argument after an optional one (which could never be satisfied
/// positionally).
pub fn parse_arg_specs(table: &Table) -> Result<Vec<ArgSpec>, FieldError> {
    let entries = toml_util::table_array(table, "args")?;
    let mut specs: Vec<ArgSpec> = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        let spec = ArgSpec::from_toml(entry, i)?;
        if specs.iter().any(|s| s.name == spec.name) {
            return Err(FieldError::key(
                format!("args[{i}].name"),
                format!("duplicate argument name '{}'", spec.name),
            ));
        }
        specs.push(spec);
    }
    validate_arg_specs(&specs)?;
    Ok(specs)
}

/// Whole-spec invariants, separated so callers constructing specs in code
/// (embedded defaults, tests) get the same checks.
pub fn validate_arg_specs(specs: &[ArgSpec]) -> Result<(), FieldError> {
    for (i, spec) in specs.iter().enumerate() {
        if spec.rest && i + 1 != specs.len() {
            return Err(FieldError::key(
                format!("args[{i}].rest"),
                format!(
                    "only the LAST argument may set rest = true ('{}' is followed by '{}')",
                    spec.name,
                    specs[i + 1].name
                ),
            ));
        }
        if spec.required && i > 0 && !specs[i - 1].required {
            return Err(FieldError::key(
                format!("args[{i}].required"),
                format!(
                    "required argument '{}' cannot follow optional argument '{}' (arguments bind positionally)",
                    spec.name,
                    specs[i - 1].name
                ),
            ));
        }
    }
    Ok(())
}

/// Bind a raw argument string to the spec.
///
/// - Splits on ASCII whitespace; leading/trailing whitespace is ignored.
/// - A `rest` argument takes the remainder of the line VERBATIM (internal
///   whitespace preserved), so prose arguments survive intact.
/// - Missing optional arguments get their `default` (possibly `""`), so every
///   declared name is always present in the result and templates never leak a
///   raw `{{placeholder}}`.
/// - Extra words beyond the spec are an error UNLESS the spec is empty, in
///   which case anything is accepted (this is the legacy, spec-less path).
///
/// The returned map is in declaration order and additionally carries `args`,
/// the raw untouched string, for backward compatibility with templates that
/// predate the argument spec.
pub fn bind_args(specs: &[ArgSpec], raw: &str) -> Result<IndexMap<String, String>, FieldError> {
    let mut out: IndexMap<String, String> = IndexMap::new();
    let trimmed = raw.trim();

    if specs.is_empty() {
        out.insert("args".to_string(), trimmed.to_string());
        return Ok(out);
    }

    // Positional walk over `trimmed`, tracking a byte cursor so the `rest`
    // argument can take the remainder verbatim rather than a re-joined split.
    let mut cursor = 0usize;
    for (i, spec) in specs.iter().enumerate() {
        let remainder = trimmed[cursor..].trim_start();
        if spec.rest {
            let value = if remainder.is_empty() {
                spec.default.clone()
            } else {
                remainder.to_string()
            };
            if spec.required && value.is_empty() {
                return Err(missing(spec, i));
            }
            out.insert(spec.name.clone(), value);
            cursor = trimmed.len();
            continue;
        }
        let word = remainder.split_whitespace().next().unwrap_or("");
        if word.is_empty() {
            if spec.required {
                return Err(missing(spec, i));
            }
            out.insert(spec.name.clone(), spec.default.clone());
            cursor = trimmed.len();
            continue;
        }
        if !spec.choices.is_empty() && !spec.choices.contains(&word.to_string()) {
            return Err(FieldError::key(
                spec.name.clone(),
                format!(
                    "'{word}' is not a valid value for '{}' (expected one of {})",
                    spec.name,
                    spec.choices.join(", ")
                ),
            ));
        }
        out.insert(spec.name.clone(), word.to_string());
        // Advance past this word within `trimmed`.
        let offset = trimmed.len() - remainder.len();
        cursor = offset + word.len();
    }

    let leftover = trimmed[cursor.min(trimmed.len())..].trim();
    if !leftover.is_empty() {
        return Err(FieldError::key(
            "args",
            format!(
                "unexpected extra argument '{}' (usage: {})",
                leftover.split_whitespace().next().unwrap_or(leftover),
                usage(specs)
            ),
        ));
    }

    out.insert("args".to_string(), trimmed.to_string());
    Ok(out)
}

/// The human usage line shown in `/help` and in binding errors:
/// `<env> [note...]`.
pub fn usage(specs: &[ArgSpec]) -> String {
    if specs.is_empty() {
        return "<no arguments>".to_string();
    }
    specs
        .iter()
        .map(|s| {
            let ellipsis = if s.rest { "..." } else { "" };
            if s.required {
                format!("<{}{ellipsis}>", s.name)
            } else {
                format!("[{}{ellipsis}]", s.name)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn missing(spec: &ArgSpec, index: usize) -> FieldError {
    let mut msg = format!("missing required argument '{}'", spec.name);
    if !spec.description.is_empty() {
        msg = format!("{msg} ({})", spec.description);
    }
    let _ = index;
    FieldError::key(spec.name.clone(), msg)
}

fn valid_arg_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(src: &str) -> Table {
        src.parse::<toml::Value>()
            .expect("toml")
            .as_table()
            .expect("table")
            .clone()
    }

    fn specs(src: &str) -> Vec<ArgSpec> {
        parse_arg_specs(&t(src)).expect("valid spec")
    }

    // ---- parsing ----

    #[test]
    fn parses_a_realistic_spec_in_declaration_order() {
        let s = specs(
            r#"
[[args]]
name = "env"
required = true
choices = ["staging", "prod"]
description = "target environment"

[[args]]
name = "note"
rest = true
description = "free-text note"
"#,
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "env");
        assert!(s[0].required);
        assert_eq!(s[0].choices, vec!["staging", "prod"]);
        assert_eq!(s[0].description, "target environment");
        assert!(s[1].rest);
        assert!(!s[1].required);
        assert_eq!(usage(&s), "<env> [note...]");
    }

    #[test]
    fn no_args_table_yields_an_empty_spec() {
        assert!(parse_arg_specs(&t("description = \"x\"\n")).unwrap().is_empty());
        assert_eq!(usage(&[]), "<no arguments>");
    }

    #[test]
    fn defaults_are_conservative() {
        let s = specs("[[args]]\nname = \"x\"\n");
        assert_eq!(s[0], ArgSpec::new("x"));
    }

    // ---- schema validation errors name file+key ----

    #[test]
    fn missing_name_is_an_error_naming_the_index() {
        let e = parse_arg_specs(&t("[[args]]\nrequired = true\n")).unwrap_err();
        assert_eq!(
            e.in_file("commands/deploy.toml").to_string(),
            "commands/deploy.toml: key `args[0].name`: is required and must be a non-empty string"
        );
    }

    #[test]
    fn invalid_name_is_rejected() {
        let e = parse_arg_specs(&t("[[args]]\nname = \"Env Name\"\n")).unwrap_err();
        assert_eq!(e.key, "args[0].name");
        assert!(e.msg.contains("is not a valid argument name"), "got {}", e.msg);
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let e =
            parse_arg_specs(&t("[[args]]\nname = \"a\"\n\n[[args]]\nname = \"a\"\n")).unwrap_err();
        assert_eq!(e.key, "args[1].name");
        assert_eq!(e.msg, "duplicate argument name 'a'");
    }

    #[test]
    fn rest_must_be_last() {
        let e = parse_arg_specs(&t(
            "[[args]]\nname = \"a\"\nrest = true\n\n[[args]]\nname = \"b\"\n",
        ))
        .unwrap_err();
        assert_eq!(e.key, "args[0].rest");
        assert!(e.msg.contains("only the LAST argument"), "got {}", e.msg);
    }

    #[test]
    fn required_cannot_follow_optional() {
        let e = parse_arg_specs(&t(
            "[[args]]\nname = \"a\"\n\n[[args]]\nname = \"b\"\nrequired = true\n",
        ))
        .unwrap_err();
        assert_eq!(e.key, "args[1].required");
        assert!(e.msg.contains("cannot follow optional"), "got {}", e.msg);
    }

    #[test]
    fn default_must_be_a_member_of_choices() {
        let e = parse_arg_specs(&t(
            "[[args]]\nname = \"env\"\nchoices = [\"a\", \"b\"]\ndefault = \"c\"\n",
        ))
        .unwrap_err();
        assert_eq!(e.key, "args[0].default");
        assert_eq!(e.msg, "'c' is not one of choices (a, b)");
    }

    #[test]
    fn required_plus_default_is_contradictory() {
        let e = parse_arg_specs(&t(
            "[[args]]\nname = \"env\"\nrequired = true\ndefault = \"x\"\n",
        ))
        .unwrap_err();
        assert_eq!(e.key, "args[0].default");
    }

    #[test]
    fn rest_cannot_have_choices() {
        let e = parse_arg_specs(&t(
            "[[args]]\nname = \"n\"\nrest = true\nchoices = [\"a\"]\n",
        ))
        .unwrap_err();
        assert_eq!(e.key, "args[0].choices");
    }

    #[test]
    fn wrong_typed_field_reports_the_expected_type() {
        let e = parse_arg_specs(&t("[[args]]\nname = \"a\"\nrequired = \"yes\"\n")).unwrap_err();
        assert_eq!(e.key, "args[0].required");
        assert_eq!(e.msg, "must be a boolean, got a string");
    }

    // ---- binding ----

    #[test]
    fn empty_spec_passes_the_raw_string_through_as_args() {
        let bound = bind_args(&[], "  anything at   all  ").unwrap();
        assert_eq!(bound.len(), 1);
        assert_eq!(bound["args"], "anything at   all");
    }

    #[test]
    fn binds_positionally_and_keeps_rest_verbatim() {
        let s = specs(
            "[[args]]\nname = \"env\"\nrequired = true\n\n[[args]]\nname = \"note\"\nrest = true\n",
        );
        let bound = bind_args(&s, "prod  ship   it now").unwrap();
        assert_eq!(bound["env"], "prod");
        assert_eq!(bound["note"], "ship   it now");
        // The raw string is still available under `args` for back compat.
        assert_eq!(bound["args"], "prod  ship   it now");
        assert_eq!(bound.keys().collect::<Vec<_>>(), vec!["env", "note", "args"]);
    }

    #[test]
    fn missing_optionals_get_their_defaults_and_are_always_present() {
        let s = specs("[[args]]\nname = \"env\"\ndefault = \"staging\"\n\n[[args]]\nname = \"tag\"\n");
        let bound = bind_args(&s, "").unwrap();
        assert_eq!(bound["env"], "staging");
        assert_eq!(bound["tag"], "");
    }

    #[test]
    fn missing_required_argument_is_an_error_naming_it() {
        let s = specs("[[args]]\nname = \"env\"\nrequired = true\ndescription = \"where to deploy\"\n");
        let e = bind_args(&s, "   ").unwrap_err();
        assert_eq!(e.key, "env");
        assert_eq!(
            e.msg,
            "missing required argument 'env' (where to deploy)"
        );
    }

    #[test]
    fn missing_required_rest_argument_is_an_error() {
        let s = specs("[[args]]\nname = \"text\"\nrequired = true\nrest = true\n");
        assert_eq!(bind_args(&s, "").unwrap_err().key, "text");
        assert_eq!(bind_args(&s, "hello world").unwrap()["text"], "hello world");
    }

    #[test]
    fn choices_are_enforced_at_bind_time() {
        let s = specs("[[args]]\nname = \"env\"\nchoices = [\"staging\", \"prod\"]\n");
        assert_eq!(bind_args(&s, "prod").unwrap()["env"], "prod");
        let e = bind_args(&s, "wat").unwrap_err();
        assert_eq!(e.key, "env");
        assert_eq!(
            e.msg,
            "'wat' is not a valid value for 'env' (expected one of staging, prod)"
        );
    }

    #[test]
    fn extra_words_beyond_the_spec_are_rejected_with_a_usage_line() {
        let s = specs("[[args]]\nname = \"env\"\nrequired = true\n");
        let e = bind_args(&s, "prod extra stuff").unwrap_err();
        assert_eq!(e.key, "args");
        assert_eq!(
            e.msg,
            "unexpected extra argument 'extra' (usage: <env>)"
        );
    }

    #[test]
    fn multibyte_arguments_do_not_panic_on_byte_cursors() {
        let s = specs("[[args]]\nname = \"a\"\n\n[[args]]\nname = \"b\"\nrest = true\n");
        let bound = bind_args(&s, "привет мир и ещё").unwrap();
        assert_eq!(bound["a"], "привет");
        assert_eq!(bound["b"], "мир и ещё");
    }

    #[test]
    fn tabs_and_newlines_count_as_separators() {
        let s = specs("[[args]]\nname = \"a\"\n\n[[args]]\nname = \"b\"\nrest = true\n");
        let bound = bind_args(&s, "one\ttwo\nthree").unwrap();
        assert_eq!(bound["a"], "one");
        assert_eq!(bound["b"], "two\nthree");
    }
}
