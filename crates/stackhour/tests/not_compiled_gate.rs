#![cfg(not(all(feature = "tracker", feature = "agent", feature = "control")))]
//! The compile-time half of the module gate (Layer 1), driven through the
//! BUILT binary.
//!
//! This file only exists in a REDUCED build: with every feature on there is
//! nothing to refuse, and the whole point is to prove that a verb whose arm
//! was `#[cfg]`-ed out of the dispatch match is refused with exit 2 and its
//! own message rather than silently falling through to the exit-0 usage
//! banner. That fall-through is the single failure mode the top-of-`main`
//! gate exists to prevent, and only the real executable can demonstrate it.
//!
//! DELIBERATE DIVERGENCE (no Node original): the Node CLI has no notion of
//! modules or of a build that omits verbs.
//!
//! SAFETY: every run gets `env_clear()` and a throwaway HOME, so nothing here
//! reads the developer's own `~/.config/stackhour/config.json` — and no
//! control daemon verb is ever spawned.

use std::process::{Command, Output};
use tempfile::TempDir;

/// The binary under test. `CARGO_BIN_EXE_*` is resolved by cargo at compile
/// time and always points at the binary built for THIS test run — the
/// hand-rolled `current_exe()`-walking form can silently reuse a stale one.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_stackhour")
}

fn run(args: &[&str]) -> Output {
    let home = TempDir::new().unwrap();
    Command::new(bin())
        .args(args)
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("the stackhour binary must be executable")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("the process exited normally")
}

/// The modules this build does NOT have, paired with a verb that belongs to
/// each. Built from `cfg!` rather than from the binary's own report so the
/// test would still catch a `compiled_modules()` that lies.
///
/// `control` is represented by the bare verb: it needs no sub-verb, touches no
/// network, and starts no poller even when the module IS compiled in.
fn missing_modules() -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    if !cfg!(feature = "tracker") {
        out.push(("tracker", "serve"));
    }
    if !cfg!(feature = "agent") {
        out.push(("agent", "agent"));
    }
    if !cfg!(feature = "control") {
        out.push(("control", "control"));
    }
    // The file-level `cfg` guarantees at least one.
    assert!(!out.is_empty(), "a reduced build has at least one module off");
    out
}

#[test]
fn a_verb_from_an_uncompiled_module_exits_two_with_the_compile_time_message() {
    for (module, verb) in missing_modules() {
        let out = run(&[verb]);
        assert_eq!(code(&out), 2, "{verb}: stderr={}", stderr(&out));
        let err = stderr(&out);
        assert!(
            err.contains("not compiled into this binary"),
            "{verb}: stderr={err}"
        );
        // Layer 1 names the Cargo feature to rebuild with, never the config
        // key — nobody must be sent to edit config.json to fix a binary.
        assert!(
            err.contains(&format!("--features {module}")),
            "{verb}: stderr={err}"
        );
        assert!(!err.contains("modules."), "{verb}: stderr={err}");
        assert_eq!(stdout(&out), "", "{verb} must write nothing to stdout");
    }
}

#[test]
fn an_uncompiled_verb_never_falls_through_to_the_usage_banner() {
    for (_, verb) in missing_modules() {
        let out = run(&[verb]);
        assert!(
            !stdout(&out).contains("usage: stackhour <command>"),
            "{verb} fell through to the default arm"
        );
        assert_ne!(code(&out), 0, "{verb} must not succeed");
    }
}

/// doctor belongs to no module and is the diagnostic of last resort, so it
/// keeps running no matter which features were left out.
#[test]
fn doctor_still_runs_in_a_reduced_build() {
    let out = run(&["doctor"]);
    assert!(
        stdout(&out).contains("Stackhour doctor "),
        "stdout={}",
        stdout(&out)
    );
    assert_ne!(code(&out), 2, "doctor must never report the gate's exit code");
}

/// The pinned exit-0 usage banner is not a module's verb and must survive
/// every feature combination, byte-for-byte body included.
#[test]
fn the_usage_banner_still_exits_zero_in_a_reduced_build() {
    let out = run(&[]);
    assert_eq!(code(&out), 0, "stderr={}", stderr(&out));
    assert!(
        stdout(&out).contains("usage: stackhour <command>"),
        "stdout={}",
        stdout(&out)
    );
    assert_eq!(stderr(&out), "", "the help path keeps stderr clean");
}
