use serde_json::{json, Map, Value};
use stackhour_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const RELEASE_INSTALLER_URL: &str =
    "https://github.com/NikitaVoitik/stackhour/releases/latest/download/install-stackhour.sh";

const HELP: &str = "\
usage:
  stackhour control install hub [options]
  stackhour control install node --hub-url=URL --token=TOKEN [options]
  stackhour control install ssh --host=HOST --hub-url=URL --token=TOKEN [options]

hub options:
  --bind=ADDRESS        listen address (default: 127.0.0.1:4050)
  --db=PATH             control database path
  --public-url=URL      public coordinator URL
  --node-token=TOKEN    fixed node token (generated when absent)
  --client-token=TOKEN  fixed panel token (generated when absent)
  --no-start            write files but do not start the service

node options:
  --hub-url=URL         full ws:// or wss:// node connection URL
  --id=ID               stable machine ID (default: host name)
  --token=TOKEN         node token (or STACKHOUR_CONTROL_NODE_TOKEN)
  --workspace=PATH      default agent workspace
  --claude-bin=COMMAND  Claude command (default: claude)
  --codex-bin=COMMAND   Codex command (default: codex)
  --no-start            write files but do not start the service

ssh options:
  --host=HOST           SSH host or alias
  --user=USER           SSH user
  --port=PORT           SSH port (default: 22)
  --identity=PATH       identity file on the coordinator
  plus the node options above
";

pub fn run(args: &[String]) -> Result<()> {
    if args.is_empty()
        || args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "-h" | "--help"))
        || args
            .get(1)
            .is_some_and(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        print!("{HELP}");
        return Ok(());
    }
    match args.first().map(String::as_str) {
        Some("hub") => install_hub(&args[1..]),
        Some("node") => install_node(&args[1..]),
        Some("ssh") => install_ssh(&args[1..]),
        _ => Err(Error::msg("usage: stackhour control install <hub|node|ssh>")),
    }
}

fn option(args: &[String], name: &str) -> Option<String> {
    let exact = format!("--{name}");
    let prefix = format!("{exact}=");
    let mut found = None;
    let mut index = 0;
    while index < args.len() {
        if args[index] == exact {
            if let Some(value) = args.get(index + 1) {
                found = Some(value.clone());
            }
            index += 2;
            continue;
        }
        if let Some(value) = args[index].strip_prefix(&prefix) {
            found = Some(value.to_string());
        }
        index += 1;
    }
    found.filter(|value| !value.trim().is_empty())
}

fn required(args: &[String], name: &str) -> Result<String> {
    option(args, name).ok_or_else(|| Error::msg(format!("--{name} is required")))
}

fn required_node_token(args: &[String]) -> Result<String> {
    option(args, "token")
        .or_else(|| std::env::var("STACKHOUR_CONTROL_NODE_TOKEN").ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            Error::msg("--token or the STACKHOUR_CONTROL_NODE_TOKEN environment variable is required")
        })
}

fn flag(args: &[String], name: &str) -> bool {
    let expected = format!("--{name}");
    args.iter().any(|arg| arg == &expected)
}

fn reject_unknown(args: &[String], allowed: &[&str]) -> Result<()> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if !arg.starts_with("--") {
            return Err(Error::msg(format!("unexpected argument: {arg}")));
        }
        let name = arg.trim_start_matches("--").split('=').next().unwrap_or_default();
        if !allowed.contains(&name) {
            return Err(Error::msg(format!("unknown option: --{name}")));
        }
        if !arg.contains('=') && name != "no-start" {
            index += 1;
            if index >= args.len() {
                return Err(Error::msg(format!("--{name} needs a value")));
            }
        }
        index += 1;
    }
    Ok(())
}

fn config_path() -> PathBuf {
    stackhour_core::paths::resolve_storage_paths_from_process_env().config_path
}

fn load_raw(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| Error::msg(format!("cannot parse {}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(Error::msg(format!("cannot read {}: {error}", path.display()))),
    }
}

fn raw_string(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)?
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
}

fn object(value: &mut Value) -> Result<&mut Map<String, Value>> {
    value
        .as_object_mut()
        .ok_or_else(|| Error::msg("config root must be an object"))
}

fn child_object<'a>(parent: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let entry = parent.entry(key.to_string()).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    entry.as_object_mut().expect("entry was set to object")
}

