//! The individual doctor checks with EXACT names/statuses/messages (split
//! from doctor.rs for the line budget).
//!
//! Inventory, in order: 'runtime' (was 'node' before Node was retired — see
//! `runtime_check`), 'sqlite',
//! 'config' / 'config-permissions' (octal 3-pad, ' (recommend 600)',
//! missing-file warn WITHOUT a following ok line), 'data-dir' (ancestor
//! walk-up), 'offline-queue' (byte message in both ok and warn branches),
//! 'token', 'project-roots'/'project-root' asymmetry, 'claude-input',
//! 'codex-input', 'zed-input', 'database' (quick_check), 'server-auth' and
//! the agent-report/agent-version/agent-queue/clock-skew/watcher-<name>
//! chain, then 'services'.

use crate::doctor::{Check, CheckStatus, DoctorOpts};
use serde_json::Value;
use stackhour_core::config::Config;
use stackhour_core::jsnum::js_round_f64;
use stackhour_core::modules::{Module, ModuleSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use CheckStatus::{Error, Ok as StatusOk, Warn};

/// The Zed threads.db candidates, in probe order (ported from
/// src/agent/watch-zed.js `ZED_DB_PATHS`).
pub fn zed_db_paths(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("Library")
            .join("Application Support")
            .join("Zed")
            .join("threads")
            .join("threads.db"),
        home.join(".local")
            .join("share")
            .join("zed")
            .join("threads")
            .join("threads.db"),
    ]
}

/// The 'runtime' check.
///
/// The Node doctor reported its interpreter's major version and errored below
/// 22. The port kept the check keyed `node` — reporting "no node runtime
/// required" — so scripts grepping that key kept working during the
/// migration. Node is now gone from the project entirely, and a check named
/// after it is exactly the stale artifact that misleads a reader about what
/// the program needs, so the key is `runtime`.
///
/// BREAKING (`doctor --json`): `checks[0].name` is `runtime`, was `node`.
fn runtime_check() -> Check {
    Check::new(
        "runtime",
        StatusOk,
        format!("rust {}", stackhour_core::VERSION),
    )
}

/// The 'sqlite' check — the Rust build links SQLite statically, so this is
/// an availability report rather than a dynamic-import probe.
///
/// Gated on the two modules that actually link rusqlite: the tracker (via
/// stackhour-store) and the agent (its own Zed `threads.db` snapshot).
/// Reporting "sqlite available" from a bridge-only binary that has no SQLite
/// in it would be a lie, so the check is absent there instead.
#[cfg(any(feature = "tracker", feature = "agent"))]
fn sqlite_check() -> Check {
    match rusqlite::Connection::open_in_memory() {
        Result::Ok(_) => Check::new("sqlite", StatusOk, "sqlite available"),
        Result::Err(e) => Check::new("sqlite", Error, e.to_string()),
    }
}

fn config_checks(cfg: Result<&Config, &str>, opts: &DoctorOpts, out: &mut Vec<Check>) {
    let path = &opts.config_path;
    let display = path.display();
    // Node adds the missing-file WARN and then, only if the file exists, a
    // second ok line. A missing config therefore yields exactly one entry.
    if !path.exists() {
        out.push(Check::new("config", Warn, format!("not found: {display}")));
    }
    match cfg {
        Result::Ok(_) if path.exists() => {
            out.push(Check::new("config", StatusOk, format!("valid JSON: {display}")));
        }
        Result::Ok(_) => {}
        Result::Err(msg) => out.push(Check::new(
            "config",
            Error,
            format!("cannot load {display}: {msg}"),
        )),
    }

    if path.exists() {
        match std::fs::metadata(path) {
            Result::Ok(md) => {
                use std::os::unix::fs::PermissionsExt;
                let mode = md.permissions().mode() & 0o777;
                let loose = mode & 0o077 != 0;
                out.push(Check::new(
                    "config-permissions",
                    if loose { Warn } else { StatusOk },
                    format!(
                        "{mode:03o} {display}{}",
                        if loose { " (recommend 600)" } else { "" }
                    ),
                ));
            }
            Result::Err(e) => out.push(Check::new("config-permissions", Error, e.to_string())),
        }
    }
}

fn data_dir_checks(opts: &DoctorOpts, out: &mut Vec<Check>) {
    let dir = &opts.data_dir;
    // Walk up to the nearest existing ancestor and test THAT for rw — a
    // not-yet-created data dir is fine as long as it can be created.
    let mut probe = dir.clone();
    while !probe.exists() {
        match probe.parent() {
            Some(p) if p != probe => probe = p.to_path_buf(),
            _ => break,
        }
    }
    let writable = probe.exists()
        && std::fs::metadata(&probe)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false);
    if writable {
        out.push(Check::new(
            "data-dir",
            StatusOk,
            if dir.exists() {
                dir.display().to_string()
            } else {
                format!("{} (will be created)", dir.display())
            },
        ));
    } else {
        out.push(Check::new(
            "data-dir",
            Error,
            format!("{}: not readable and writable", dir.display()),
        ));
    }

    let queue = dir.join("queue.jsonl");
    if queue.exists() {
        match std::fs::metadata(&queue) {
            Result::Ok(md) => {
                let bytes = md.len();
                out.push(Check::new(
                    "offline-queue",
                    if bytes > 0 { Warn } else { StatusOk },
                    format!("{bytes} bytes waiting in {}", queue.display()),
                ));
            }
            Result::Err(e) => out.push(Check::new("offline-queue", Error, e.to_string())),
        }
    } else {
        out.push(Check::new("offline-queue", StatusOk, "empty"));
    }
}

