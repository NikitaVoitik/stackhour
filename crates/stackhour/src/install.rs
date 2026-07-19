//! `stackhour install <server|agent>` — systemd/launchd user services.
//!
//! Unit text byte-exact (systemdQuote with newline rejection, RestartSec 5
//! server / 10 agent, PATH Environment embedding the running binary's dir).
//! launchd plist XML-escaped, label com.stackhour.agent, log
//! /tmp/stackhour-agent.log. Executable = <repoRoot>/bin/stackhour derived
//! from the running binary + existence check (units must keep pointing at
//! the stable bin/ path — transition risk). Atomic 0644 unit writes; linux
//! systemctl daemon-reload + enable --now pair; darwin agent-only
//! bootout(ignored)/bootstrap/enable/kickstart order with the gui/<uid>
//! domain. `runInstall('server')` installs BOTH roles with exact wording.

use stackhour_core::{Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// What one role install did. Node's `installService` returns this
/// descriptor and callers may inspect it; the CLI path only needs the
/// success/failure, hence the allow.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Installed {
    pub role: String,
    /// The written unit/plist path.
    pub unit_path: PathBuf,
}

/// `systemdQuote` — a double-quoted string with `\` and `"` escaped. A
/// newline anywhere would let the value break out of the unit-file line, so
/// it is rejected rather than escaped.
fn systemd_quote(value: &str) -> Result<String> {
    if value.contains('\r') || value.contains('\n') {
        return Err(Error::msg("service executable path contains a newline"));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// The JS `xml()` escaper: the same five entities, in the same forms.
fn xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Render the systemd unit for a role (byte-exact).
pub fn systemd_unit(role: &str, exe: &Path, path_dir: &Path) -> Result<String> {
    let (description, command, restart_sec) = match role {
        "server" => ("Stackhour coding time-tracking server", "serve", 5),
        "agent" => ("Stackhour coding time-tracking agent", "agent", 10),
        _ => return Err(Error::msg("service role must be server or agent")),
    };
    let path_env = systemd_quote(&format!(
        "PATH={}:/usr/local/bin:/usr/bin:/bin",
        path_dir.display()
    ))?;
    let exec = systemd_quote(&exe.to_string_lossy())?;
    Ok(format!(
        "[Unit]\nDescription={description}\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nEnvironment={path_env}\n\
         ExecStart={exec} {command}\nRestart=always\nRestartSec={restart_sec}\n\n\
         [Install]\nWantedBy=default.target\n"
    ))
}

/// Render the launchd plist (byte-exact, XML-escaped).
pub fn launchd_plist(exe: &Path, path_dir: &Path) -> String {
    let path_env = xml(&format!(
        "{}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
        path_dir.display()
    ));
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"https://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key><string>com.stackhour.agent</string>\n\
         \x20 <key>ProgramArguments</key><array>\n\
         \x20   <string>{exe}</string><string>agent</string>\n\
         \x20 </array>\n\
         \x20 <key>EnvironmentVariables</key><dict>\n\
         \x20   <key>PATH</key><string>{path_env}</string>\n\
         \x20 </dict>\n\
         \x20 <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n\
         \x20 <key>StandardOutPath</key><string>/tmp/stackhour-agent.log</string>\n\
         \x20 <key>StandardErrorPath</key><string>/tmp/stackhour-agent.log</string>\n\
         </dict>\n</plist>\n",
        exe = xml(&exe.to_string_lossy()),
    )
}

/// The stable `<repoRoot>/bin/stackhour` shim the units must point at. The
/// running binary lives at `<repoRoot>/target/<profile>/stackhour`, so the
/// repo root is found by walking up until a `bin/stackhour` appears.
fn service_executable() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::msg(format!("cannot locate the running binary: {e}")))?;
    let mut dir = exe.parent();
    while let Some(d) = dir {
        let candidate = d.join("bin").join("stackhour");
        if candidate.exists() {
            return Ok(candidate);
        }
        dir = d.parent();
    }
    Err(Error::msg(format!(
        "Stackhour executable not found: {}",
        exe.parent().unwrap_or(&exe).join("bin/stackhour").display()
    )))
}

fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|e| Error::msg(format!("{program}: {e}")))?;
    if !status.success() {
        return Err(Error::msg(format!(
            "{program} {} failed with {status}",
            args.join(" ")
        )));
    }
    Ok(())
}

/// Install + start one role's service.
pub fn install_service(role: &str) -> Result<Installed> {
    let exe = service_executable()?;
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/usr/bin"));

    if cfg!(target_os = "linux") {
        let unit_name = format!("stackhour-{role}.service");
        let unit_path = home
            .join(".config")
            .join("systemd")
            .join("user")
            .join(&unit_name);
        let unit = systemd_unit(role, &exe, &exe_dir)?;
        stackhour_core::fsutil::atomic_write_0644(&unit_path, unit.as_bytes())
            .map_err(|e| Error::msg(format!("cannot write {}: {e}", unit_path.display())))?;
        run_cmd("systemctl", &["--user", "daemon-reload"])?;
        run_cmd("systemctl", &["--user", "enable", "--now", &unit_name])?;
        return Ok(Installed {
            role: role.to_string(),
            unit_path,
        });
    }

    if cfg!(target_os = "macos") {
        if role != "agent" {
            return Err(Error::msg(
                "automatic macOS installation supports the agent role only",
            ));
        }
        let label = "com.stackhour.agent";
        let plist_path = home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{label}.plist"));
        stackhour_core::fsutil::atomic_write_0644(
            &plist_path,
            launchd_plist(&exe, &exe_dir).as_bytes(),
        )
        .map_err(|e| Error::msg(format!("cannot write {}: {e}", plist_path.display())))?;
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
        let plist_str = plist_path.to_string_lossy().into_owned();
        // bootout is best-effort: it fails when nothing is loaded yet.
        let _ = run_cmd("launchctl", &["bootout", &domain, &plist_str]);
        run_cmd("launchctl", &["bootstrap", &domain, &plist_str])?;
        let target = format!("{domain}/{label}");
        run_cmd("launchctl", &["enable", &target])?;
        run_cmd("launchctl", &["kickstart", "-k", &target])?;
        return Ok(Installed {
            role: role.to_string(),
            unit_path: plist_path,
        });
    }

    Err(Error::msg(format!(
        "automatic service installation is not supported on {}",
        std::env::consts::OS
    )))
}