fn write_config(path: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    stackhour_core::fsutil::atomic_write_0600(path, text.as_bytes())
        .map_err(|error| Error::msg(format!("cannot write {}: {error}", path.display())))
}

fn hub_config(
    mut raw: Value,
    bind: &str,
    db: &Path,
    public_url: Option<&str>,
    node_token: &str,
    client_token: &str,
) -> Result<Value> {
    let root = object(&mut raw)?;
    child_object(root, "modules").insert("control".to_string(), Value::Bool(true));
    let control = child_object(root, "control");
    if let Some(public_url) = public_url {
        control.insert("publicUrl".to_string(), json!(public_url));
    }
    let hub = child_object(control, "hub");
    hub.insert("bind".to_string(), json!(bind));
    hub.insert("db".to_string(), json!(db.to_string_lossy()));
    hub.insert("nodeToken".to_string(), json!(node_token));
    hub.insert("clientToken".to_string(), json!(client_token));
    Ok(raw)
}

struct NodeSettings<'a> {
    hub_url: &'a str,
    id: &'a str,
    token: &'a str,
    workspace: Option<&'a Path>,
    claude_bin: &'a str,
    codex_bin: &'a str,
}

fn node_config(mut raw: Value, settings: &NodeSettings<'_>) -> Result<Value> {
    let root = object(&mut raw)?;
    child_object(root, "modules").insert("control".to_string(), Value::Bool(true));
    let control = child_object(root, "control");
    let node = child_object(control, "node");
    node.insert("hubUrl".to_string(), json!(settings.hub_url));
    node.insert("id".to_string(), json!(settings.id));
    node.insert("token".to_string(), json!(settings.token));
    if let Some(workspace) = settings.workspace {
        node.insert("workspace".to_string(), json!(workspace.to_string_lossy()));
    }
    node.insert("claudeBin".to_string(), json!(settings.claude_bin));
    node.insert("codexBin".to_string(), json!(settings.codex_bin));
    Ok(raw)
}

fn service_executable() -> Result<PathBuf> {
    let source = std::env::current_exe()
        .map_err(|error| Error::msg(format!("cannot locate the running binary: {error}")))?;
    let source = std::fs::canonicalize(&source).unwrap_or(source);
    let home =
        PathBuf::from(std::env::var("HOME").map_err(|_| Error::msg("HOME is required for installation"))?);
    let target = home.join(".local/bin/stackhour");
    if source != target {
        let bytes = std::fs::read(&source)
            .map_err(|error| Error::msg(format!("cannot read {}: {error}", source.display())))?;
        stackhour_core::fsutil::atomic_write(&target, &bytes, 0o755)
            .map_err(|error| Error::msg(format!("cannot install {}: {error}", target.display())))?;
    }
    Ok(target)
}

fn unit(role: &str, executable: &Path, config: &Path, path_env: &str) -> Result<String> {
    let command = match role {
        "hub" => "hub",
        "node" => "node",
        _ => return Err(Error::msg("control service role must be hub or node")),
    };
    let executable = systemd_quote(&executable.to_string_lossy())?;
    let config = systemd_quote(&format!("STACKHOUR_CONFIG={}", config.display()))?;
    let path_env = systemd_quote(&format!("PATH={path_env}"))?;
    Ok(format!(
        "[Unit]\nDescription=Stackhour control {role}\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nEnvironment={config}\nEnvironment={path_env}\nExecStart={executable} control {command}\nRestart=always\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n"
    ))
}

