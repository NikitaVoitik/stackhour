//! Doctor orchestration + output.
//!
//! `diagnose` runs the ordered check list built in doctor_checks.rs;
//! `print_doctor` renders text (✓ ! ✗ icons) or `--json` (exact shape); the
//! exit code is 1 ONLY when errors (not warns) exist.

use crate::args::has_flag;
use serde_json::{json, Value};
use stackhour_core::config::Config;
use std::io::Write;
use std::path::PathBuf;

/// One check outcome. Names/statuses/messages are EXACT parity surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

impl Check {
    pub fn new(name: &str, status: CheckStatus, message: impl Into<String>) -> Self {
        Check {
            name: name.to_string(),
            status,
            message: message.into(),
        }
    }
}

/// ✓ / ! / ✗.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Error,
}

impl CheckStatus {
    /// The wire name used in `--json` output.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Error => "error",
        }
    }

    /// The text-mode icon.
    pub fn icon(self) -> char {
        match self {
            CheckStatus::Ok => '✓',
            CheckStatus::Warn => '!',
            CheckStatus::Error => '✗',
        }
    }
}

/// The full doctor report.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// `ok` is false as soon as ANY check errored. Warnings never fail.
    pub fn ok(&self) -> bool {
        !self.checks.iter().any(|c| c.status == CheckStatus::Error)
    }

    /// Exit code: 1 ONLY when errors (not warns) exist.
    pub fn exit_code(&self) -> i32 {
        i32::from(!self.ok())
    }

    /// The exact `--json` document: `{ ok, version, checks: [...] }`.
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok(),
            "version": stackhour_core::VERSION,
            "checks": self.checks.iter().map(|c| json!({
                "name": c.name,
                "status": c.status.as_str(),
                "message": c.message,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Doctor invocation options.
#[derive(Debug, Clone)]
pub struct DoctorOpts {
    pub json: bool,
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub home: PathBuf,
    /// Skip the systemd/launchd probe (`checkServices: false`).
    pub check_services: bool,
    /// Skip every network probe. Not a Node option — it exists so tests can
    /// run the check list hermetically.
    pub check_server: bool,
    /// Override the Zed threads.db candidates.
    pub zed_db_paths: Option<Vec<PathBuf>>,
}

impl Default for DoctorOpts {
    fn default() -> Self {
        let storage = stackhour_core::paths::resolve_storage_paths_from_process_env();
        DoctorOpts {
            json: false,
            config_path: storage.config_path,
            data_dir: storage.data_dir,
            home: PathBuf::from(std::env::var("HOME").unwrap_or_default()),
            check_services: true,
            check_server: true,
            zed_db_paths: None,
        }
    }
}

/// Run the ordered checks (config passed as Ok(&cfg) or Err(load-error
/// message), reproducing the JS behaviour of diagnosing a broken config).
pub fn diagnose(cfg: Result<&Config, &str>, opts: &DoctorOpts) -> Report {
    Report {
        checks: crate::doctor_checks::all_checks(cfg, opts),
    }
}

/// Render the report as text or the exact `--json` shape.
pub fn print_doctor(report: &Report, json: bool) {
    let mut out = std::io::stdout();
    let _ = write_doctor(report, json, &mut out);
}

/// Injectable renderer (the output text is a parity contract).
pub fn write_doctor(report: &Report, json: bool, out: &mut dyn Write) -> std::io::Result<()> {
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&report.to_json())?)?;
        return Ok(());
    }
    writeln!(out, "Stackhour doctor {}", stackhour_core::VERSION)?;
    for check in &report.checks {
        writeln!(out, "{} {}: {}", check.status.icon(), check.name, check.message)?;
    }
    let warnings = report
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Warn)
        .count();
    let errors = report
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Error)
        .count();
    writeln!(out, "\n{errors} errors, {warnings} warnings")?;
    Ok(())
}

/// The `stackhour doctor [--json]` CLI; returns the exit code.
pub fn run_doctor(args: &[String]) -> i32 {
    let opts = DoctorOpts {
        json: has_flag(args, "--json"),
        ..Default::default()
    };
    // Node's diagnose() loads the config ITSELF and turns a load failure into
    // a 'config' error check rather than crashing — doctor is the one verb
    // that must survive a corrupt config.
    let loaded = stackhour_core::config::load_config(&opts.config_path);
    let report = match &loaded {
        Ok(cfg) => diagnose(Ok(cfg), &opts),
        Err(e) => diagnose(Err(e.message()), &opts),
    };
    print_doctor(&report, opts.json);
    report.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(statuses: &[CheckStatus]) -> Report {
        Report {
            checks: statuses
                .iter()
                .enumerate()
                .map(|(i, s)| Check::new(&format!("c{i}"), *s, "m"))
                .collect(),
        }
    }

    /// Warnings must never fail the exit code — only errors do.
    #[test]
    fn exit_code_is_one_only_for_errors() {
        assert_eq!(report(&[]).exit_code(), 0);
        assert_eq!(report(&[CheckStatus::Ok, CheckStatus::Warn]).exit_code(), 0);
        assert_eq!(report(&[CheckStatus::Warn, CheckStatus::Error]).exit_code(), 1);
    }

    #[test]
    fn text_output_uses_the_node_icons_and_summary_line() {
        let r = Report {
            checks: vec![
                Check::new("node", CheckStatus::Ok, "v22.0.0 (requires >=22)"),
                Check::new("config", CheckStatus::Warn, "not found: /x"),
                Check::new("database", CheckStatus::Error, "/db: broken"),
            ],
        };
        let mut out = Vec::new();
        write_doctor(&r, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with(&format!("Stackhour doctor {}\n", stackhour_core::VERSION)));
        assert!(text.contains("✓ node: v22.0.0 (requires >=22)\n"));
        assert!(text.contains("! config: not found: /x\n"));
        assert!(text.contains("✗ database: /db: broken\n"));
        assert!(text.ends_with("\n1 errors, 1 warnings\n"));
    }

    /// The `--json` document shape is consumed by scripts.
    #[test]
    fn json_output_has_the_documented_shape() {
        let r = report(&[CheckStatus::Warn]);
        let mut out = Vec::new();
        write_doctor(&r, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.ends_with('\n'));
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["version"], stackhour_core::VERSION);
        assert_eq!(parsed["checks"][0]["status"], "warn");
        assert_eq!(parsed["checks"][0]["name"], "c0");
        assert_eq!(parsed["checks"][0]["message"], "m");
    }

    #[test]
    fn json_ok_is_false_when_any_check_errored() {
        let r = report(&[CheckStatus::Ok, CheckStatus::Error]);
        assert_eq!(r.to_json()["ok"], false);
    }
}
