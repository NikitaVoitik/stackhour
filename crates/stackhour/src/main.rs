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

use std::process::ExitCode;

mod args;
#[cfg(feature = "bridge")]
mod bridge_migrate;
#[cfg(feature = "control")]
mod control;
#[cfg(feature = "control")]
mod control_install;
mod doctor;
mod doctor_checks;
mod init;
mod install;
#[cfg(feature = "tracker")]
mod status;
#[cfg(feature = "tracker")]
mod token;

/// The help text body (everything before the trailing dynamic
/// `config: <path>` line). Byte-exact vs tests-fixtures/help.txt.
const HELP: &str = concat!(
    "stackhour — self-hosted coding time tracker\n",
    "\n",
    "usage: stackhour <command>\n",
    "\n",
    "  serve             run the server (ingest API + dashboard) on this machine\n",
    "  agent [--once]    run the watcher agent (files, claude, codex, mac apps)\n",
    "  import-wakatime [--days=365]   backfill history from wakatime.com\n",
    "  status            print today's totals from the server\n",
    "  doctor [--json]   check config, inputs, database, server, and services\n",
    "  init server [--public-url=URL] [--project-root=PATH ...] [--install]\n",
    "                    configure the server and its local agent\n",
    "  init agent --enrollment=CODE [--project-root=PATH ...] [--install]\n",
    "                    enroll and optionally install an agent service\n",
    "  token create MACHINE [--force] [--raw] [--server-url=URL]\n",
    "                    enroll a machine and print its copy-paste command\n",
    "  token list                       list enrolled machines (never secrets)\n",
    "  token revoke MACHINE             revoke a machine token\n",
    "  data stats [--json]              inspect local database size and coverage\n",
    "  data export --output=FILE [--from=TIME] [--to=TIME] [--force]\n",
    "                                   atomically export JSONL\n",
    "  data prune --before=TIME [--confirm]\n",
    "                                   preview or confirm retention pruning\n",
    "  backup create [--output=FILE] [--force]\n",
    "                                   create and verify a consistent snapshot\n",
    "  backup verify FILE               integrity-check a backup\n",
    "  backup restore FILE [--confirm]  preview or restore, preserving old DB\n",
    "  install <server|agent>           install and start user service(s)\n",
    "  bridge install <coordinator|worker> [--reconfigure] [--no-start]\n",
    "                                   set up the Telegram Claude/Codex bridge\n",
    "  bridge doctor|status|restart <coordinator|worker>\n",
    "                                   operate the bridge service\n",
    "  control <hub|node>               run the control plane\n",
    "  control install <hub|node|ssh>   configure and install a control service\n",
    "\n",
);

/// Node's `try { runX(argv) } catch (err) { console.error(...); exitCode = 1 }`
/// wrapper: the message is prefixed with `stackhour <cmd>: ` and the exit code
/// is DEFERRED (the process still runs to completion), never an immediate
/// `process.exit`.
fn deferred(cmd: &str, result: stackhour_core::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("stackhour {cmd}: {}", err.message());
            ExitCode::FAILURE
        }
    }
}

/// The modules compiled into this binary — Layer 1.
///
/// The Cargo feature names and the `modules` config sub-keys are deliberately
/// the same three strings (`Module::name`), so one identifier names both
/// layers. A default build turns all three on and therefore resolves to
/// `ModuleSet::ALL`, which is exactly what the gate treats as "no opinion".
fn compiled_modules() -> stackhour_core::modules::ModuleSet {
    stackhour_core::modules::ModuleSet::new(
        cfg!(feature = "tracker"),
        cfg!(feature = "agent"),
        cfg!(feature = "bridge"),
    )
    .with_control(cfg!(feature = "control"))
}

/// Both gate layers plus the config file the runtime one came from, resolved
/// from the process environment. The ONLY place the ambient environment is
/// consulted for module state; everything downstream takes the context as an
/// argument so it stays testable in a tempdir.
fn gate_context() -> stackhour_core::modules::GateContext {
    let paths = stackhour_core::paths::resolve_storage_paths_from_process_env();
    stackhour_core::modules::GateContext::from_config_file(compiled_modules(), paths.config_path)
}