fn systemd_quote(value: &str) -> Result<String> {
    if value.contains('\r') || value.contains('\n') {
        return Err(Error::msg("service value contains a newline"));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn launchd_plist(role: &str, executable: &Path, config: &Path, path_env: &str) -> Result<String> {
    let command = match role {
        "hub" => "hub",
        "node" => "node",
        _ => return Err(Error::msg("control service role must be hub or node")),
    };
    let label = format!("com.stackhour.control-{role}");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"https://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{}</string>\n\
         <key>ProgramArguments</key><array><string>{}</string><string>control</string><string>{}</string></array>\n\
         <key>EnvironmentVariables</key><dict>\
         <key>STACKHOUR_CONFIG</key><string>{}</string>\
         <key>PATH</key><string>{}</string></dict>\n\
         <key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n\
         <key>StandardOutPath</key><string>/tmp/stackhour-control-{}.log</string>\n\
         <key>StandardErrorPath</key><string>/tmp/stackhour-control-{}.log</string>\n\
         </dict></plist>\n",
        xml(&label),
        xml(&executable.to_string_lossy()),
        command,
        xml(&config.to_string_lossy()),
        xml(path_env),
        role,
        role,
    ))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn install_service(role: &str, no_start: bool) -> Result<PathBuf> {
    let executable = service_executable()?;
    let config = config_path();
    let home = PathBuf::from(
        std::env::var("HOME").map_err(|_| Error::msg("HOME is required for service installation"))?,
    );
    let path_env = std::env::var("PATH").unwrap_or_else(|_| {
        format!(
            "{}:/usr/local/bin:/usr/bin:/bin",
            home.join(".local/bin").display()
        )
    });
    if cfg!(target_os = "linux") {
        let name = format!("stackhour-control-{role}.service");
        let path = home.join(".config/systemd/user").join(&name);
        stackhour_core::fsutil::atomic_write_0644(
            &path,
            unit(role, &executable, &config, &path_env)?.as_bytes(),
        )
        .map_err(|error| Error::msg(format!("cannot write {}: {error}", path.display())))?;
        if !no_start {
            run_status("systemctl", &["--user", "daemon-reload"])?;
            run_status("systemctl", &["--user", "enable", "--now", &name])?;
            let mut active = false;
            for _ in 0..10 {
                active = Command::new("systemctl")
                    .args(["--user", "is-active", "--quiet", &name])
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if active {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            if !active {
                let _ = Command::new("journalctl")
                    .args(["--user-unit", &name, "-n", "40", "--no-pager"])
                    .status();
                return Err(Error::msg(format!(
                    "{name} did not become active after installation"
                )));
            }
        }
        return Ok(path);
    }
    if cfg!(target_os = "macos") {
        let label = format!("com.stackhour.control-{role}");
        let path = home.join("Library/LaunchAgents").join(format!("{label}.plist"));
        stackhour_core::fsutil::atomic_write_0644(
            &path,
            launchd_plist(role, &executable, &config, &path_env)?.as_bytes(),
        )
        .map_err(|error| Error::msg(format!("cannot write {}: {error}", path.display())))?;
        if !no_start {
            let uid = rustix::process::getuid().as_raw();
            let domain = format!("gui/{uid}");
            let path_text = path.to_string_lossy().to_string();
            let target = format!("{domain}/{label}");
            let _ = run_status("launchctl", &["bootout", &domain, &path_text]);
            run_status("launchctl", &["bootstrap", &domain, &path_text])?;
            run_status("launchctl", &["enable", &target])?;
            run_status("launchctl", &["kickstart", "-k", &target])?;
            let healthy = Command::new("launchctl")
                .args(["print", &target])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !healthy {
                return Err(Error::msg(format!("{label} was not healthy after installation")));
            }
        }
        return Ok(path);
    }
    Err(Error::msg(format!(
        "automatic service installation is not supported on {}",
        std::env::consts::OS
    )))
}

fn run_status(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| Error::msg(format!("{program}: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::msg(format!(
            "{program} {} failed with {status}",
            args.join(" ")
        )))
    }
}

fn install_hub(args: &[String]) -> Result<()> {
    reject_unknown(
        args,
        &[
            "bind",
            "db",
            "public-url",
            "node-token",
            "client-token",
            "no-start",
        ],
    )?;
    let paths = stackhour_core::paths::resolve_storage_paths_from_process_env();
    let raw = load_raw(&paths.config_path)?;
    let bind = option(args, "bind")
        .or_else(|| raw_string(&raw, "/control/hub/bind"))
        .unwrap_or_else(|| "127.0.0.1:4050".to_string());
    bind.parse::<std::net::SocketAddr>()
        .map_err(|error| Error::msg(format!("invalid --bind: {error}")))?;
    let db = option(args, "db")
        .map(PathBuf::from)
        .or_else(|| raw_string(&raw, "/control/hub/db").map(PathBuf::from))
        .unwrap_or_else(|| paths.data_dir.join("control.db"));
    let public_url = option(args, "public-url").or_else(|| raw_string(&raw, "/control/publicUrl"));
    if let Some(url) = &public_url {
        validate_hub_url(url)?;
    }
    let existing_node_token = raw_string(&raw, "/control/hub/nodeToken");
    let existing_client_token = raw_string(&raw, "/control/hub/clientToken");
    let supplied_node_token = option(args, "node-token");
    let supplied_client_token = option(args, "client-token");
    let generated_node = supplied_node_token.is_none() && existing_node_token.is_none();
    let generated_client = supplied_client_token.is_none() && existing_client_token.is_none();
    let node_token = supplied_node_token
        .or(existing_node_token)
        .unwrap_or_else(stackhour_core::tokens::generate_token);
    let client_token = supplied_client_token
        .or(existing_client_token)
        .unwrap_or_else(stackhour_core::tokens::generate_token);
    let config = hub_config(raw, &bind, &db, public_url.as_deref(), &node_token, &client_token)?;
    write_config(&paths.config_path, &config)?;
    let unit = install_service("hub", flag(args, "no-start"))?;
    println!("Configured control hub: {}", paths.config_path.display());
    println!("Installed control hub service: {}", unit.display());
    if generated_client {
        println!("Client token: {client_token}");
    }
    if generated_node {
        println!("Node token: {node_token}");
    }
    Ok(())
}

fn validate_hub_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|error| Error::msg(format!("invalid hub URL: {error}")))?;
    if !matches!(parsed.scheme(), "ws" | "wss" | "http" | "https") {
        return Err(Error::msg("hub URL must use ws, wss, http, or https"));
    }
    if parsed.host_str().is_none() {
        return Err(Error::msg("hub URL must include a host"));
    }
    Ok(())
}

