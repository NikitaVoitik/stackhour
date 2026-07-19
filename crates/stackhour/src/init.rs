//! `stackhour init server|agent`.
//!
//! ALL validations run BEFORE any write (validPort, host, machine name,
//! token, publicUrl via valid_url, --project-root realpath with the ORIGINAL
//! arg quoted in errors). init_server writes the `tokens` map — never the
//! legacy `token` key — and an `agent` section only-if-absent; the db path
//! comes from resolve_storage_paths honoring a forced STACKHOUR_CONFIG.
//! init_agent: --enrollment XOR explicit flags with the machine!==hostname
//! loophole quirk, STACKHOUR_TOKEN env fallback. Exact runInit stdout
//! including --install chaining and the 'Next:' hint lines. Secrets are
//! never printed.

use crate::args::{has_flag, last_option, option_values};
use serde_json::{json, Map, Value};
use stackhour_core::config::read_existing_raw;
use stackhour_core::jsnum::js_number;
use stackhour_core::paths::{expand_home, resolve_storage_paths};
use stackhour_core::tokens::{
    generate_token, js_trim, parse_enrollment, valid_url, write_raw_config,
};
use stackhour_core::{Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Raw options for `init server` (values as they arrived from the CLI —
/// port stays a string until validPort, exactly like the JS).
#[derive(Debug, Clone, Default)]
pub struct InitServerOpts {
    pub config_path: PathBuf,
    pub force: bool,
    /// Default '0.0.0.0'.
    pub host: Option<String>,
    /// Default 4040; validated via validPort.
    pub port: Option<String>,
    /// Default `http://127.0.0.1:<port>`.
    pub public_url: Option<String>,
    /// Default os hostname.
    pub machine: Option<String>,
    /// None -> a fresh generate_token().
    pub token: Option<String>,
    pub project_roots: Vec<String>,
}

/// Raw options for `init agent`.
#[derive(Debug, Clone, Default)]
pub struct InitAgentOpts {
    pub config_path: PathBuf,
    pub force: bool,
    pub server_url: Option<String>,
    /// `--token` or the STACKHOUR_TOKEN env fallback (None with enrollment).
    pub token: Option<String>,
    /// Default os hostname (the machine!==hostname loophole check).
    pub machine: Option<String>,
    pub project_roots: Vec<String>,
    pub enrollment: Option<String>,
}

/// What an init produced (feeds the exact stdout lines; secrets stay out of
/// Debug output paths).
#[derive(Debug, Clone)]
pub struct InitResult {
    pub config_path: PathBuf,
    /// The written `server` section (init server only).
    pub server: Option<Value>,
    /// The written/preserved `agent` section.
    pub agent: Option<Value>,
}

fn os_hostname() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// `validPort` — `Number(port)` with full JS coercion, then an
/// integer-in-[1,65535] check.
fn valid_port(port: &str) -> Result<i64> {
    let value = js_number(&Value::String(port.to_string()));
    if !value.is_finite() || value.fract() != 0.0 || !(1.0..=65535.0).contains(&value) {
        return Err(Error::msg("port must be an integer from 1 to 65535"));
    }
    Ok(value as i64)
}

/// `resolvedRoots` — expand `~`, resolve against cwd, realpath, and require a
/// directory. The error quotes the ORIGINAL argument, not the resolved path.
fn resolved_roots(roots: &[String], home: &Path) -> Result<Vec<String>> {
    roots
        .iter()
        .map(|root| {
            let expanded = expand_home(root, home);
            let resolved = std::path::Path::new(&expanded)
                .canonicalize()
                .ok()
                .filter(|p| p.is_dir())
                .ok_or_else(|| {
                    Error::msg(format!(
                        "invalid project root (must be an existing directory): {root}"
                    ))
                })?;
            Ok(resolved.to_string_lossy().into_owned())
        })
        .collect()
}

/// `String(v || '').trim()` — the JS default-parameter rule is that only an
/// ABSENT (`undefined`) value takes the default; an explicit empty string
/// falls through to the emptiness check.
fn clean_or_default(value: Option<&String>, default: &str) -> String {
    match value {
        None => js_trim(default).to_string(),
        Some(v) => js_trim(v).to_string(),
    }
}

/// Library form of init server (JS `initServer`).
pub fn init_server(opts: InitServerOpts) -> Result<InitResult> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let clean_host = clean_or_default(opts.host.as_ref(), "0.0.0.0");
    let clean_machine = clean_or_default(opts.machine.as_ref(), &os_hostname());
    let generated;
    let clean_token = match opts.token.as_ref() {
        None => {
            generated = generate_token();
            js_trim(&generated).to_string()
        }
        Some(t) => js_trim(t).to_string(),
    };
    if clean_host.is_empty() {
        return Err(Error::msg("host cannot be empty"));
    }
    if clean_machine.is_empty() {
        return Err(Error::msg("machine name cannot be empty"));
    }
    if clean_token.is_empty() {
        return Err(Error::msg("token cannot be empty"));
    }

    let existing = read_existing_raw(&opts.config_path)?;
    let has_server = existing.get("server").is_some_and(|v| !v.is_null());
    if has_server && !opts.force {
        return Err(Error::msg(
            "server config already exists; pass --force to replace it",
        ));
    }

    // resolveStoragePaths({ ...process.env, STACKHOUR_CONFIG: configPath }):
    // the config path wins, data/db still follow the environment.
    let cfg_path_str = opts.config_path.to_string_lossy().into_owned();
    let env = |key: &str| {
        if key == "STACKHOUR_CONFIG" {
            Some(cfg_path_str.clone())
        } else {
            std::env::var(key).ok()
        }
    };
    let storage = resolve_storage_paths(&env, &home);

    let clean_port = valid_port(opts.port.as_deref().unwrap_or("4040"))?;
    let public_url_input = match opts.public_url.as_deref() {
        Some(u) if !u.is_empty() => u.to_string(),
        // `publicUrl || \`http://127.0.0.1:${port}\`` — falsy (absent or
        // empty) falls back to the loopback default.
        _ => format!("http://127.0.0.1:{clean_port}"),
    };
    let clean_public_url = valid_url(&public_url_input)?;

    let server = json!({
        "host": clean_host,
        "port": clean_port,
        "publicUrl": clean_public_url,
        "db": storage.db_path.to_string_lossy(),
        "tokens": { clean_machine.clone(): clean_token.clone() },
    });

    let mut config = match existing {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    // `{ ...existing, server }`: insert on an existing key keeps its position.
    config.insert("server".to_string(), server.clone());

    // A server commonly runs its own agent. Configure it on first setup while
    // preserving an explicitly initialized agent section.
    let agent = if config.get("agent").is_some_and(|v| !v.is_null()) {
        config.get("agent").cloned()
    } else {
        let agent = json!({
            "serverUrl": format!("http://127.0.0.1:{clean_port}"),
            "token": clean_token,
            "machine": clean_machine,
            "projectRoots": resolved_roots(&opts.project_roots, &home)?,
        });
        config.insert("agent".to_string(), agent.clone());
        Some(agent)
    };

    let value = Value::Object(config);
    write_raw_config(&opts.config_path, &value)?;
    Ok(InitResult {
        config_path: opts.config_path,
        server: Some(server),
        agent,
    })
}

/// Library form of init agent (JS `initAgent`).
pub fn init_agent(opts: InitAgentOpts) -> Result<InitResult> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let hostname = os_hostname();

    let (server_url, token, machine) = match opts.enrollment.as_deref() {
        // JS: `if (enrollment)` — an EMPTY enrollment string is falsy and
        // takes the explicit-flags path.
        Some(code) if !code.is_empty() => {
            // The `machine !== os.hostname()` loophole: passing --machine with
            // exactly the local hostname is NOT rejected, because the default
            // parameter has already made the two indistinguishable.
            let machine_conflicts = opts.machine.as_deref().is_some_and(|m| m != hostname);
            if opts.server_url.is_some() || opts.token.is_some() || machine_conflicts {
                return Err(Error::msg(
                    "do not combine --enrollment with server URL, token, or machine",
                ));
            }
            let parsed = parse_enrollment(code)?;
            (
                Some(parsed.server_url),
                Some(parsed.token),
                Some(parsed.machine),
            )
        }
        _ => (opts.server_url.clone(), opts.token.clone(), opts.machine),
    };

    // `if (!serverUrl)` — absent OR empty is a missing server URL.
    let server_url = match server_url {
        Some(u) if !u.is_empty() => u,
        _ => return Err(Error::msg("--server-url is required")),
    };
    let clean_token = js_trim(token.as_deref().unwrap_or("")).to_string();
    if clean_token.is_empty() {
        return Err(Error::msg("--token is required"));
    }

    let existing = read_existing_raw(&opts.config_path)?;
    let has_agent = existing.get("agent").is_some_and(|v| !v.is_null());
    if has_agent && !opts.force {
        return Err(Error::msg(
            "agent config already exists; pass --force to replace it",
        ));
    }

    let clean_machine = clean_or_default(machine.as_ref(), &hostname);
    let agent = json!({
        "serverUrl": valid_url(&server_url)?,
        "token": clean_token,
        "machine": clean_machine,
        "projectRoots": resolved_roots(&opts.project_roots, &home)?,
    });
    // JS checks machine emptiness AFTER building the object, so a URL or root
    // failure is reported first.
    if clean_machine.is_empty() {
        return Err(Error::msg("machine name cannot be empty"));
    }

    let mut config = match existing {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    config.insert("agent".to_string(), agent.clone());
    let value = Value::Object(config);
    write_raw_config(&opts.config_path, &value)?;
    Ok(InitResult {
        config_path: opts.config_path,
        server: None,
        agent: Some(agent),
    })
}

/// The `stackhour init <server|agent> [options]` CLI.
pub fn run_init(args: &[String]) -> Result<()> {
    let config_path = stackhour_core::paths::resolve_storage_paths_from_process_env().config_path;
    let mut out = std::io::stdout();
    run_init_into(args, &config_path, &mut out, &|role| {
        crate::install::install_service(role).map(|_| ())
    })
}

/// Injectable form: the config path, the stdout sink, and the installer are
/// parameters so tests can drive the exact `runInit` output without touching
/// the real HOME or launching services.
pub fn run_init_into(
    args: &[String],
    config_path: &Path,
    out: &mut dyn Write,
    installer: &dyn Fn(&str) -> Result<()>,
) -> Result<()> {
    let role = args.first().map(String::as_str).unwrap_or("");
    let force = has_flag(args, "--force");
    let install = has_flag(args, "--install");

    match role {
        "server" => {
            let result = init_server(InitServerOpts {
                config_path: config_path.to_path_buf(),
                force,
                host: last_option(args, "host"),
                port: last_option(args, "port"),
                public_url: last_option(args, "public-url"),
                machine: last_option(args, "machine"),
                token: None,
                project_roots: option_values(args, "project-root"),
            })?;
            let server = result.server.as_ref().expect("init server writes a server");
            let agent = result.agent.as_ref().expect("init server writes an agent");
            writeln!(
                out,
                "Created server config at {}",
                result.config_path.display()
            )?;
            writeln!(out, "Public URL: {}", str_field(server, "publicUrl"))?;
            writeln!(out, "Local agent enrolled as {}", str_field(agent, "machine"))?;
            if install {
                installer("server")?;
                installer("agent")?;
                writeln!(
                    out,
                    "Installed and started stackhour-server and stackhour-agent"
                )?;
            }
            writeln!(out, "Next: ./bin/stackhour token create <machine>")?;
            Ok(())
        }
        "agent" => {
            let enrollment = last_option(args, "enrollment");
            // With an enrollment code the token flag is dropped entirely —
            // including the STACKHOUR_TOKEN fallback, which must not smuggle
            // an ambient secret into the "do not combine" check.
            let token = match enrollment.as_deref() {
                Some(code) if !code.is_empty() => None,
                _ => last_option(args, "token")
                    .filter(|t| !t.is_empty())
                    .or_else(|| std::env::var("STACKHOUR_TOKEN").ok()),
            };
            let result = init_agent(InitAgentOpts {
                config_path: config_path.to_path_buf(),
                force,
                server_url: last_option(args, "server-url"),
                token,
                machine: last_option(args, "machine"),
                project_roots: option_values(args, "project-root"),
                enrollment,
            })?;
            let agent = result.agent.as_ref().expect("init agent writes an agent");
            writeln!(
                out,
                "Created agent config at {}",
                result.config_path.display()
            )?;
            writeln!(
                out,
                "Enrolled {} with {}",
                str_field(agent, "machine"),
                str_field(agent, "serverUrl")
            )?;
            if install {
                installer("agent")?;
                writeln!(out, "Installed and started stackhour-agent")?;
            }
            writeln!(out, "Next: ./bin/stackhour doctor")?;
            Ok(())
        }
        _ => Err(Error::msg(
            "usage: stackhour init <server|agent> [options]",
        )),
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn no_install(_role: &str) -> Result<()> {
        panic!("installer must not run without --install");
    }

    /// The headline regression for the `init server` scaffold: the verb has
    /// to actually produce a config file with the documented shape.
    #[test]
    fn init_server_writes_the_documented_config_shape() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let result = init_server(InitServerOpts {
            config_path: cfg.clone(),
            public_url: Some("https://stack.example.com/".to_string()),
            machine: Some("laptop".to_string()),
            token: Some("secret-token".to_string()),
            ..Default::default()
        })
        .unwrap();

        let written = read_json(&cfg);
        let server = &written["server"];
        assert_eq!(server["host"], "0.0.0.0");
        assert_eq!(server["port"], 4040);
        // validUrl strips exactly one trailing slash.
        assert_eq!(server["publicUrl"], "https://stack.example.com");
        // The `tokens` MAP is written; the legacy scalar `token` key is not.
        assert_eq!(server["tokens"]["laptop"], "secret-token");
        assert!(server.get("token").is_none());
        // A server gets a co-located agent on first setup.
        assert_eq!(written["agent"]["machine"], "laptop");
        assert_eq!(written["agent"]["serverUrl"], "http://127.0.0.1:4040");
        assert_eq!(written["agent"]["projectRoots"], json!([]));
        assert_eq!(result.config_path, cfg);
    }

    /// Config files hold machine secrets: the atomic write must land 0600.
    #[test]
    fn init_server_config_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("nested").join("config.json");
        init_server(InitServerOpts {
            config_path: cfg.clone(),
            ..Default::default()
        })
        .unwrap();
        let mode = std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must not be group/world readable");
    }

    #[test]
    fn init_server_refuses_to_clobber_without_force() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let opts = InitServerOpts {
            config_path: cfg.clone(),
            ..Default::default()
        };
        init_server(opts.clone()).unwrap();
        let err = init_server(opts.clone()).unwrap_err();
        assert_eq!(
            err.message(),
            "server config already exists; pass --force to replace it"
        );
        init_server(InitServerOpts { force: true, ..opts }).unwrap();
    }

    /// An explicitly initialized agent survives a `--force` server re-init.
    #[test]
    fn init_server_preserves_an_existing_agent_section() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        std::fs::write(
            &cfg,
            r#"{"agent":{"machine":"mine","serverUrl":"http://x.test","token":"t","projectRoots":[]}}"#,
        )
        .unwrap();
        init_server(InitServerOpts {
            config_path: cfg.clone(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(read_json(&cfg)["agent"]["machine"], "mine");
    }

    /// Unknown top-level keys must round-trip: init rewrites the whole file.
    #[test]
    fn init_server_preserves_unknown_keys() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        std::fs::write(&cfg, r#"{"wakatime":{"apiKey":"k"}}"#).unwrap();
        init_server(InitServerOpts {
            config_path: cfg.clone(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(read_json(&cfg)["wakatime"]["apiKey"], "k");
    }

    #[test]
    fn port_validation_matches_number_coercion() {
        assert_eq!(valid_port("4040").unwrap(), 4040);
        // JS Number() accepts surrounding whitespace and hex literals.
        assert_eq!(valid_port(" 8080 ").unwrap(), 8080);
        assert_eq!(valid_port("0x10").unwrap(), 16);
        for bad in ["0", "65536", "-1", "4040.5", "abc", "", "Infinity"] {
            assert!(valid_port(bad).is_err(), "{bad} must be rejected");
        }
    }

    /// Nothing may be written when an argument is invalid.
    #[test]
    fn invalid_input_writes_no_file() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        for opts in [
            InitServerOpts {
                config_path: cfg.clone(),
                port: Some("0".into()),
                ..Default::default()
            },
            InitServerOpts {
                config_path: cfg.clone(),
                host: Some("  ".into()),
                ..Default::default()
            },
            InitServerOpts {
                config_path: cfg.clone(),
                public_url: Some("ftp://x.test".into()),
                ..Default::default()
            },
            InitServerOpts {
                config_path: cfg.clone(),
                project_roots: vec!["/definitely/not/here".into()],
                ..Default::default()
            },
        ] {
            assert!(init_server(opts).is_err());
            assert!(!cfg.exists(), "a rejected init must not create a config");
        }
    }

    #[test]
    fn project_root_errors_quote_the_original_argument() {
        let tmp = TempDir::new().unwrap();
        let err = init_server(InitServerOpts {
            config_path: tmp.path().join("config.json"),
            project_roots: vec!["~/definitely-not-a-real-dir".into()],
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(
            err.message(),
            "invalid project root (must be an existing directory): ~/definitely-not-a-real-dir"
        );
    }

    #[test]
    fn init_agent_round_trips_an_enrollment_code() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let code = stackhour_core::tokens::create_enrollment(
            "http://server.test:4040",
            "laptop",
            "sekrit",
        )
        .unwrap();
        init_agent(InitAgentOpts {
            config_path: cfg.clone(),
            enrollment: Some(code),
            ..Default::default()
        })
        .unwrap();
        let agent = &read_json(&cfg)["agent"];
        assert_eq!(agent["serverUrl"], "http://server.test:4040");
        assert_eq!(agent["machine"], "laptop");
        assert_eq!(agent["token"], "sekrit");
    }

    #[test]
    fn init_agent_rejects_enrollment_combined_with_explicit_flags() {
        let tmp = TempDir::new().unwrap();
        let code =
            stackhour_core::tokens::create_enrollment("http://s.test", "laptop", "sekrit").unwrap();
        let err = init_agent(InitAgentOpts {
            config_path: tmp.path().join("config.json"),
            enrollment: Some(code),
            token: Some("other".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(
            err.message(),
            "do not combine --enrollment with server URL, token, or machine"
        );
    }

    #[test]
    fn init_agent_requires_a_server_url_and_token() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let err = init_agent(InitAgentOpts {
            config_path: cfg.clone(),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.message(), "--server-url is required");
        let err = init_agent(InitAgentOpts {
            config_path: cfg.clone(),
            server_url: Some("http://s.test".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.message(), "--token is required");
    }

    /// The exact `runInit` stdout is a user-facing contract.
    #[test]
    fn run_init_server_prints_the_node_stdout() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let mut out = Vec::new();
        run_init_into(
            &argv(&["server", "--public-url=http://h.test:4040", "--machine=box"]),
            &cfg,
            &mut out,
            &no_install,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "Created server config at {}\nPublic URL: http://h.test:4040\nLocal agent enrolled as box\nNext: ./bin/stackhour token create <machine>\n",
                cfg.display()
            )
        );
    }

    /// `--install` chains BOTH roles for a server, in order, and adds a line.
    #[test]
    fn run_init_server_with_install_chains_both_roles() {
        use std::cell::RefCell;
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let seen = RefCell::new(Vec::new());
        let installer = |role: &str| {
            seen.borrow_mut().push(role.to_string());
            Ok(())
        };
        let mut out = Vec::new();
        run_init_into(&argv(&["server", "--install"]), &cfg, &mut out, &installer).unwrap();
        assert_eq!(seen.into_inner(), vec!["server", "agent"]);
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("Installed and started stackhour-server and stackhour-agent\n"));
    }

    #[test]
    fn run_init_rejects_an_unknown_role() {
        let tmp = TempDir::new().unwrap();
        let mut out = Vec::new();
        let err = run_init_into(
            &argv(&["frobnicate"]),
            &tmp.path().join("config.json"),
            &mut out,
            &no_install,
        )
        .unwrap_err();
        assert_eq!(
            err.message(),
            "usage: stackhour init <server|agent> [options]"
        );
        assert!(out.is_empty());
    }

    /// `--project-root` is repeatable and order-preserving all the way from
    /// argv into the written config.
    #[test]
    fn run_init_collects_repeated_project_roots_in_order() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let cfg = tmp.path().join("config.json");
        let mut out = Vec::new();
        run_init_into(
            &argv(&[
                "server",
                &format!("--project-root={}", b.display()),
                &format!("--project-root={}", a.display()),
            ]),
            &cfg,
            &mut out,
            &no_install,
        )
        .unwrap();
        let roots = read_json(&cfg)["agent"]["projectRoots"].clone();
        assert_eq!(
            roots,
            json!([
                b.canonicalize().unwrap().to_string_lossy(),
                a.canonicalize().unwrap().to_string_lossy()
            ])
        );
    }
}
