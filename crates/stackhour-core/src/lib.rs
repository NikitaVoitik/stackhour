//! stackhour-core — shared, dependency-light foundation of the Rust rewrite.
//!
//! NO sqlite, NO http in this crate. JS-semantics helpers, storage paths,
//! atomic fs rituals, config load/merge, time parsing, pricing, git-based
//! project canonicalisation, machine tokens / enrollment, the Layer-2
//! config-directory registry, and the compile-time/runtime module gate
//! (`modules` — not to be confused with `registry`, the config-directory
//! registry).

use std::fmt;

pub mod config;
pub mod fsutil;
pub mod jsnum;
pub mod modules;
pub mod paths;
pub mod pricing;
pub mod project;
pub mod registry;
pub mod timeparse;
pub mod tokens;

pub use jsnum::*;

/// The user-facing Stackhour version. The release workflow reads the same
/// workspace package version, so binaries, release tags, doctor output, and
/// agent reports cannot drift.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build-info string carried in the agent health report's `nodeVersion` key
/// (JSON key name kept for parity; dashboard only displays it). The JS agent
/// reported `process.version` here; the Rust agent reports a fixed
/// `stackhour-rust/<VERSION>` string instead.
pub fn build_info() -> &'static str {
    concat!("stackhour-rust/", env!("CARGO_PKG_VERSION"))
}

/// Workspace-wide error type.
///
/// Error MESSAGES are part of the parity contract: fallible library functions
/// construct this from exact strings and must never Debug-print config values
/// (secret hygiene). `status_code` reproduces the JS `err.statusCode || 500`
/// convention used by the HTTP server's error mapping.
#[derive(Debug, Clone)]
pub struct Error {
    msg: String,
    status_code: Option<u16>,
}

impl Error {
    /// Build an error carrying an exact, parity-checked message.
    pub fn msg(msg: impl Into<String>) -> Self {
        Error {
            msg: msg.into(),
            status_code: None,
        }
    }

    /// Build an error carrying an HTTP status code (JS `err.statusCode`).
    pub fn with_status(msg: impl Into<String>, status_code: u16) -> Self {
        Error {
            msg: msg.into(),
            status_code: Some(status_code),
        }
    }

    /// The message exactly as it must be printed / serialized.
    pub fn message(&self) -> &str {
        &self.msg
    }

    /// `err.statusCode || 500` semantics.
    pub fn status_code(&self) -> u16 {
        self.status_code.unwrap_or(500)
    }

    /// Whether an explicit status code was attached.
    pub fn has_status(&self) -> bool {
        self.status_code.is_some()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::msg(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::msg(e.to_string())
    }
}

impl From<String> for Error {
    fn from(msg: String) -> Self {
        Error::msg(msg)
    }
}

impl From<&str> for Error {
    fn from(msg: &str) -> Self {
        Error::msg(msg)
    }
}

/// Workspace-wide result alias (default error = [`Error`]).
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_tracks_the_cargo_package() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn build_info_is_prefixed_and_tracks_version() {
        assert_eq!(build_info(), format!("stackhour-rust/{VERSION}"));
    }

    #[test]
    fn build_info_is_static() {
        // Signature guarantee used by the health report builder.
        let s: &'static str = build_info();
        assert!(s.starts_with("stackhour-rust/"));
    }

    #[test]
    fn error_message_roundtrip() {
        let e = Error::msg("dimension and value are required");
        assert_eq!(e.message(), "dimension and value are required");
        assert_eq!(e.to_string(), "dimension and value are required");
    }

    #[test]
    fn error_status_code_defaults_to_500() {
        // JS: `err.statusCode || 500`
        let e = Error::msg("boom");
        assert_eq!(e.status_code(), 500);
        assert!(!e.has_status());
    }

    #[test]
    fn error_with_status_carries_code() {
        let e = Error::with_status("body too large", 413);
        assert_eq!(e.status_code(), 413);
        assert!(e.has_status());
        assert_eq!(e.message(), "body too large");
    }

    #[test]
    fn error_from_string_conversions() {
        let e: Error = "expected array".into();
        assert_eq!(e.message(), "expected array");
        assert_eq!(e.status_code(), 500);

        let e: Error = String::from("invalid JSON").into();
        assert_eq!(e.message(), "invalid JSON");
    }

    #[test]
    fn error_from_io_error_uses_display() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let e: Error = io.into();
        assert_eq!(e.message(), "no such file");
        assert!(!e.has_status());
    }

    #[test]
    fn error_from_serde_json_error() {
        let parse_err = serde_json::from_str::<serde_json::Value>("{not json").unwrap_err();
        let e: Error = parse_err.into();
        assert!(!e.message().is_empty());
        assert_eq!(e.status_code(), 500);
    }

    #[test]
    fn error_is_std_error_and_clonable() {
        let e = Error::with_status("unauthorized", 401);
        let dyn_err: &dyn std::error::Error = &e;
        assert_eq!(dyn_err.to_string(), "unauthorized");
        let e2 = e.clone();
        assert_eq!(e2.status_code(), 401);
    }
}