fn install_node(args: &[String]) -> Result<()> {
    reject_unknown(
        args,
        &[
            "hub-url",
            "id",
            "token",
            "workspace",
            "claude-bin",
            "codex-bin",
            "no-start",
        ],
    )?;
    let hub_url = required(args, "hub-url")?;
    validate_hub_url(&hub_url)?;
    let token = required_node_token(args)?;
    let id = option(args, "id")
        .unwrap_or_else(|| hostname::get().unwrap_or_default().to_string_lossy().to_string());
    validate_simple("node id", &id)?;
    let workspace = PathBuf::from(
        option(args, "workspace")
            .or_else(|| std::env::var("HOME").ok())
            .ok_or_else(|| Error::msg("--workspace is required when HOME is unavailable"))?,
    );
    let claude_bin = option(args, "claude-bin").unwrap_or_else(|| "claude".to_string());
    let codex_bin = option(args, "codex-bin").unwrap_or_else(|| "codex".to_string());
    let settings = NodeSettings {
        hub_url: &hub_url,
        id: &id,
        token: &token,
        workspace: Some(&workspace),
        claude_bin: &claude_bin,
        codex_bin: &codex_bin,
    };
    let path = config_path();
    write_config(&path, &node_config(load_raw(&path)?, &settings)?)?;
    let unit = install_service("node", flag(args, "no-start"))?;
    println!("Configured control node {id}: {}", path.display());
    println!("Installed control node service: {}", unit.display());
    Ok(())
}

