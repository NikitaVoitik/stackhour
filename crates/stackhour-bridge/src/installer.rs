//! `stackhour bridge install <role>` + the bridge CLI dispatcher.
//!
//! Arg parsing (--runtime-dir/--reconfigure/--no-start/--non-interactive/
//! -h); interactive prompts (env-var-first non-interactive path with exact
//! required-var errors validated BEFORE any write; raw-mode secret echo '*'
//! with backspace; Ctrl-C -> 'Setup cancelled.'; find_executable PATH
//! search); config validation-before-write; timestamped 0600 backups.
//! Runtime install = copy the running stackhour binary into the runtime dir
//! + write claim.mjs/return.mjs/tg-send.mjs shims (0755): each shim is a
//! VALID NODE SCRIPT (child_process.spawnSync of the adjacent stackhour
//! binary with the matching hidden verb, stdio inherit, exit-code forward)
//! because the JS worker invokes `<remoteNode> <remoteDir>/claim.mjs` with
//! node as the interpreter — this keeps both mixed Node/Rust pairings
//! working. Unit/plist writes use config.rs render_* (ExecStart = the
//! installed stackhour binary with `bridge coordinator|worker`), then the
//! systemctl/launchctl sequences + linger note + plutil lint.

/// The `stackhour bridge …` CLI entry: dispatches
/// install|doctor|status|restart|coordinator|worker|claim|return|tg-send.
/// Returns the process exit code.
pub fn run_bridge_cli(args: &[String]) -> i32 {
    let _ = args;
    todo!()
}

/// The install verb itself (split for testability). Returns the exit code.
pub fn run_install(role: &str, args: &[String]) -> i32 {
    let _ = (role, args);
    todo!()
}