fn token_check(cfg: &Config) -> Check {
    let tokens_len = cfg
        .raw
        .get("server")
        .and_then(|s| s.get("tokens"))
        .and_then(Value::as_object)
        .map_or(0, serde_json::Map::len);
    if !cfg.server.token.is_empty() || tokens_len > 0 || !cfg.agent.token.is_empty() {
        Check::new("token", StatusOk, "configured (value hidden)")
    } else {
        Check::new("token", Warn, "no ingest token configured")
    }
}

fn project_root_checks(cfg: &Config, out: &mut Vec<Check>) {
    // Note the asymmetry: the "none configured" warning is 'project-roots'
    // (plural) while each individual root reports as 'project-root'.
    if cfg.agent.project_roots.is_empty() {
        out.push(Check::new(
            "project-roots",
            Warn,
            "none configured; file watcher cannot run",
        ));
    }
    for root in &cfg.agent.project_roots {
        let p = Path::new(root);
        if !p.is_dir() {
            out.push(Check::new(
                "project-root",
                Error,
                format!("{root}: not a directory"),
            ));
        } else if std::fs::read_dir(p).is_err() {
            out.push(Check::new(
                "project-root",
                Error,
                format!("{root}: permission denied"),
            ));
        } else {
            out.push(Check::new("project-root", StatusOk, root.clone()));
        }
    }
}

fn input_checks(cfg: &Config, opts: &DoctorOpts, out: &mut Vec<Check>) {
    let inputs = [
        (
            "claude-input",
            cfg.agent.watch.claude,
            opts.home.join(".claude").join("projects"),
        ),
        (
            "codex-input",
            cfg.agent.watch.codex,
            opts.home.join(".codex").join("sessions"),
        ),
    ];
    for (name, enabled, input) in inputs {
        if !enabled {
            out.push(Check::new(name, StatusOk, "disabled"));
        } else if !input.exists() {
            out.push(Check::new(name, Warn, format!("not found: {}", input.display())));
        } else if std::fs::read_dir(&input).is_err() {
            out.push(Check::new(
                name,
                Error,
                format!("{}: permission denied", input.display()),
            ));
        } else {
            out.push(Check::new(name, StatusOk, input.display().to_string()));
        }
    }

    if !cfg.agent.watch.zed {
        out.push(Check::new("zed-input", StatusOk, "disabled"));
        return;
    }
    let candidates = opts
        .zed_db_paths
        .clone()
        .unwrap_or_else(|| zed_db_paths(&opts.home));
    match candidates.into_iter().find(|c| c.exists()) {
        Some(db) => out.push(Check::new("zed-input", StatusOk, db.display().to_string())),
        None => out.push(Check::new("zed-input", Warn, "threads.db not found")),
    }
}

/// Probes `cfg.server.db`, which is a tracker artifact — a build without the
/// tracker feature has no server database to check, and rusqlite may not even
/// be linked.
#[cfg(feature = "tracker")]
fn database_check(cfg: &Config) -> Check {
    let db = &cfg.server.db;
    if !db.exists() {
        return Check::new("database", Warn, format!("not found: {}", db.display()));
    }
    let probe = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .and_then(|conn| conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)));
    match probe {
        Result::Ok(result) => Check::new(
            "database",
            if result == "ok" { StatusOk } else { Error },
            format!("{}: {result}", db.display()),
        ),
        Result::Err(e) => Check::new("database", Error, format!("{}: {e}", db.display())),
    }
}

/// The `server-auth` probe and, on success, the agent-report chain. Node
/// wraps the WHOLE block in one try/catch, so a failure partway through can
/// append a SECOND `server-auth` error entry after the first ok — preserved.
fn server_checks(cfg: &Config, out: &mut Vec<Check>) {
    let base = cfg.agent.server_url.trim_end_matches('/').to_string();
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Result::Ok(c) => c,
        Result::Err(e) => {
            out.push(Check::new("server-auth", Error, format!("{base}: {e}")));
            return;
        }
    };
    let mut req = client.get(format!("{base}/api/auth-check"));
    if !cfg.agent.token.is_empty() {
        req = req.header("authorization", format!("Bearer {}", cfg.agent.token));
    }
    let res = match req.send() {
        Result::Ok(r) => r,
        Result::Err(e) => {
            out.push(Check::new("server-auth", Error, format!("{base}: {e}")));
            return;
        }
    };
    if !res.status().is_success() {
        out.push(Check::new(
            "server-auth",
            Error,
            format!("{base}: HTTP {}", res.status().as_u16()),
        ));
        return;
    }
    out.push(Check::new("server-auth", StatusOk, base.clone()));

    let statuses: Value = match client
        .get(format!("{base}/api/agent-status"))
        .send()
        .and_then(reqwest::blocking::Response::json)
    {
        Result::Ok(v) => v,
        // The second `server-auth` error entry after an ok one.
        Result::Err(e) => {
            out.push(Check::new("server-auth", Error, format!("{base}: {e}")));
            return;
        }
    };
    let local = statuses
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|s| s.get("machine").and_then(Value::as_str) == Some(&cfg.agent.machine))
        })
        .cloned();
    agent_report_checks(cfg, local.as_ref(), out);
}

