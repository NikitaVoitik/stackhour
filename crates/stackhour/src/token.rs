//! `stackhour token create|revoke|list` — the machine-enrollment CLI.
//!
//! Ports `runToken` in src/tokens.js. The CRUD itself lives in
//! stackhour-core::tokens; this module owns only the exact stdout, the
//! `--raw` / no-publicUrl fallback, and the enrollment-code hand-off. A
//! secret is printed ONLY on the `--raw` path (and inside the opaque
//! enrollment blob), never on `list`.

use crate::args::{has_flag, last_option};
use serde_json::Value;
use stackhour_core::tokens::{
    create_enrollment, create_machine_token, list_machine_tokens, revoke_machine_token, valid_url,
};
use stackhour_core::{Error, Result};
use std::io::Write;
use std::path::Path;

/// `serverUrl(config, override)` — the override wins, then `server.publicUrl`;
/// a falsy result means "no URL", which downgrades `create` to raw output.
fn public_url(cfg_path: &Path, override_url: Option<String>) -> Result<Option<String>> {
    let value = match override_url.filter(|u| !u.is_empty()) {
        Some(u) => Some(u),
        None => {
            let text = std::fs::read_to_string(cfg_path)
                .map_err(|e| Error::msg(format!("cannot read config: {e}")))?;
            let config: Value =
                serde_json::from_str(&text).map_err(|e| Error::msg(format!("cannot read config: {e}")))?;
            config
                .get("server")
                .and_then(|s| s.get("publicUrl"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }
    };
    match value {
        Some(u) => Ok(Some(valid_url(&u)?)),
        None => Ok(None),
    }
}

/// The `stackhour token <create MACHINE|revoke MACHINE|list>` CLI.
pub fn run_token(args: &[String]) -> Result<()> {
    let cfg_path = stackhour_core::paths::resolve_storage_paths_from_process_env().config_path;
    let mut out = std::io::stdout();
    run_token_into(args, &cfg_path, &mut out)
}

/// Injectable form (config path + stdout sink) so the output contract is
/// testable without touching the real HOME.
pub fn run_token_into(args: &[String], cfg_path: &Path, out: &mut dyn Write) -> Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("");
    let machine = args.get(1).map(String::as_str).unwrap_or("");

    match command {
        "create" => {
            let (name, secret) = create_machine_token(
                cfg_path,
                machine,
                last_option(args, "token"),
                has_flag(args, "--force"),
            )?;
            let url = public_url(cfg_path, last_option(args, "server-url"))?;
            match url {
                // `--raw`, or no public URL to enroll against: print the
                // secret and stop.
                _ if has_flag(args, "--raw") => {
                    writeln!(out, "Token for {name}: {secret}")?;
                }
                None => {
                    writeln!(out, "Token for {name}: {secret}")?;
                }
                Some(url) => {
                    let code = create_enrollment(&url, &name, &secret)?;
                    writeln!(out, "Enrolled {name}. On that machine run:\n")?;
                    writeln!(
                        out,
                        "  ./target/release/stackhour init agent --enrollment={code} --install\n"
                    )?;
                    writeln!(
                        out,
                        "Add one or more --project-root=/path options before --install if needed."
                    )?;
                }
            }
            Ok(())
        }
        "revoke" => {
            revoke_machine_token(cfg_path, machine)?;
            // The name is re-derived by the core call; echo the trimmed form.
            writeln!(out, "Revoked token for {}", machine.trim())?;
            Ok(())
        }
        "list" => {
            for name in list_machine_tokens(cfg_path)? {
                writeln!(out, "{name}")?;
            }
            Ok(())
        }
        _ => Err(Error::msg(
            "usage: stackhour token <create MACHINE|revoke MACHINE|list>",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn seeded(public_url: Option<&str>) -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let body = match public_url {
            Some(u) => {
                format!(r#"{{"server":{{"host":"0.0.0.0","port":4040,"publicUrl":"{u}","tokens":{{}}}}}}"#)
            }
            None => r#"{"server":{"host":"0.0.0.0","port":4040,"tokens":{}}}"#.to_string(),
        };
        std::fs::write(&cfg, body).unwrap();
        (tmp, cfg)
    }

    fn run(args: &[&str], cfg: &Path) -> Result<String> {
        let mut out = Vec::new();
        run_token_into(&argv(args), cfg, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    /// With a publicUrl configured, `create` emits a copy-paste enrollment
    /// command rather than the bare secret.
    #[test]
    fn create_prints_an_enrollment_command() {
        let (_tmp, cfg) = seeded(Some("http://server.test:4040"));
        let out = run(&["create", "laptop", "--token=sekrit"], &cfg).unwrap();
        assert!(out.starts_with("Enrolled laptop. On that machine run:\n\n"));
        assert!(out.contains("  ./target/release/stackhour init agent --enrollment="));
        assert!(out.ends_with("Add one or more --project-root=/path options before --install if needed.\n"));
        // The raw secret is never printed on this path.
        assert!(!out.contains("sekrit"));

        // The emitted code must actually round-trip.
        let code = out
            .split("--enrollment=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        let parsed = stackhour_core::tokens::parse_enrollment(code).unwrap();
        assert_eq!(parsed.machine, "laptop");
        assert_eq!(parsed.token, "sekrit");
        assert_eq!(parsed.server_url, "http://server.test:4040");
    }

    #[test]
    fn create_with_raw_prints_the_secret() {
        let (_tmp, cfg) = seeded(Some("http://server.test:4040"));
        assert_eq!(
            run(&["create", "laptop", "--token=sekrit", "--raw"], &cfg).unwrap(),
            "Token for laptop: sekrit\n"
        );
    }

    /// Without a publicUrl there is nothing to enroll against, so `create`
    /// falls back to raw output even without `--raw`.
    #[test]
    fn create_without_a_public_url_falls_back_to_raw() {
        let (_tmp, cfg) = seeded(None);
        assert_eq!(
            run(&["create", "laptop", "--token=sekrit"], &cfg).unwrap(),
            "Token for laptop: sekrit\n"
        );
    }

    #[test]
    fn create_persists_the_token_and_requires_force_to_rotate() {
        let (_tmp, cfg) = seeded(Some("http://server.test:4040"));
        run(&["create", "laptop", "--token=one"], &cfg).unwrap();
        let stored: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(stored["server"]["tokens"]["laptop"], "one");

        let err = run(&["create", "laptop", "--token=two"], &cfg).unwrap_err();
        assert_eq!(
            err.message(),
            "token for laptop already exists; pass --force to rotate it"
        );
        run(&["create", "laptop", "--token=two", "--force"], &cfg).unwrap();
    }

    /// `list` prints names, one per line, and NEVER a secret.
    #[test]
    fn list_prints_machine_names_only() {
        let (_tmp, cfg) = seeded(Some("http://server.test:4040"));
        run(&["create", "beta", "--token=s1"], &cfg).unwrap();
        run(&["create", "alpha", "--token=s2"], &cfg).unwrap();
        let out = run(&["list"], &cfg).unwrap();
        assert_eq!(out, "alpha\nbeta\n");
        assert!(!out.contains("s1") && !out.contains("s2"));
    }

    #[test]
    fn revoke_removes_the_token() {
        let (_tmp, cfg) = seeded(Some("http://server.test:4040"));
        run(&["create", "laptop", "--token=s"], &cfg).unwrap();
        assert_eq!(
            run(&["revoke", "laptop"], &cfg).unwrap(),
            "Revoked token for laptop\n"
        );
        assert_eq!(run(&["list"], &cfg).unwrap(), "");
        assert_eq!(
            run(&["revoke", "laptop"], &cfg).unwrap_err().message(),
            "no token exists for laptop"
        );
    }

    #[test]
    fn unknown_subcommand_is_a_usage_error() {
        let (_tmp, cfg) = seeded(None);
        assert_eq!(
            run(&["frobnicate"], &cfg).unwrap_err().message(),
            "usage: stackhour token <create MACHINE|revoke MACHINE|list>"
        );
        assert_eq!(
            run(&[], &cfg).unwrap_err().message(),
            "usage: stackhour token <create MACHINE|revoke MACHINE|list>"
        );
    }
}
