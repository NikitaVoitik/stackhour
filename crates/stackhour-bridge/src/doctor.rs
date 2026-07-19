//! Bridge doctor / status / restart, split from installer.rs (line budget).
//!
//! Per-role checks with ✓/✗ output and exit code: the runtime analogue of
//! the node>=22 check (check name kept), config validity + mode, binaries
//! executable, runtime files present, systemd is-active / launchctl print,
//! ssh remote-helper probe using shell_quote. restart = the exact
//! systemctl/launchctl sequences. NEW: registry validation errors surfaced
//! as additional ✗/! lines AFTER the existing checks.

use std::path::Path;

/// Run `stackhour bridge doctor <role>`; returns the exit code.
pub fn run_doctor(role: &str, runtime_dir: &Path) -> i32 {
    let _ = (role, runtime_dir);
    todo!()
}

/// Run `stackhour bridge status|restart <role>` (the exact systemctl /
/// launchctl sequences); returns the exit code.
pub fn run_service_cmd(role: &str, verb: &str) -> i32 {
    let _ = (role, verb);
    todo!()
}