/// The agent-report / agent-version / agent-queue / clock-skew / watcher-<n>
/// chain, split out so it can be tested against a canned status payload.
pub fn agent_report_checks(cfg: &Config, local: Option<&Value>, out: &mut Vec<Check>) {
    let machine = &cfg.agent.machine;
    let Some(local) = local else {
        out.push(Check::new(
            "agent-report",
            Warn,
            format!("no report stored for {machine}"),
        ));
        return;
    };
    let num = |key: &str| local.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    // `Math.max(90, (intervalSeconds || 20) * 3)`.
    let interval = match local.get("intervalSeconds").and_then(Value::as_f64) {
        Some(v) if v != 0.0 => v,
        _ => 20.0,
    };
    let stale_after = 90.0_f64.max(interval * 3.0);

    let age = num("ageSeconds");
    out.push(Check::new(
        "agent-report",
        if age <= stale_after { StatusOk } else { Error },
        format!("{machine}: {}s old", js_round_f64(age)),
    ));

    let version = local.get("version").and_then(Value::as_str).unwrap_or("");
    out.push(Check::new(
        "agent-version",
        if version == stackhour_core::VERSION {
            StatusOk
        } else {
            Warn
        },
        format!("agent {version}, doctor {}", stackhour_core::VERSION),
    ));

    let depth = num("queueDepth");
    out.push(Check::new(
        "agent-queue",
        if depth != 0.0 { Warn } else { StatusOk },
        if depth != 0.0 {
            format!("{depth} heartbeats ({} bytes) queued", num("queueBytes"))
        } else {
            "empty".to_string()
        },
    ));

    let skew = num("clockSkewSeconds");
    out.push(Check::new(
        "clock-skew",
        if skew.abs() > 30.0 { Warn } else { StatusOk },
        format!("{}s", js_round_f64(skew)),
    ));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let Some(watchers) = local.get("watchers").and_then(Value::as_object) else {
        return;
    };
    for (name, watcher) in watchers {
        let flag = |k: &str| watcher.get(k).and_then(Value::as_bool).unwrap_or(false);
        if !flag("enabled") || !flag("available") {
            continue;
        }
        let last_ok = watcher.get("lastOk").and_then(Value::as_f64).unwrap_or(0.0);
        let watcher_age = now - last_ok;
        let error = watcher.get("error").and_then(Value::as_str).unwrap_or("");
        let consecutive = watcher
            .get("consecutiveErrors")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let unmatched = watcher
            .get("unmatchedInputRuns")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let key = format!("watcher-{name}");
        if !error.is_empty() || consecutive > 0.0 {
            let msg = if error.is_empty() {
                format!("{consecutive} consecutive errors")
            } else {
                error.to_string()
            };
            out.push(Check::new(&key, Error, msg));
        } else if last_ok == 0.0 || watcher_age > stale_after {
            out.push(Check::new(
                &key,
                Error,
                format!("last successful poll {}s ago", js_round_f64(watcher_age)),
            ));
        } else if unmatched >= 5.0 {
            out.push(Check::new(
                &key,
                Warn,
                format!("{unmatched} input changes produced no heartbeat"),
            ));
        } else {
            out.push(Check::new(
                &key,
                StatusOk,
                format!("last poll {}s ago", js_round_f64(watcher_age.max(0.0))),
            ));
        }
    }
}

fn services_check(out: &mut Vec<Check>) {
    if cfg!(target_os = "linux") {
        // `systemctl is-active` exits nonzero when any unit is inactive but
        // still prints one status line per unit, so stdout is counted either
        // way rather than treating a nonzero exit as a failure.
        let stdout = std::process::Command::new("systemctl")
            .args(["--user", "is-active", "stackhour-agent", "stackhour-server"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let active = stdout.trim().split('\n').filter(|line| *line == "active").count();
        out.push(Check::new(
            "services",
            if active > 0 { StatusOk } else { Warn },
            format!("{active}/2 Stackhour user services active"),
        ));
    } else if cfg!(target_os = "macos") {
        let uid = unsafe { libc::getuid() };
        let ok = std::process::Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/com.stackhour.agent")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        out.push(if ok {
            Check::new("services", StatusOk, "Stackhour launch agent active")
        } else {
            Check::new("services", Warn, "Stackhour launch agent inactive")
        });
    }
}

