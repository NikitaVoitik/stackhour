//! The ported `optionValues` parser. Quirks are contract:
//!
//! ONLY `--name=value` is recognized; values collected in order; scalars are
//! last-wins; `--project-root` is repeatable and order-preserving; boolean
//! flags by exact membership; unknown args silently ignored; the
//! `--name value` space form is deliberately NOT recognized (test-pinned);
//! `--project-roots` must NOT prefix-match `--project-root`.

/// All values of `--name=…`, in order.
pub fn option_values(args: &[String], name: &str) -> Vec<String> {
    let _ = (args, name);
    todo!()
}

/// The last `--name=…` value (`.at(-1)` semantics).
pub fn last_option(args: &[String], name: &str) -> Option<String> {
    let _ = (args, name);
    todo!()
}

/// Exact-membership boolean flag (e.g. `--force`, `--json`, `--once`).
pub fn has_flag(args: &[String], flag: &str) -> bool {
    let _ = (args, flag);
    todo!()
}
