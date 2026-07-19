//! `stackhour bridge migrate` — argv parsing, exit codes, output.
//!
//! The module deliberately owns nothing but the CLI surface; the conversion
//! itself lives in [`stackhour_bridge::migrate`], which is where the tests
//! against the committed fixture live too.
//!
//! Exit codes, because a migration is the kind of thing people script:
//!
//! | code | meaning |
//! |------|---------|
//! | 0    | wrote the plan (or printed it, under `--dry-run`) |
//! | 1    | bad usage, unreadable source, or a failed write |
//! | 2    | destinations already exist — NOTHING was written |
//! | 3    | `--verify` found a discrepancy |

use stackhour_bridge::migrate::{self, LegacyConfig, MigrateOptions};
use stackhour_bridge::BridgePaths;
use std::path::PathBuf;
use std::process::ExitCode;

pub const USAGE: &str = "\
usage: stackhour bridge migrate --from <legacy-config.json> [options]

Convert a legacy Node coordinator config.json into the stackhour config
directory. Reads the legacy file; never writes it.

  --from <path>          REQUIRED. The legacy config.json. Deliberately has no
                         default: auto-discovering a live bridge config invites
                         an accidental run.
  --to <dir>             Registry root (default: the resolved config dir).
  --runtime-dir <dir>    Bridge runtime dir (default: the resolved runtime dir).
  --dry-run              Print the plan. Writes nothing at all — no mkdir, no
                         temp file. Secrets are always masked.
  --force                Back each conflicting file up to <name>.bak-<unix-ts>,
                         then overwrite.
  --no-engines           Do not freeze engines/claude.toml and codex.toml; keep
                         tracking the built-ins instead.
  --verify               Re-read both sides and report any drift, including a
                         legacy key that reached no destination. Writes nothing.
";

/// Parse argv and run. `args` excludes `bridge migrate` itself.
pub fn run(args: &[String]) -> ExitCode {
    let mut from: Option<PathBuf> = None;
    let mut to: Option<PathBuf> = None;
    let mut runtime_dir: Option<PathBuf> = None;
    let (mut dry_run, mut force, mut engines, mut verify_only) = (false, false, true, false);

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // Both `--flag value` and `--flag=value`, because both are muscle
        // memory and guessing wrong here costs a re-run.
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (arg, None),
        };
        let take = |i: &mut usize| -> Option<String> {
            if let Some(v) = inline.clone() {
                return Some(v);
            }
            *i += 1;
            args.get(*i).cloned()
        };
        match name {
            "--from" => match take(&mut i) {
                Some(v) => from = Some(PathBuf::from(v)),
                None => return usage_error("--from needs a path"),
            },
            "--to" => match take(&mut i) {
                Some(v) => to = Some(PathBuf::from(v)),
                None => return usage_error("--to needs a path"),
            },
            "--runtime-dir" => match take(&mut i) {
                Some(v) => runtime_dir = Some(PathBuf::from(v)),
                None => return usage_error("--runtime-dir needs a path"),
            },
            "--dry-run" => dry_run = true,
            "--force" => force = true,
            "--no-engines" => engines = false,
            "--verify" => verify_only = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => return usage_error(&format!("unknown option '{other}'")),
        }
        i += 1;
    }

    let Some(from) = from else {
        return usage_error("--from is required (the legacy config.json to read)");
    };

    let home = home_dir();
    let storage = stackhour_core::paths::resolve_storage_paths_from_process_env();
    let opts = MigrateOptions {
        from: from.clone(),
        to: to.unwrap_or(storage.config_dir),
        runtime_dir: runtime_dir
            .unwrap_or_else(|| BridgePaths::resolve(&|k| std::env::var(k).ok(), &home).runtime_dir),
        dry_run,
        force,
        engines,
        home,
    };

    let legacy = match LegacyConfig::read(&opts.from) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("stackhour bridge migrate: {e}");
            return ExitCode::FAILURE;
        }
    };
    let plan = migrate::build_plan(&legacy, &opts);

    if verify_only {
        let issues = migrate::verify(&plan);
        if issues.is_empty() {
            println!(
                "verify: {} destination(s) match {}; every legacy key reached a destination.",
                plan.files.len(),
                opts.from.display()
            );
            return ExitCode::SUCCESS;
        }
        for issue in &issues {
            eprintln!("  {issue}");
        }
        eprintln!("\nverify: {} discrepancy(ies).", issues.len());
        return ExitCode::from(3);
    }

    if dry_run {
        print!("{}", migrate::render_plan(&plan));
        return ExitCode::SUCCESS;
    }

    if !force && !plan.conflicts().is_empty() {
        eprint!("{}", migrate::render_conflicts(&plan));
        return ExitCode::from(2);
    }

    match migrate::apply(&plan, force) {
        Ok(applied) => {
            for (original, backup) in &applied.backups {
                println!("backed up {} → {}", original.display(), backup.display());
            }
            for path in &applied.written {
                println!("wrote {}", path.display());
            }
            for warning in &plan.warnings {
                println!("warn  {warning}");
            }
            for key in &plan.unmapped {
                println!("warn  legacy key '{key}' reached no destination");
            }
            println!(
                "\nmigrated {} → {}. Next: `stackhour bridge doctor`, then cut over (stop the \
                 Node coordinator BEFORE starting the Rust bridge — two pollers on one token \
                 steal each other's messages).",
                opts.from.display(),
                opts.to.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("stackhour bridge migrate: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("stackhour bridge migrate: {msg}\n");
    eprint!("{USAGE}");
    ExitCode::FAILURE
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