/// Append one status line per module that is OFF at either layer.
///
/// DELIBERATE DIVERGENCE (no Node original): Node's doctor has no notion of
/// modules. Emits NOTHING when everything is on, which is what keeps a
/// default build against a config with no `modules` key byte-identical — and
/// what keeps `check_order_matches_the_node_inventory` green untouched.
///
/// Status is always `Ok`: a deliberate operator choice is not a fault, and an
/// `Error` here would make a healthy bridge-only box start exiting 1. The
/// lines exist so nobody debugs a deliberately absent `database` check as a
/// broken install.
///
/// Layer 1 is reported INSTEAD of Layer 2 when both are off, matching
/// `modules::gate`: recompiling is the only remedy, so sending the operator
/// to config.json would be the wrong instruction.
///
/// Split out as `pub` (like `agent_report_checks`) so tests can drive both
/// layers hermetically without a `DoctorOpts` change.
pub fn module_checks(
    runtime: ModuleSet,
    compiled: ModuleSet,
    config_path: &Path,
    out: &mut Vec<Check>,
) {
    for m in Module::ALL {
        if !compiled.contains(m) {
            out.push(Check::new(
                &format!("module-{}", m.name()),
                StatusOk,
                format!(
                    "not compiled into this binary (rebuild with --features {})",
                    m.name()
                ),
            ));
        } else if !runtime.contains(m) {
            out.push(Check::new(
                &format!("module-{}", m.name()),
                StatusOk,
                format!(
                    "disabled by \"{}\": false in {}",
                    m.config_key(),
                    config_path.display()
                ),
            ));
        }
    }
}

