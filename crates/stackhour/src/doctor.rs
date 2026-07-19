//! Doctor orchestration + output.
//!
//! `diagnose` runs the ordered check list built in doctor_checks.rs;
//! `print_doctor` renders text (✓ ! ✗ icons) or `--json` (exact shape); the
//! exit code is 1 ONLY when errors (not warns) exist. NEW: non-fatal
//! 'registry' checks (plus 'engine-<id>'/'agent-<name>'/'skill-<id>'/
//! 'command-<name>' entries) listing config-dir validation errors as warns
//! AFTER all existing checks, so current output ordering is preserved.

use stackhour_core::config::Config;

/// One check outcome. Names/statuses/messages are EXACT parity surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

/// ✓ / ! / ✗.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Error,
}

/// The full doctor report.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// Exit code: 1 ONLY when errors (not warns) exist.
    pub fn exit_code(&self) -> i32 {
        todo!()
    }
}

/// Doctor invocation options.
#[derive(Debug, Clone, Default)]
pub struct DoctorOpts {
    pub json: bool,
}

/// The `stackhour doctor [--json]` CLI; returns the exit code.
pub fn run_doctor(args: &[String]) -> i32 {
    let _ = args;
    todo!()
}

/// Run the ordered checks (config passed as Ok(&cfg) or Err(load-error
/// message), reproducing the JS behaviour of diagnosing a broken config).
pub fn diagnose(cfg: Result<&Config, &str>, opts: &DoctorOpts) -> Report {
    let _ = (cfg, opts);
    todo!()
}

/// Render the report as text or the exact `--json` shape.
pub fn print_doctor(report: &Report, json: bool) {
    let _ = (report, json);
    todo!()
}
