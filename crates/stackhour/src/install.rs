//! `stackhour install <server|agent>` — systemd/launchd user services.
//!
//! Unit text byte-exact (systemdQuote with newline rejection, RestartSec 5
//! server / 10 agent, PATH Environment embedding the running binary's dir).
//! launchd plist XML-escaped, label com.stackhour.agent, log
//! /tmp/stackhour-agent.log. Executable = <repoRoot>/bin/stackhour derived
//! from the running binary + existence check (units must keep pointing at
//! the stable bin/ path — transition risk). Atomic 0644 unit writes; linux
//! systemctl daemon-reload + enable --now pair; darwin agent-only
//! bootout(ignored)/bootstrap/enable/kickstart order with the gui/<uid>
//! domain. `runInstall('server')` installs BOTH roles with exact wording.

use stackhour_core::Result;
use std::path::{Path, PathBuf};

/// What one role install did.
#[derive(Debug, Clone)]
pub struct Installed {
    pub role: String,
    /// The written unit/plist path.
    pub unit_path: PathBuf,
}

/// The `stackhour install <server|agent>` CLI.
pub fn run_install(args: &[String]) -> Result<()> {
    let _ = args;
    todo!()
}

/// Render the systemd unit for a role (byte-exact).
pub fn systemd_unit(role: &str, exe: &Path, path_dir: &Path) -> Result<String> {
    let _ = (role, exe, path_dir);
    todo!()
}

/// Render the launchd plist (byte-exact, XML-escaped).
pub fn launchd_plist(exe: &Path, path_dir: &Path) -> String {
    let _ = (exe, path_dir);
    todo!()
}

/// Install + start one role's service.
pub fn install_service(role: &str) -> Result<Installed> {
    let _ = role;
    todo!()
}
