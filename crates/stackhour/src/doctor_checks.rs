//! The individual doctor checks with EXACT names/statuses/messages (split
//! from doctor.rs for the line budget). Each check fn stays private and is
//! unit-tested via tests-fixtures/.
//!
//! Inventory: 'node' (a fixed-ok runtime report keeping the check NAME
//! 'node' — flagged parity decision), 'sqlite' availability,
//! config/config-permissions (octal 3-pad, ' (recommend 600)', missing-file
//! warn without an ok line), data-dir ancestor walk-up, offline-queue byte
//! message in both ok/warn branches, token, project-roots vs project-root
//! asymmetry, claude/codex/zed input dirs, database quick_check, server-auth
//! (Bearer only when the token is set, 5s timeout, the possible SECOND
//! server-auth error entry preserved), agent-report / agent-version
//! (compares against stackhour_core::VERSION) / agent-queue / clock-skew
//! chain, dynamic watcher-<name> checks from the health report, services
//! (systemctl 'active' line counting incl. captured stdout on nonzero exit /
//! launchctl print).

use crate::doctor::{Check, DoctorOpts};
use stackhour_core::config::Config;

/// Build the full ordered check list.
pub fn all_checks(cfg: Result<&Config, &str>, opts: &DoctorOpts) -> Vec<Check> {
    let _ = (cfg, opts);
    todo!()
}
