//! stackhour — the single binary; verb dispatch with the parity-critical
//! config-load ordering.
//!
//! doctor/init/token/data/backup/install/bridge dispatch WITHOUT loading
//! config; serve/agent/import-wakatime/status/help load config FIRST — so a
//! corrupt config.json still crashes the help path, exactly like today.
//! Errors print `stackhour <cmd>: <message>` to stderr with a deferred
//! exit-code-1 (vs status's immediate exit(1)). Hidden verbs
//! (coordinator/worker/claim/return/tg-send) route to the bridge crate.
//! stderr stays clean on success. NO tokio here — the server crate builds
//! its own runtime.

// Scaffold phase: the bin modules are not yet wired into `main`'s dispatch,
// so everything reads as dead code. REMOVE this allow when implementing main.
#![allow(dead_code)]

use std::process::ExitCode;

mod args;
mod doctor;
mod doctor_checks;
mod init;
mod install;
mod status;

/// The help text body (everything before the trailing dynamic
/// `config: <path>` line). Byte-exact vs tests-fixtures/help.txt.
const HELP: &str = "REPLACED-BY-IMPLEMENTATION: pin to tests-fixtures/help.txt";

fn main() -> ExitCode {
    todo!()
}
