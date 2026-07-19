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
mod doctor;
mod doctor_checks;
mod init;
mod install;
mod status;
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

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let cmd = argv.get(1).map(String::as_str).unwrap_or("");
    // `process.argv.slice(3)` — every verb parses its own tail.
    let tail: Vec<String> = argv.iter().skip(2).cloned().collect();

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
        "token" => deferred("token", token::run_token(&tail)),
        "install" => deferred("install", install::run_install(&tail)),
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
        // Verbs that exist in the Node CLI but are not ported yet. Kept
        // distinct from the help path so we never silently claim parity.
        "agent" | "import-wakatime" | "bridge" => {
            eprintln!("stackhour: `{cmd}` is not implemented in the Rust port yet");
            ExitCode::FAILURE
        }
        // Node's `default:` case — an unknown verb (or none) prints usage and
        // exits 0. `loadConfig()` runs BEFORE the switch in cli.js, so a
        // corrupt config.json must fail here rather than print help.
        _ => {
            let paths = stackhour_core::paths::resolve_storage_paths_from_process_env();
            if let Err(err) = stackhour_core::config::load_config(&paths.config_path) {
                eprintln!("stackhour: {err}");
                return ExitCode::FAILURE;
            }
            print!("{HELP}");
            println!("config: {}", paths.config_path.display());
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HELP;

    /// The usage banner is a user-visible contract shared with the Node CLI.
    /// `tests-fixtures/help.txt` is a capture of `node bin/stackhour` with a
    /// trailing dynamic `config: <path>` line; HELP is everything before it.
    #[test]
    fn help_body_matches_the_node_fixture() {
        let fixture = include_str!("../../../tests-fixtures/help.txt");
        let body = &fixture[..fixture.rfind("config: ").expect("fixture has a config: line")];
        assert_eq!(HELP, body);
    }
}