/// The `stackhour install <server|agent>` CLI.
pub fn run_install(args: &[String]) -> Result<()> {
    let mut out = std::io::stdout();
    run_install_into(args, &mut out, &|role| install_service(role).map(|_| ()))
}

/// Injectable form so the exact stdout and the role ORDER can be tested
/// without touching systemd/launchd.
pub fn run_install_into(
    args: &[String],
    out: &mut dyn Write,
    installer: &dyn Fn(&str) -> Result<()>,
) -> Result<()> {
    match args.first().map(String::as_str).unwrap_or("") {
        "server" => {
            installer("server")?;
            installer("agent")?;
            writeln!(
                out,
                "Installed and started stackhour-server and stackhour-agent"
            )?;
            Ok(())
        }
        "agent" => {
            installer("agent")?;
            writeln!(out, "Installed and started stackhour-agent")?;
            Ok(())
        }
        _ => Err(Error::msg("usage: stackhour install <server|agent>")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn systemd_unit_is_byte_exact_for_the_server_role() {
        let unit = systemd_unit("server", Path::new("/repo/bin/stackhour"), Path::new("/usr/bin"))
            .unwrap();
        assert_eq!(
            unit,
            "[Unit]\nDescription=Stackhour coding time-tracking server\n\
             After=network-online.target\nWants=network-online.target\n\n\
             [Service]\nEnvironment=\"PATH=/usr/bin:/usr/local/bin:/usr/bin:/bin\"\n\
             ExecStart=\"/repo/bin/stackhour\" serve\nRestart=always\nRestartSec=5\n\n\
             [Install]\nWantedBy=default.target\n"
        );
    }

    /// The agent differs in three places: description, verb, and RestartSec.
    #[test]
    fn systemd_unit_agent_uses_the_agent_verb_and_restartsec_10() {
        let unit =
            systemd_unit("agent", Path::new("/repo/bin/stackhour"), Path::new("/usr/bin")).unwrap();
        assert!(unit.contains("Description=Stackhour coding time-tracking agent\n"));
        assert!(unit.contains("ExecStart=\"/repo/bin/stackhour\" agent\n"));
        assert!(unit.contains("RestartSec=10\n"));
    }

    #[test]
    fn systemd_unit_rejects_an_unknown_role() {
        let err = systemd_unit("wombat", Path::new("/x"), Path::new("/y")).unwrap_err();
        assert_eq!(err.message(), "service role must be server or agent");
    }

    /// A quote or backslash in the path must not break out of the quoted
    /// value, and a newline must be refused outright.
    #[test]
    fn systemd_quoting_escapes_and_newlines_are_rejected() {
        let unit = systemd_unit(
            "agent",
            Path::new(r#"/repo/we"ird\path/stackhour"#),
            Path::new("/usr/bin"),
        )
        .unwrap();
        assert!(unit.contains(r#"ExecStart="/repo/we\"ird\\path/stackhour" agent"#));
        assert!(systemd_unit("agent", Path::new("/a\nb"), Path::new("/usr/bin")).is_err());
        assert!(systemd_unit("agent", Path::new("/a\rb"), Path::new("/usr/bin")).is_err());
    }

    #[test]
    fn launchd_plist_xml_escapes_the_executable_path() {
        let plist = launchd_plist(Path::new("/re<po>/bin/stack&hour"), Path::new("/usr/bin"));
        assert!(plist.contains("<string>/re&lt;po&gt;/bin/stack&amp;hour</string><string>agent</string>"));
        assert!(plist.contains("<key>Label</key><string>com.stackhour.agent</string>"));
        assert!(plist.contains("<string>/tmp/stackhour-agent.log</string>"));
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(plist.ends_with("</dict>\n</plist>\n"));
    }

    /// `install server` installs BOTH roles, server first.
    #[test]
    fn run_install_server_installs_both_roles_in_order() {
        let seen = RefCell::new(Vec::new());
        let installer = |role: &str| {
            seen.borrow_mut().push(role.to_string());
            Ok(())
        };
        let mut out = Vec::new();
        run_install_into(&argv(&["server"]), &mut out, &installer).unwrap();
        assert_eq!(seen.into_inner(), vec!["server", "agent"]);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Installed and started stackhour-server and stackhour-agent\n"
        );
    }

    #[test]
    fn run_install_agent_installs_only_the_agent() {
        let seen = RefCell::new(Vec::new());
        let installer = |role: &str| {
            seen.borrow_mut().push(role.to_string());
            Ok(())
        };
        let mut out = Vec::new();
        run_install_into(&argv(&["agent"]), &mut out, &installer).unwrap();
        assert_eq!(seen.into_inner(), vec!["agent"]);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Installed and started stackhour-agent\n"
        );
    }

    #[test]
    fn run_install_rejects_an_unknown_role() {
        let mut out = Vec::new();
        let err = run_install_into(&argv(&[]), &mut out, &|_| Ok(())).unwrap_err();
        assert_eq!(err.message(), "usage: stackhour install <server|agent>");
        assert!(out.is_empty());
    }
}