fn validate_simple(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 200
        || value
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\'' | '"' | '`' | '$' | ';' | '|' | '&'))
    {
        return Err(Error::msg(format!("{label} contains unsafe characters")));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SshPlan {
    install: Vec<String>,
}

fn ssh_plan(
    target: &str,
    port: u16,
    identity: Option<&Path>,
    settings: &NodeSettings<'_>,
) -> Result<SshPlan> {
    validate_simple("SSH target", target)?;
    let mut ssh = vec![
        "-p".to_string(),
        port.to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
    ];
    if let Some(identity) = identity {
        let identity = identity.to_string_lossy().to_string();
        ssh.extend(["-i".to_string(), identity]);
    }
    let mut remote = vec![
        "$HOME/.local/bin/stackhour".to_string(),
        "control".to_string(),
        "install".to_string(),
        "node".to_string(),
        format!("--hub-url={}", shell_quote(settings.hub_url)),
        format!("--id={}", shell_quote(settings.id)),
        format!("--claude-bin={}", shell_quote(settings.claude_bin)),
        format!("--codex-bin={}", shell_quote(settings.codex_bin)),
    ];
    if let Some(workspace) = settings.workspace {
        remote.push(format!(
            "--workspace={}",
            shell_quote(&workspace.to_string_lossy())
        ));
    }
    let remote = remote.join(" ");
    ssh.extend([
        target.to_string(),
        format!(
            "set -eu; command -v curl >/dev/null 2>&1 || {{ echo 'curl is required on the SSH machine.' >&2; exit 1; }}; \
             installer_dir=$(mktemp -d \"${{TMPDIR:-/tmp}}/stackhour-bootstrap.XXXXXX\"); \
             trap 'rm -rf -- \"$installer_dir\"' EXIT HUP INT TERM; \
             curl --fail --location --silent --show-error {} --output \"$installer_dir/install.sh\"; \
             sh \"$installer_dir/install.sh\" </dev/null; \
             rm -rf -- \"$installer_dir\"; trap - EXIT HUP INT TERM; \
             IFS= read -r STACKHOUR_CONTROL_NODE_TOKEN; export STACKHOUR_CONTROL_NODE_TOKEN; exec {remote}",
            shell_quote(RELEASE_INSTALLER_URL)
        ),
    ]);
    Ok(SshPlan { install: ssh })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn run_owned_with_input(program: &str, args: &[String], input: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| Error::msg(format!("{program}: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| Error::msg(format!("{program}: cannot open standard input")))?
        .write_all(format!("{input}\n").as_bytes())
        .map_err(|error| Error::msg(format!("{program}: cannot write standard input: {error}")))?;
    let status = child
        .wait()
        .map_err(|error| Error::msg(format!("{program}: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::msg(format!("{program} failed with {status}")))
    }
}

fn install_ssh(args: &[String]) -> Result<()> {
    reject_unknown(
        args,
        &[
            "host",
            "user",
            "port",
            "identity",
            "hub-url",
            "id",
            "token",
            "workspace",
            "claude-bin",
            "codex-bin",
        ],
    )?;
    let host = required(args, "host")?;
    validate_simple("SSH host", &host)?;
    let user = option(args, "user");
    if let Some(user) = &user {
        validate_simple("SSH user", user)?;
    }
    let target = user.map(|user| format!("{user}@{host}")).unwrap_or(host);
    let port = option(args, "port")
        .unwrap_or_else(|| "22".to_string())
        .parse::<u16>()
        .map_err(|_| Error::msg("--port must be between 1 and 65535"))?;
    if port == 0 {
        return Err(Error::msg("--port must be between 1 and 65535"));
    }
    let hub_url = required(args, "hub-url")?;
    validate_hub_url(&hub_url)?;
    let id = option(args, "id").unwrap_or_else(|| target.replace('@', "-"));
    validate_simple("node id", &id)?;
    let token = required_node_token(args)?;
    let workspace = option(args, "workspace").map(PathBuf::from);
    let claude_bin = option(args, "claude-bin").unwrap_or_else(|| "claude".to_string());
    let codex_bin = option(args, "codex-bin").unwrap_or_else(|| "codex".to_string());
    let settings = NodeSettings {
        hub_url: &hub_url,
        id: &id,
        token: &token,
        workspace: workspace.as_deref(),
        claude_bin: &claude_bin,
        codex_bin: &codex_bin,
    };
    let identity = option(args, "identity").map(PathBuf::from);
    if let Some(identity) = &identity {
        if !identity.is_file() {
            return Err(Error::msg(format!(
                "SSH identity does not exist: {}",
                identity.display()
            )));
        }
    }
    let plan = ssh_plan(&target, port, identity.as_deref(), &settings)?;
    run_owned_with_input("ssh", &plan.install, &token)?;
    println!("Installed and started control node {id} on {target}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_config_preserves_unknown_keys_and_adds_control() {
        let value = hub_config(
            json!({"unknown": 7, "modules": {"tracker": false}}),
            "127.0.0.1:4050",
            Path::new("/data/control.db"),
            Some("https://control.example.com"),
            "node-secret",
            "client-secret",
        )
        .unwrap();
        assert_eq!(value["unknown"], 7);
        assert_eq!(value["modules"]["tracker"], false);
        assert_eq!(value["modules"]["control"], true);
        assert_eq!(value["control"]["hub"]["nodeToken"], "node-secret");
        assert_eq!(value["control"]["publicUrl"], "https://control.example.com");
    }

    #[test]
    fn raw_string_reads_only_non_empty_strings() {
        let value = json!({"control": {"hub": {"token": "keep", "blank": " "}}});
        assert_eq!(raw_string(&value, "/control/hub/token").as_deref(), Some("keep"));
        assert_eq!(raw_string(&value, "/control/hub/blank"), None);
        assert_eq!(raw_string(&value, "/control/hub/missing"), None);
    }

    #[test]
    fn node_config_preserves_hub_config() {
        let value = node_config(
            json!({"control": {"hub": {"bind": "x"}}}),
            &NodeSettings {
                hub_url: "wss://control.example.com/v1/node/connect",
                id: "laptop",
                token: "secret",
                workspace: Some(Path::new("/work")),
                claude_bin: "claude",
                codex_bin: "codex",
            },
        )
        .unwrap();
        assert_eq!(value["control"]["hub"]["bind"], "x");
        assert_eq!(value["control"]["node"]["id"], "laptop");
        assert_eq!(value["control"]["node"]["workspace"], "/work");
    }

    #[test]
    fn systemd_unit_contains_config_and_control_role() {
        let text = unit(
            "node",
            Path::new("/home/u/.local/bin/stackhour"),
            Path::new("/home/u/.config/stackhour/config.json"),
            "/home/u/.local/bin:/usr/bin:/bin",
        )
        .unwrap();
        assert!(text.contains("stackhour\" control node"));
        assert!(text.contains("STACKHOUR_CONFIG=/home/u/.config/stackhour/config.json"));
        assert!(text.contains("PATH=/home/u/.local/bin:/usr/bin:/bin"));
        assert!(text.contains("Restart=always"));
    }

    #[test]
    fn service_values_reject_newlines() {
        assert!(systemd_quote("x\nExecStart=bad").is_err());
    }

    #[test]
    fn ssh_plan_uses_strict_host_keys_and_the_remote_release_installer() {
        let settings = NodeSettings {
            hub_url: "wss://control.example.com/v1/node/connect",
            id: "devbox",
            token: "secret",
            workspace: Some(Path::new("/srv/work")),
            claude_bin: "claude",
            codex_bin: "codex",
        };
        let plan = ssh_plan("nikita@devbox", 2222, Some(Path::new("/keys/dev")), &settings).unwrap();
        assert!(plan.install.contains(&"StrictHostKeyChecking=yes".to_string()));
        assert!(plan.install.contains(&"/keys/dev".to_string()));
        assert!(plan.install.last().unwrap().contains("control install node"));
        assert!(plan.install.last().unwrap().contains(RELEASE_INSTALLER_URL));
        assert!(plan
            .install
            .last()
            .unwrap()
            .contains("sh \"$installer_dir/install.sh\" </dev/null"));
        assert!(!plan.install.last().unwrap().contains("secret"));
        assert!(plan
            .install
            .last()
            .unwrap()
            .contains("read -r STACKHOUR_CONTROL_NODE_TOKEN"));
    }

    #[test]
    fn launchd_supports_both_control_roles() {
        for role in ["hub", "node"] {
            let text = launchd_plist(
                role,
                Path::new("/Users/u/.local/bin/stackhour"),
                Path::new("/Users/u/.config/stackhour/config.json"),
                "/Users/u/.local/bin:/usr/bin:/bin",
            )
            .unwrap();
            assert!(text.contains(&format!("com.stackhour.control-{role}")));
            assert!(text.contains(&format!("<string>{role}</string></array>")));
            assert!(text.contains(&format!("/tmp/stackhour-control-{role}.log")));
        }
        assert!(launchd_plist(
            "bad",
            Path::new("/bin/stackhour"),
            Path::new("/tmp/config.json"),
            "/usr/bin:/bin",
        )
        .is_err());
    }

    #[test]
    fn unsafe_ssh_identifiers_are_rejected() {
        assert!(validate_simple("host", "host; reboot").is_err());
        assert!(validate_simple("host", "host\nbad").is_err());
        assert!(validate_simple("host", "safe-host").is_ok());
    }

    #[test]
    fn unknown_options_fail_before_installation() {
        assert!(reject_unknown(&["--wat=yes".to_string()], &["id"]).is_err());
        assert!(reject_unknown(&["bare".to_string()], &["id"]).is_err());
    }

    #[test]
    fn hub_url_requires_a_supported_scheme_and_host() {
        assert!(validate_hub_url("wss://control.example.com/x").is_ok());
        assert!(validate_hub_url("file:///tmp/x").is_err());
        assert!(validate_hub_url("not a url").is_err());
    }

    #[test]
    fn installer_help_names_all_three_install_modes() {
        assert!(HELP.contains("control install hub"));
        assert!(HELP.contains("control install node"));
        assert!(HELP.contains("control install ssh"));
        assert!(HELP.contains("STACKHOUR_CONTROL_NODE_TOKEN"));
    }
}