/// Layer 1 (compile-time) + Layer 2 (runtime) gate, run BEFORE the dispatch
/// match so a `#[cfg]`-removed arm can never fall through to the exit-0
/// usage banner.
///
/// DELIBERATE DIVERGENCE (no Node original). Two properties are load-bearing:
///   * verbs that map to no module (doctor, the help/default arm, unknown
///     verbs) return early WITHOUT touching the filesystem, so stderr stays
///     clean on the help path and doctor keeps surviving a corrupt config;
///   * the runtime read is LENIENT, so a corrupt config.json still fails
///     inside the verb with today's message and exit 1, not here.
fn module_gate(cmd: &str, tail: &[String]) -> Option<ExitCode> {
    use stackhour_core::modules;
    let sub = tail.first().map(String::as_str);
    modules::module_for(cmd, sub)?;
    let msg = gate_context().refusal(cmd, sub)?;
    eprintln!("{msg}");
    Some(ExitCode::from(modules::GATED_EXIT_CODE))
}

/// One `note:` line per module that is OFF at either layer, printed after the
/// `config: <path>` line of the usage banner.
///
/// DELIBERATE DIVERGENCE (no Node original). `HELP` stays a `const` and stays
/// byte-identical — filtering the pinned banner would break
/// `help_body_matches_the_node_fixture` and `tests-fixtures/help.txt`. Instead
/// the verbs stay listed and a note explains why some of them will not run.
///
/// Returns immediately when nothing is off, which is the prime constraint:
/// a default build reading a config with no `modules` key must print exactly
/// what it printed before modules existed. Everything goes to STDOUT; the
/// help path keeps stderr clean.
///
/// The config path is deliberately NOT repeated here — the `config: <path>`
/// line directly above already names the file the notes are talking about.
fn print_module_notes(runtime: stackhour_core::modules::ModuleSet) {
    use stackhour_core::modules::{Module, ModuleSet};
    let compiled = compiled_modules();
    if runtime == ModuleSet::ALL && compiled == ModuleSet::ALL {
        return;
    }
    println!();
    // Layer 1 first, exactly as `modules::gate` resolves it: a module that is
    // neither compiled nor enabled must send the reader to `cargo build`.
    for m in Module::ALL {
        if !compiled.contains(m) {
            println!(
                "note: the {} module was not compiled into this binary; its commands above exit 2.",
                m.name()
            );
        } else if !runtime.contains(m) {
            println!(
                "note: the {} module is disabled by \"{}\": false; its commands above exit 2.",
                m.name(),
                m.config_key()
            );
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let cmd = argv.get(1).map(String::as_str).unwrap_or("");
    // `process.argv.slice(3)` — every verb parses its own tail.
    let tail: Vec<String> = argv.iter().skip(2).cloned().collect();

    if let Some(code) = module_gate(cmd, &tail) {
        return code;
    }

    match cmd {
        // ---------------------------------------------------------------
        // Verbs Node dispatches BEFORE `loadConfig()`. These must keep
        // working when config.json is missing or corrupt — `init` in
        // particular exists precisely to create that file.
        // ---------------------------------------------------------------
        "doctor" => {
            let code = doctor::run_doctor(&tail);
            if code == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        "init" => deferred("init", init::run_init(&tail)),
        // No `#[cfg(not(...))]` twin arms: `module_gate` runs before this
        // match and has already returned exit 2 for any verb whose module is
        // not compiled in, so a removed arm can never fall through to `_` and
        // print the usage banner with exit 0.
        //
        // DELIBERATE DIVERGENCE: `status` and `token` link no tracker crate
        // and would compile fine without the feature. They are gated anyway
        // so that "tracker off" means the same thing at both layers.
        #[cfg(feature = "tracker")]
        "token" => deferred("token", token::run_token(&tail)),
        "install" => deferred("install", install::run_install(&tail)),
        #[cfg(feature = "tracker")]
        "data" | "backup" => {
            // These two DO need a config, but Node still dispatches them
            // before the shared `loadConfig()` so they own their own error
            // prefix; a load failure surfaces as `stackhour data: <msg>`.
            let paths = stackhour_core::paths::resolve_storage_paths_from_process_env();
            let result = stackhour_core::config::load_config(&paths.config_path).and_then(|cfg| {
                if cmd == "data" {
                    stackhour_store::data::run_data(&cfg, &tail)
                } else {
                    stackhour_store::backup::run_backup_cli(&cfg, &tail)
                }
            });
            deferred(cmd, result)
        }

        // ---------------------------------------------------------------
        // Verbs below the `const cfg = loadConfig()` line: a corrupt
        // config.json fails HERE rather than inside the verb.
        // ---------------------------------------------------------------
        #[cfg(feature = "tracker")]
        "serve" => {
            let paths = stackhour_core::paths::resolve_storage_paths_from_process_env();
            let cfg = match stackhour_core::config::load_config(&paths.config_path) {
                Ok(cfg) => cfg,
                Err(err) => {
                    eprintln!("stackhour serve: {err}");
                    return ExitCode::FAILURE;
                }
            };
            match stackhour_server::start_server(cfg) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("stackhour serve: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(feature = "tracker")]
        "status" => {
            let paths = stackhour_core::paths::resolve_storage_paths_from_process_env();
            let cfg = match stackhour_core::config::load_config(&paths.config_path) {
                Ok(cfg) => cfg,
                Err(err) => {
                    eprintln!("stackhour status: {err}");
                    return ExitCode::FAILURE;
                }
            };
            if status::run_status(&cfg) == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        #[cfg(feature = "agent")]
        "agent" => {
            let paths = stackhour_core::paths::resolve_storage_paths_from_process_env();
            let result = stackhour_core::config::load_config(&paths.config_path)
                .and_then(|cfg| stackhour_agent::run_agent(&cfg, args::has_flag(&tail, "--once")));
            match result {
                Ok(_report) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("stackhour agent: {}", err.message());
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(feature = "tracker")]
        "import-wakatime" => {
            let paths = stackhour_core::paths::resolve_storage_paths_from_process_env();
            // Node: `process.argv.find(a => a.startsWith('--days='))` — the
            // FIRST occurrence, not the last, then a raw `Number()` coercion
            // (so `--days=abc` yields NaN and imports nothing, exit 0).
            let days = match args::option_values(&tail, "days").first() {
                Some(raw) => stackhour_core::jsnum::js_number(&serde_json::Value::String(raw.clone())),
                None => 365.0,
            };
            let result = stackhour_core::config::load_config(&paths.config_path)
                .and_then(|cfg| stackhour_store::wakatime::import_wakatime(&cfg, days));
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("stackhour: {}", err.message());
                    ExitCode::FAILURE
                }
            }
        }
        // The bridge family. Hidden wire/daemon verbs are routed here;
        // everything else falls through to the operator CLI (cli.mjs
        // runBridgeCli): install/doctor/status/restart plus the usage banner.
        #[cfg(feature = "bridge")]
        "bridge" => match tail.first().map(String::as_str) {
            // The one bridge verb that is ported. It touches no network and
            // starts no poller, so it is safe to run beside the live Node
            // coordinator.
            Some("migrate") => bridge_migrate::run(&tail[1..]),
            // The two daemons. Both long-poll or long-run and never return;
            // `--runtime-dir <dir>` beats $STACKHOUR_BRIDGE_HOME, which beats
            // the default, matching cli.mjs.
            //
            // `bridge coordinator` opens a getUpdates long-poll against the
            // configured token. Two pollers on one token silently steal each
            // other's messages, so the Node coordinator MUST be stopped first
            // — `bridge migrate` says so on the way out.
            // The two halves of the Mac worker's on-disk protocol. Both are
            // invoked over SSH by the worker (directly, or through the
            // node shims the installer writes), touch no network, and are
            // safe to run beside the live Node coordinator: `claim` only
            // renames files the worker is entitled to take, `return` only
            // publishes a result the worker produced.
            Some(verb @ ("claim" | "return")) => {
                let paths = match runtime_dir_from(&tail[1..]) {
                    Ok(dir) => dir,
                    Err(msg) => {
                        eprintln!("stackhour bridge {verb}: {msg}");
                        return ExitCode::FAILURE;
                    }
                };
                let code = if verb == "claim" {
                    // `bridge claim [target]`: the optional positional names
                    // the target this worker claims for. No positional = the
                    // legacy claim-anything mode the live Node Mac worker
                    // drives through the claim.mjs shim.
                    let target = positional_after(&tail[1..]);
                    stackhour_bridge::jobs::run_claim(&paths, target.as_deref())
                } else {
                    // `argv[2]` in return.mjs: the first positional after the
                    // verb, ignoring the --runtime-dir pair. A missing id is
                    // exit 2 with the reference's own message.
                    let id = positional_after(&tail[1..]).unwrap_or_default();
                    stackhour_bridge::jobs::run_return(&paths, &id)
                };
                ExitCode::from(code as u8)
            }
            Some(role @ ("coordinator" | "worker")) => {
                let paths = match runtime_dir_from(&tail[1..]) {
                    Ok(dir) => dir,
                    Err(msg) => {
                        eprintln!("stackhour bridge {role}: {msg}");
                        return ExitCode::FAILURE;
                    }
                };
                if role == "coordinator" {
                    stackhour_bridge::coordinator::run_coordinator(&paths)
                } else {
                    stackhour_bridge::worker::run_worker(&paths)
                }
            }
            // The one-shot notifier (fully ported + tested in tgsend.rs).
            // Node ran it as a standalone script, so its args are everything
            // after the verb.
            Some("tg-send") => ExitCode::from(stackhour_bridge::tgsend::run_tg_send(&tail[1..]) as u8),
            // install | doctor | status | restart, plus -h/--help and the
            // usage-on-stderr exit(1) for anything unknown — exactly what
            // cli.js hands to `runBridgeCli(process.argv.slice(3))`.
            _ => ExitCode::from(stackhour_bridge::installer::run_bridge_cli(&tail) as u8),
        },
        #[cfg(feature = "control")]
        "control" => {
            if tail.first().map(String::as_str) == Some("install") {
                return deferred("control install", control_install::run(&tail[1..]));
            }
            let paths = stackhour_core::paths::resolve_storage_paths_from_process_env();
            let result = stackhour_core::config::load_config(&paths.config_path)
                .and_then(|cfg| control::run(&tail, &cfg));
            deferred("control", result)
        }
        // Node's `default:` case — an unknown verb (or none) prints usage and
        // exits 0. `loadConfig()` runs BEFORE the switch in cli.js, so a
        // corrupt config.json must fail here rather than print help.
        _ => {
            let paths = stackhour_core::paths::resolve_storage_paths_from_process_env();
            // The Ok value is BOUND rather than discarded only so the module
            // notes can read `cfg.modules`. The error branch is byte-identical
            // to the `if let Err` form it replaces, so a corrupt config.json
            // still fails here with today's message and exit 1.
            let cfg = match stackhour_core::config::load_config(&paths.config_path) {
                Ok(cfg) => cfg,
                Err(err) => {
                    eprintln!("stackhour: {err}");
                    return ExitCode::FAILURE;
                }
            };
            print!("{HELP}");
            println!("config: {}", paths.config_path.display());
            // Module-aware help WITHOUT touching the pinned banner: the notes
            // print only when something is off, so the default build against a
            // config with no `modules` key emits byte-identical stdout and the
            // `ends_with("config: <path>")` contract holds.
            print_module_notes(cfg.modules);
            ExitCode::SUCCESS
        }
    }
}

/// Resolve the bridge runtime dir for a daemon verb.
///
/// `--runtime-dir <dir>` comes from argv so it is the caller's job, exactly as
/// in cli.mjs; everything below it is [`BridgePaths::resolve`]'s.
#[cfg(feature = "bridge")]
fn runtime_dir_from(args: &[String]) -> Result<std::path::PathBuf, String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--runtime-dir" {
            return match it.next() {
                Some(v) if !v.trim().is_empty() => Ok(std::path::PathBuf::from(v)),
                _ => Err("--runtime-dir needs a directory".to_string()),
            };
        }
    }
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    Ok(stackhour_bridge::BridgePaths::resolve(&|k| std::env::var(k).ok(), &home).runtime_dir)
}

/// The first positional argument, skipping the `--runtime-dir <dir>` pair.
///
/// `return.mjs` reads a bare `argv[2]`; the Rust verb additionally accepts the
/// runtime-dir flag either side of the id.
#[cfg(feature = "bridge")]
fn positional_after(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--runtime-dir" {
            let _ = it.next();
            continue;
        }
        if arg.starts_with("--") {
            continue;
        }
        return Some(arg.clone());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::HELP;
    // Split out from the `HELP` import so the pinned help-body test stays
    // ungated: these two helpers only exist in a bridge build.
    #[cfg(feature = "bridge")]
    use super::{positional_after, runtime_dir_from};

    #[cfg(feature = "bridge")]
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[cfg(feature = "bridge")]
    #[test]
    fn the_return_id_is_found_around_the_runtime_dir_flag() {
        assert_eq!(positional_after(&argv(&["abc"])).as_deref(), Some("abc"));
        assert_eq!(
            positional_after(&argv(&["--runtime-dir", "/tmp/rt", "abc"])).as_deref(),
            Some("abc")
        );
        assert_eq!(
            positional_after(&argv(&["abc", "--runtime-dir", "/tmp/rt"])).as_deref(),
            Some("abc")
        );
        assert_eq!(positional_after(&argv(&["--runtime-dir", "/tmp/rt"])), None);
        assert_eq!(positional_after(&argv(&[])), None);
    }

    #[cfg(feature = "bridge")]
    #[test]
    fn an_explicit_runtime_dir_beats_the_environment() {
        let args: Vec<String> = ["--runtime-dir", "/tmp/rt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(runtime_dir_from(&args).unwrap(), std::path::Path::new("/tmp/rt"));
    }

    /// A bare `--runtime-dir` must not silently resolve to the default and
    /// point a daemon at the wrong jobs directory.
    #[cfg(feature = "bridge")]
    #[test]
    fn a_runtime_dir_flag_without_a_value_is_an_error() {
        let args = vec!["--runtime-dir".to_string()];
        assert!(runtime_dir_from(&args).is_err());
        let args = vec!["--runtime-dir".to_string(), "  ".to_string()];
        assert!(runtime_dir_from(&args).is_err());
    }

    /// The usage banner is a user-visible contract shared with the Node CLI.
    /// `tests-fixtures/help.txt` is a capture of `node bin/stackhour` with a
    /// trailing dynamic `config: <path>` line; HELP is everything before it.
    #[test]
    fn help_body_matches_the_node_fixture() {
        let fixture = include_str!("../../../tests-fixtures/help.txt");
        let body = &fixture[..fixture.rfind("config: ").expect("fixture has a config: line")];
        assert_eq!(HELP, body);
    }

    /// Layer 1 has exactly one source of truth. If someone renames a feature
    /// or forgets to wire a new one into `compiled_modules`, a reduced build
    /// would silently report a module as present and then hit a `#[cfg]`-ed
    /// away arm — the one failure mode the gate exists to prevent.
    #[test]
    fn the_compiled_set_reflects_the_cargo_features() {
        let set = super::compiled_modules();
        assert_eq!(set.tracker, cfg!(feature = "tracker"));
        assert_eq!(set.agent, cfg!(feature = "agent"));
        assert_eq!(set.bridge, cfg!(feature = "bridge"));
        assert_eq!(set.control, cfg!(feature = "control"));
    }

    /// The prime constraint, stated as a test: a default build has every
    /// module compiled in, so Layer 1 never refuses anything and the binary
    /// behaves exactly as it did before features existed.
    #[cfg(all(
        feature = "tracker",
        feature = "agent",
        feature = "bridge",
        feature = "control"
    ))]
    #[test]
    fn a_default_build_compiles_in_every_module() {
        assert_eq!(super::compiled_modules(), stackhour_core::modules::ModuleSet::ALL);
    }
}