/// Build the full ordered check list.
pub fn all_checks(cfg: Result<&Config, &str>, opts: &DoctorOpts) -> Vec<Check> {
    // Positions are load-bearing: a default build must still emit
    // `node, sqlite, config, ...` in exactly today's order, so the two
    // feature-gated entries are pushed where they have always sat rather than
    // appended at the end.
    let mut out = vec![runtime_check()];
    #[cfg(any(feature = "tracker", feature = "agent"))]
    out.push(sqlite_check());
    config_checks(cfg, opts, &mut out);
    data_dir_checks(opts, &mut out);

    // Everything below needs a config; a load failure stops the report here,
    // exactly like Node's `if (cfg) { ... }` guard.
    let Result::Ok(cfg) = cfg else {
        // A config we could not load resolves the RUNTIME layer to all-enabled
        // (the gate fails open), so a corrupt config still reports the
        // compile-time layer rather than guessing at the user's block.
        module_checks(ModuleSet::ALL, crate::compiled_modules(), &opts.config_path, &mut out);
        return out;
    };
    out.push(token_check(cfg));
    project_root_checks(cfg, &mut out);
    input_checks(cfg, opts, &mut out);
    #[cfg(feature = "tracker")]
    out.push(database_check(cfg));
    if opts.check_server {
        server_checks(cfg, &mut out);
    }
    if opts.check_services {
        services_check(&mut out);
    }
    // Appended LAST, after `services`, so every existing check keeps its
    // pinned index. `crate::compiled_modules()` rather than a second copy of
    // the `cfg!` triple: Layer 1 has exactly one source of truth.
    module_checks(cfg.modules, crate::compiled_modules(), &opts.config_path, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn opts_for(tmp: &TempDir) -> DoctorOpts {
        DoctorOpts {
            json: false,
            config_path: tmp.path().join("config.json"),
            data_dir: tmp.path().join("data"),
            home: tmp.path().to_path_buf(),
            check_services: false,
            check_server: false,
            zed_db_paths: Some(vec![]),
        }
    }

    fn config_at(path: &Path, body: &str) -> Config {
        std::fs::write(path, body).unwrap();
        stackhour_core::config::load_config(path).unwrap()
    }

    fn find<'a>(checks: &'a [Check], name: &str) -> Vec<&'a Check> {
        checks.iter().filter(|c| c.name == name).collect()
    }

    /// A missing config yields exactly ONE 'config' entry (the warn) — Node
    /// only adds the "valid JSON" ok line when the file exists.
    #[test]
    fn missing_config_warns_once_with_no_ok_line() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = stackhour_core::config::load_config(&opts.config_path).unwrap();
        let checks = all_checks(Result::Ok(&cfg), &opts);
        let config = find(&checks, "config");
        assert_eq!(config.len(), 1);
        assert_eq!(config[0].status, Warn);
        assert!(config[0].message.starts_with("not found: "));
        // No file, so no permission check either.
        assert!(find(&checks, "config-permissions").is_empty());
    }

    /// A group- or world-readable config is a warning with the 3-padded octal
    /// mode and the ' (recommend 600)' suffix.
    #[test]
    fn config_permissions_warn_when_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = config_at(&opts.config_path, "{}");
        std::fs::set_permissions(&opts.config_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let checks = all_checks(Result::Ok(&cfg), &opts);
        let perms = find(&checks, "config-permissions");
        assert_eq!(perms.len(), 1);
        assert_eq!(perms[0].status, Warn);
        assert!(perms[0].message.starts_with("644 "), "got {:?}", perms[0].message);
        assert!(perms[0].message.ends_with(" (recommend 600)"));
    }

    /// 0600 is clean: ok status, no suffix, and the mode is 3-padded.
    #[test]
    fn config_permissions_ok_at_0600_with_padded_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = config_at(&opts.config_path, "{}");
        std::fs::set_permissions(&opts.config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let checks = all_checks(Result::Ok(&cfg), &opts);
        let perms = find(&checks, "config-permissions");
        assert_eq!(perms[0].status, StatusOk);
        assert!(perms[0].message.starts_with("600 "));
        assert!(!perms[0].message.contains("recommend"));
    }

    /// A config that fails to LOAD errors and stops the report: none of the
    /// config-dependent checks may run.
    #[test]
    fn a_broken_config_errors_and_truncates_the_report() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        std::fs::write(&opts.config_path, "{ not json").unwrap();
        let checks = all_checks(Result::Err("bad json at line 1"), &opts);
        let config = find(&checks, "config");
        assert_eq!(config[0].status, Error);
        assert!(config[0].message.contains("cannot load "));
        assert!(config[0].message.ends_with("bad json at line 1"));
        for downstream in ["token", "project-roots", "database", "server-auth"] {
            assert!(
                find(&checks, downstream).is_empty(),
                "{downstream} must not run without a config"
            );
        }
    }

    /// A not-yet-created data dir is fine as long as an ancestor is writable.
    #[test]
    fn data_dir_reports_will_be_created_when_absent() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = config_at(&opts.config_path, "{}");
        let checks = all_checks(Result::Ok(&cfg), &opts);
        let dd = find(&checks, "data-dir");
        assert_eq!(dd[0].status, StatusOk);
        assert!(dd[0].message.ends_with(" (will be created)"));
    }

    /// A non-empty queue file WARNS and reports its byte count; an absent one
    /// is simply 'empty'.
    #[test]
    fn offline_queue_warns_with_a_byte_count() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = config_at(&opts.config_path, "{}");
        assert_eq!(
            find(&all_checks(Result::Ok(&cfg), &opts), "offline-queue")[0].message,
            "empty"
        );

        std::fs::create_dir_all(&opts.data_dir).unwrap();
        let queue = opts.data_dir.join("queue.jsonl");
        std::fs::write(&queue, "0123456789").unwrap();
        let q = find(&all_checks(Result::Ok(&cfg), &opts), "offline-queue")[0].clone();
        assert_eq!(q.status, Warn);
        assert_eq!(q.message, format!("10 bytes waiting in {}", queue.display()));

        // An existing but EMPTY queue is ok — and still reports bytes.
        std::fs::write(&queue, "").unwrap();
        let q = find(&all_checks(Result::Ok(&cfg), &opts), "offline-queue")[0].clone();
        assert_eq!(q.status, StatusOk);
        assert!(q.message.starts_with("0 bytes waiting in "));
    }

    /// The token check must never leak the secret it found.
    #[test]
    fn token_check_hides_the_value() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = config_at(&opts.config_path, "{}");
        assert_eq!(token_check(&cfg).status, Warn);

        let cfg = config_at(&opts.config_path, r#"{"server":{"tokens":{"box":"s3cret"}}}"#);
        let check = token_check(&cfg);
        assert_eq!(check.status, StatusOk);
        assert_eq!(check.message, "configured (value hidden)");
        assert!(!check.message.contains("s3cret"));
    }

    /// Plural 'project-roots' for the none-configured warning, singular
    /// 'project-root' per root — the names really are different.
    #[test]
    fn project_root_check_names_are_asymmetric() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = config_at(&opts.config_path, "{}");
        let mut out = Vec::new();
        project_root_checks(&cfg, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "project-roots");
        assert_eq!(out[0].status, Warn);

        let root = tmp.path().join("proj");
        std::fs::create_dir(&root).unwrap();
        let cfg = config_at(
            &opts.config_path,
            &format!(
                r#"{{"agent":{{"projectRoots":["{}","/nope/missing"]}}}}"#,
                root.display()
            ),
        );
        let mut out = Vec::new();
        project_root_checks(&cfg, &mut out);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|c| c.name == "project-root"));
        assert_eq!(out[0].status, StatusOk);
        assert_eq!(out[1].status, Error);
        assert!(out[1].message.starts_with("/nope/missing: "));
    }

    /// A disabled watcher reports ok/'disabled' rather than probing its dir.
    #[test]
    fn disabled_inputs_report_disabled() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = config_at(
            &opts.config_path,
            r#"{"agent":{"watch":{"claude":false,"codex":false,"zed":false}}}"#,
        );
        let mut out = Vec::new();
        input_checks(&cfg, &opts, &mut out);
        for name in ["claude-input", "codex-input", "zed-input"] {
            let c = find(&out, name);
            assert_eq!(c[0].status, StatusOk);
            assert_eq!(c[0].message, "disabled");
        }
    }

    #[test]
    fn missing_input_dirs_warn_and_present_ones_pass() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = config_at(&opts.config_path, "{}");
        let mut out = Vec::new();
        input_checks(&cfg, &opts, &mut out);
        assert_eq!(find(&out, "claude-input")[0].status, Warn);

        std::fs::create_dir_all(opts.home.join(".claude").join("projects")).unwrap();
        let mut out = Vec::new();
        input_checks(&cfg, &opts, &mut out);
        let c = find(&out, "claude-input");
        assert_eq!(c[0].status, StatusOk);
        assert!(c[0].message.ends_with("/.claude/projects"));
    }

    #[test]
    fn zed_input_warns_when_no_candidate_db_exists() {
        let tmp = TempDir::new().unwrap();
        let mut opts = opts_for(&tmp);
        let cfg = config_at(&opts.config_path, "{}");
        opts.zed_db_paths = Some(vec![tmp.path().join("nope.db")]);
        let mut out = Vec::new();
        input_checks(&cfg, &opts, &mut out);
        assert_eq!(find(&out, "zed-input")[0].status, Warn);
        assert_eq!(find(&out, "zed-input")[0].message, "threads.db not found");

        let db = tmp.path().join("threads.db");
        std::fs::write(&db, "").unwrap();
        opts.zed_db_paths = Some(vec![tmp.path().join("nope.db"), db.clone()]);
        let mut out = Vec::new();
        input_checks(&cfg, &opts, &mut out);
        assert_eq!(find(&out, "zed-input")[0].status, StatusOk);
        assert_eq!(find(&out, "zed-input")[0].message, db.display().to_string());
    }

    #[cfg(feature = "tracker")]
    #[test]
    fn database_check_warns_when_absent_and_passes_quick_check() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let db = tmp.path().join("stackhour.db");
        let cfg = config_at(
            &opts.config_path,
            &format!(r#"{{"server":{{"db":"{}"}}}}"#, db.display()),
        );
        assert_eq!(database_check(&cfg).status, Warn);
        assert!(database_check(&cfg).message.starts_with("not found: "));

        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE t(a)")
            .unwrap();
        let check = database_check(&cfg);
        assert_eq!(check.status, StatusOk);
        assert!(check.message.ends_with(": ok"));
    }

    fn cfg_with_machine(tmp: &TempDir, machine: &str) -> Config {
        config_at(
            &tmp.path().join("config.json"),
            &format!(r#"{{"agent":{{"machine":"{machine}"}}}}"#),
        )
    }

    #[test]
    fn agent_report_warns_when_the_machine_has_no_status() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_machine(&tmp, "box");
        let mut out = Vec::new();
        agent_report_checks(&cfg, None, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "agent-report");
        assert_eq!(out[0].status, Warn);
        assert_eq!(out[0].message, "no report stored for box");
    }

    /// Staleness is `max(90, intervalSeconds*3)` — a 20s interval keeps the
    /// 90s floor, so 100s old is stale but 89s is not.
    #[test]
    fn agent_report_staleness_uses_the_ninety_second_floor() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_machine(&tmp, "box");
        for (age, expected) in [(89.0, StatusOk), (90.0, StatusOk), (91.0, Error)] {
            let mut out = Vec::new();
            agent_report_checks(
                &cfg,
                Some(&json!({ "machine": "box", "ageSeconds": age, "intervalSeconds": 20 })),
                &mut out,
            );
            assert_eq!(find(&out, "agent-report")[0].status, expected, "age {age}");
        }
        // A long interval raises the threshold above the floor.
        let mut out = Vec::new();
        agent_report_checks(
            &cfg,
            Some(&json!({ "machine": "box", "ageSeconds": 150.0, "intervalSeconds": 60 })),
            &mut out,
        );
        assert_eq!(find(&out, "agent-report")[0].status, StatusOk);
    }

    #[test]
    fn agent_version_mismatch_is_a_warning_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_machine(&tmp, "box");
        let mut out = Vec::new();
        agent_report_checks(
            &cfg,
            Some(&json!({ "machine": "box", "ageSeconds": 1, "version": "0.0.1-old" })),
            &mut out,
        );
        let v = find(&out, "agent-version");
        assert_eq!(v[0].status, Warn);
        assert_eq!(
            v[0].message,
            format!("agent 0.0.1-old, doctor {}", stackhour_core::VERSION)
        );

        let mut out = Vec::new();
        agent_report_checks(
            &cfg,
            Some(&json!({
                "machine": "box", "ageSeconds": 1, "version": stackhour_core::VERSION
            })),
            &mut out,
        );
        assert_eq!(find(&out, "agent-version")[0].status, StatusOk);
    }

    #[test]
    fn clock_skew_warns_only_beyond_thirty_seconds_in_either_direction() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_machine(&tmp, "box");
        for (skew, expected) in [(0.0, StatusOk), (30.0, StatusOk), (30.5, Warn), (-31.0, Warn)] {
            let mut out = Vec::new();
            agent_report_checks(
                &cfg,
                Some(&json!({ "machine": "box", "ageSeconds": 1, "clockSkewSeconds": skew })),
                &mut out,
            );
            assert_eq!(find(&out, "clock-skew")[0].status, expected, "skew {skew}");
        }
    }

    /// Watchers that are disabled or unavailable produce no entry at all.
    #[test]
    fn only_enabled_and_available_watchers_are_reported() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_machine(&tmp, "box");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let mut out = Vec::new();
        agent_report_checks(
            &cfg,
            Some(&json!({
                "machine": "box", "ageSeconds": 1,
                "watchers": {
                    "off": { "enabled": false, "available": true },
                    "gone": { "enabled": true, "available": false },
                    "good": { "enabled": true, "available": true, "lastOk": now },
                    "broken": { "enabled": true, "available": true, "lastOk": now,
                                "error": "permission denied" },
                    "noisy": { "enabled": true, "available": true, "lastOk": now,
                               "unmatchedInputRuns": 7 },
                }
            })),
            &mut out,
        );
        assert!(find(&out, "watcher-off").is_empty());
        assert!(find(&out, "watcher-gone").is_empty());
        assert_eq!(find(&out, "watcher-good")[0].status, StatusOk);
        assert!(find(&out, "watcher-good")[0].message.starts_with("last poll "));
        assert_eq!(find(&out, "watcher-broken")[0].status, Error);
        assert_eq!(find(&out, "watcher-broken")[0].message, "permission denied");
        assert_eq!(find(&out, "watcher-noisy")[0].status, Warn);
        assert_eq!(
            find(&out, "watcher-noisy")[0].message,
            "7 input changes produced no heartbeat"
        );
    }

    /// A watcher that has never polled successfully is an ERROR, not a warn.
    #[test]
    fn a_watcher_that_never_polled_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_machine(&tmp, "box");
        let mut out = Vec::new();
        agent_report_checks(
            &cfg,
            Some(&json!({
                "machine": "box", "ageSeconds": 1,
                "watchers": { "files": { "enabled": true, "available": true } }
            })),
            &mut out,
        );
        assert_eq!(find(&out, "watcher-files")[0].status, Error);
        assert!(find(&out, "watcher-files")[0]
            .message
            .starts_with("last successful poll "));
    }

    /// The queue message embeds both counts and is 'empty' when idle.
    #[test]
    fn agent_queue_reports_depth_and_bytes() {
        let tmp = TempDir::new().unwrap();
        let cfg = cfg_with_machine(&tmp, "box");
        let mut out = Vec::new();
        agent_report_checks(
            &cfg,
            Some(&json!({
                "machine": "box", "ageSeconds": 1, "queueDepth": 3, "queueBytes": 512
            })),
            &mut out,
        );
        let q = find(&out, "agent-queue");
        assert_eq!(q[0].status, Warn);
        assert_eq!(q[0].message, "3 heartbeats (512 bytes) queued");

        let mut out = Vec::new();
        agent_report_checks(
            &cfg,
            Some(&json!({ "machine": "box", "ageSeconds": 1, "queueDepth": 0 })),
            &mut out,
        );
        assert_eq!(find(&out, "agent-queue")[0].status, StatusOk);
        assert_eq!(find(&out, "agent-queue")[0].message, "empty");
    }

    /// The overall ordering is a documented output contract.
    ///
    /// Pinned to the DEFAULT build, for two independent reasons: the list
    /// below names `sqlite` and `database`, which a build without the tracker
    /// feature deliberately omits, and it names no `module-*` line, which a
    /// build missing ANY feature appends (Layer 1 is off, so doctor says so).
    /// The attribute is the only change — the vector is byte-identical to the
    /// pre-feature version.
    #[cfg(all(feature = "tracker", feature = "agent", feature = "bridge"))]
    #[test]
    fn check_order_matches_the_documented_inventory() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = config_at(&opts.config_path, "{}");
        let names: Vec<&str> = all_checks(Result::Ok(&cfg), &opts)
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
            .leak()
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(
            names,
            vec![
                "runtime",
                "sqlite",
                "config",
                "config-permissions",
                "data-dir",
                "offline-queue",
                "token",
                "project-roots",
                "claude-input",
                "codex-input",
                "zed-input",
                "database",
            ]
        );
    }

    // ---- module status lines -------------------------------------------

    fn module_line_names(checks: &[Check]) -> Vec<String> {
        checks
            .iter()
            .filter(|c| c.name.starts_with("module-"))
            .map(|c| c.name.clone())
            .collect()
    }

    /// The prime constraint, stated for doctor: with nothing off there is
    /// nothing to say, so a default build against a config with no `modules`
    /// key emits exactly the check list it always has.
    #[test]
    fn an_all_enabled_build_appends_no_module_lines() {
        let mut out = Vec::new();
        module_checks(
            ModuleSet::ALL,
            ModuleSet::ALL,
            Path::new("/home/u/.config/stackhour/config.json"),
            &mut out,
        );
        assert_eq!(out, Vec::new());
    }

    #[test]
    fn a_disabled_module_appends_one_ok_status_line_naming_the_config_key() {
        let mut out = Vec::new();
        module_checks(
            ModuleSet::new(true, true, false),
            ModuleSet::ALL,
            Path::new("/home/u/.config/stackhour/config.json"),
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "module-bridge");
        assert_eq!(out[0].status, StatusOk);
        assert_eq!(
            out[0].message,
            "disabled by \"modules.bridge\": false in /home/u/.config/stackhour/config.json"
        );
    }

    /// Layer 1 wins, exactly as `modules::gate` resolves it: a module that is
    /// neither compiled nor enabled must send the operator to `cargo build`,
    /// not to config.json, and must say so only once.
    #[test]
    fn an_uncompiled_module_is_reported_instead_of_the_disabled_line() {
        let mut out = Vec::new();
        module_checks(
            ModuleSet::new(true, true, false),
            ModuleSet::new(true, true, false),
            Path::new("/home/u/.config/stackhour/config.json"),
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "module-bridge");
        assert_eq!(
            out[0].message,
            "not compiled into this binary (rebuild with --features bridge)"
        );
        assert!(!out[0].message.contains("config.json"));
    }

    /// The lines are APPENDED: every pre-existing check keeps its index, so
    /// no script that reads `checks[n]` breaks.
    #[test]
    fn module_lines_are_appended_after_every_existing_check() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        let cfg = config_at(&opts.config_path, r#"{ "modules": { "bridge": false } }"#);
        let with = all_checks(Result::Ok(&cfg), &opts);
        let plain = config_at(&opts.config_path, "{}");
        let without = all_checks(Result::Ok(&plain), &opts);
        // Everything the plain config reports is a PREFIX of the disabled
        // one: module lines are appended, never interleaved, so no existing
        // check changes index.
        assert_eq!(&with[..without.len()], &without[..]);
        // Every module line sits at the very tail, below every real check.
        let first = with.iter().position(|c| c.name.starts_with("module-")).unwrap();
        assert!(with[first..].iter().all(|c| c.name.starts_with("module-")));
        // Turning bridge off in the CONFIG is what put a bridge line there —
        // unless this build has no bridge compiled in, in which case Layer 1
        // had already claimed the line and wins.
        let bridge = with.iter().find(|c| c.name == "module-bridge").expect("bridge is off");
        if crate::compiled_modules().bridge {
            assert!(bridge.message.starts_with("disabled by \"modules.bridge\""), "{bridge:?}");
            assert!(!without.iter().any(|c| c.name == "module-bridge"));
        } else {
            assert!(bridge.message.starts_with("not compiled"), "{bridge:?}");
        }
    }

    /// A deliberate operator choice is not a fault. If these were `Error`,
    /// every healthy bridge-only leader would start exiting 1.
    #[test]
    fn module_lines_never_change_the_exit_code() {
        let mut out = Vec::new();
        module_checks(
            ModuleSet::new(false, false, false),
            ModuleSet::ALL,
            Path::new("/home/u/.config/stackhour/config.json"),
            &mut out,
        );
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|c| c.status == StatusOk));
        let report = crate::doctor::Report { checks: out };
        assert!(report.ok());
        assert_eq!(report.exit_code(), 0);
    }

    /// doctor is the diagnostic of last resort: the early return for a config
    /// it could not load must still say which modules this binary even has.
    #[test]
    fn a_corrupt_config_still_reports_the_compile_time_layer() {
        let tmp = TempDir::new().unwrap();
        let opts = opts_for(&tmp);
        std::fs::write(&opts.config_path, "{ not json").unwrap();
        let out = all_checks(Result::Err("Unexpected token"), &opts);
        // Whatever this build compiled in, the runtime layer is all-enabled
        // here, so the ONLY module lines that may appear are Layer 1 ones.
        for c in out.iter().filter(|c| c.name.starts_with("module-")) {
            assert!(c.message.starts_with("not compiled into this binary"), "{c:?}");
        }
        let expected = ModuleSet::ALL.disabled().len()
            + crate::compiled_modules().disabled().len();
        assert_eq!(module_line_names(&out).len(), expected);
    }

    /// One grep prefix for both layers, so `doctor --json | grep module-`
    /// finds every off module regardless of which layer switched it off.
    #[test]
    fn every_module_line_is_grep_prefixed_with_module_dash() {
        let mut out = Vec::new();
        module_checks(
            ModuleSet::new(false, true, true),
            ModuleSet::new(true, true, false),
            Path::new("/home/u/.config/stackhour/config.json"),
            &mut out,
        );
        assert_eq!(
            module_line_names(&out),
            vec!["module-tracker".to_string(), "module-bridge".to_string()]
        );
        for m in Module::ALL {
            assert_eq!(format!("module-{}", m.name()).split('-').count(), 2);
        }
    }
}
