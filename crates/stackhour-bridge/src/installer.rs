//! `stackhour bridge install <role>` + the bridge CLI dispatcher.
//!
//! Arg parsing (--runtime-dir/--reconfigure/--no-start/--non-interactive/
//! -h); interactive prompts (env-var-first non-interactive path with exact
//! required-var errors validated BEFORE any write; raw-mode secret echo '*'
//! with backspace; Ctrl-C -> 'Setup cancelled.'; find_executable PATH
//! search); config validation-before-write; timestamped 0600 backups.
//! Runtime install = copy the running stackhour binary into the runtime dir
//! and writes claim.mjs/return.mjs/tg-send.mjs shims (0755): each shim is a
//! VALID NODE SCRIPT (child_process.spawnSync of the adjacent stackhour
//! binary with the matching hidden verb, stdio inherit, exit-code forward)
//! because the JS worker invokes `<remoteNode> <remoteDir>/claim.mjs` with
//! node as the interpreter — this keeps both mixed Node/Rust pairings
//! working. Unit/plist writes use config.rs render_* (ExecStart = the
//! installed stackhour binary with `bridge coordinator|worker`), then the
//! systemctl/launchctl sequences + linger note + plutil lint.

use serde_json::{json, Value};
use stackhour_core::fsutil;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::config::{
    self, validate_coordinator_config, validate_worker_config, LAUNCHD_LABEL, SERVICE_NAME,
};

/// The usage banner, byte for byte the Node `usage()` template (cli.mjs).
pub const USAGE: &str = "stackhour bridge — install and operate the Telegram Claude + Codex bridge\n\
                         \n\
                         Usage:\n\
                         \x20 stackhour bridge install <coordinator|worker> [--runtime-dir PATH] [--reconfigure] [--no-start]\n\
                         \x20 stackhour bridge doctor <coordinator|worker> [--runtime-dir PATH]\n\
                         \x20 stackhour bridge status <coordinator|worker>\n\
                         \x20 stackhour bridge restart <coordinator|worker>\n\
                         \n\
                         Non-interactive setup:\n\
                         \x20 Add --non-interactive and provide the environment variables documented in README.md.";

/// The flags cli.mjs declares to `parseArgs`, plus the positional tail.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BridgeArgs {
    pub runtime_dir: Option<String>,
    pub reconfigure: bool,
    pub no_start: bool,
    pub non_interactive: bool,
    pub help: bool,
    pub positionals: Vec<String>,
}

/// Parse the `stackhour bridge …` argument tail.
///
/// Mirrors Node's strict `parseArgs`: flags may appear anywhere, `--help`
/// has the `-h` short form, `--runtime-dir` takes a value (either
/// `--runtime-dir PATH` or `--runtime-dir=PATH`), and an unknown option is
/// an error rather than a positional.
pub fn parse_bridge_args(args: &[String]) -> Result<BridgeArgs, String> {
    let mut out = BridgeArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--runtime-dir" => match it.next() {
                Some(v) => out.runtime_dir = Some(v.clone()),
                None => return Err("Option '--runtime-dir <value>' argument missing".into()),
            },
            "--reconfigure" => out.reconfigure = true,
            "--no-start" => out.no_start = true,
            "--non-interactive" => out.non_interactive = true,
            "--help" | "-h" => out.help = true,
            other if other.starts_with("--runtime-dir=") => {
                out.runtime_dir = Some(other["--runtime-dir=".len()..].to_string());
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("Unknown option '{other}'"));
            }
            other => out.positionals.push(other.to_string()),
        }
    }
    Ok(out)
}

/// `values['runtime-dir'] || $STACKHOUR_BRIDGE_HOME || ~/.local/share/stackhour/bridge`
/// — JS `||`, so an empty flag or env value falls through.
pub fn resolve_runtime_dir(flag: Option<&str>) -> PathBuf {
    if let Some(dir) = flag.filter(|v| !v.trim().is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    crate::BridgePaths::resolve(&|k| std::env::var(k).ok(), &home).runtime_dir
}

/// The `stackhour bridge …` CLI entry: dispatches
/// install|doctor|status|restart (the daemon and wire verbs —
/// coordinator|worker|claim|return|tg-send — are routed by main.rs before it
/// falls through to here, exactly as cli.js hands everything after `bridge`
/// to `runBridgeCli`). Returns the process exit code.
pub fn run_bridge_cli(args: &[String]) -> i32 {
    let parsed = match parse_bridge_args(args) {
        // PARITY (deliberate): Node's parseArgs throws OUTSIDE runBridgeCli's
        // try/catch, so an unknown flag dies with an uncaught stack trace and
        // exit 1. Same exit code here, minus the stack trace.
        Err(message) => {
            eprintln!("\nError: {message}");
            return 1;
        }
        Ok(p) => p,
    };
    if parsed.help {
        println!("{USAGE}");
        return 0;
    }
    let command = parsed.positionals.first().map(String::as_str).unwrap_or("");
    let role = parsed.positionals.get(1).map(String::as_str).unwrap_or("");
    if !matches!(command, "install" | "doctor" | "status" | "restart")
        || !matches!(role, "coordinator" | "worker")
    {
        eprintln!("{USAGE}");
        return 1;
    }
    match command {
        "install" => run_install(role, args),
        "doctor" => {
            crate::doctor::run_doctor(role, &resolve_runtime_dir(parsed.runtime_dir.as_deref()))
        }
        verb => crate::doctor::run_service_cmd(role, verb),
    }
}

/// The install verb itself (split for testability). Returns the exit code.
pub fn run_install(role: &str, args: &[String]) -> i32 {
    let parsed = match parse_bridge_args(args) {
        Err(message) => {
            eprintln!("\nError: {message}");
            return 1;
        }
        Ok(p) => p,
    };
    let opts = InstallOpts {
        runtime_dir: resolve_runtime_dir(parsed.runtime_dir.as_deref()),
        reconfigure: parsed.reconfigure,
        no_start: parsed.no_start,
        non_interactive: parsed.non_interactive,
    };
    match install(role, &opts) {
        Ok(()) => 0,
        // The Node `catch (error)` in runBridgeCli, verbatim.
        Err(message) => {
            eprintln!("\nError: {message}");
            1
        }
    }
}

/// What `install` was asked to do, resolved from argv + env.
#[derive(Debug, Clone)]
pub struct InstallOpts {
    pub runtime_dir: PathBuf,
    pub reconfigure: bool,
    pub no_start: bool,
    pub non_interactive: bool,
}

/// `install(roleName)`: reuse-or-prompt the config (STRICT-validated before
/// any write), install the runtime, install the service, print the recap.
fn install(role: &str, opts: &InstallOpts) -> Result<(), String> {
    let env = |k: &str| std::env::var(k).ok();
    let mut prompter = StdPrompter;
    let mut wiz = Wizard {
        env: &env,
        non_interactive: opts.non_interactive,
        prompter: &mut prompter,
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        home: PathBuf::from(std::env::var("HOME").unwrap_or_default()),
    };

    let name = if role == "coordinator" { "config.json" } else { "worker-config.json" };
    let path = opts.runtime_dir.join(name);
    // `loadConfig` — a config that exists but does not parse throws, caught
    // by the shared `\nError:` printer.
    let existing: Option<Value> = if path.exists() {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Some(serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?)
    } else {
        None
    };
    let validate = if role == "coordinator" {
        validate_coordinator_config
    } else {
        validate_worker_config
    };

    let cfg = match existing {
        Some(cfg) if !opts.reconfigure => {
            let errors = validate(&cfg);
            if !errors.is_empty() {
                return Err(format!("Existing config is invalid:\n- {}", errors.join("\n- ")));
            }
            println!("Reusing {}; pass --reconfigure to replace it.", path.display());
            cfg
        }
        _ => {
            let cfg = if role == "coordinator" {
                wiz.coordinator_config()?
            } else {
                wiz.worker_config()?
            };
            let errors = validate(&cfg);
            if !errors.is_empty() {
                return Err(errors.join("\n"));
            }
            save_config(&path, &cfg)?;
            cfg
        }
    };

    copy_runtime(role, &opts.runtime_dir)?;
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    if role == "coordinator" {
        install_coordinator_service(&cfg, &opts.runtime_dir, &home, !opts.no_start)?;
    } else {
        install_worker_service(&cfg, &opts.runtime_dir, &home, !opts.no_start)?;
    }
    println!("\n✓ {role} installed in {}", opts.runtime_dir.display());
    println!("  Run: stackhour bridge doctor {role}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

/// The two terminal interactions the install wizard performs, injected so
/// tests script answers instead of owning a TTY (the same seam style as
/// `run_install_into` in the core installer).
pub trait Prompter {
    /// `ask()` — visible prompt with an optional default; an empty answer
    /// takes the default.
    fn ask(&mut self, label: &str, default: &str) -> Result<String, String>;
    /// `askSecret()` — raw-mode masked prompt ('*' echo, backspace, Ctrl-C
    /// -> 'Setup cancelled.').
    fn ask_secret(&mut self, label: &str, env_name: &str, optional: bool) -> Result<String, String>;
}

/// The real terminal prompter.
pub struct StdPrompter;

impl Prompter for StdPrompter {
    fn ask(&mut self, label: &str, default: &str) -> Result<String, String> {
        let suffix = if default.is_empty() { String::new() } else { format!(" [{default}]") };
        print!("{label}{suffix}: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
            .map_err(|e| e.to_string())?;
        let answer = line.trim().to_string();
        Ok(if answer.is_empty() { default.to_string() } else { answer })
    }

    fn ask_secret(&mut self, label: &str, env_name: &str, optional: bool) -> Result<String, String> {
        #[cfg(unix)]
        {
            read_secret_raw(label, env_name, optional)
        }
        #[cfg(not(unix))]
        {
            let _ = optional;
            Err(format!("Cannot read {label} securely here; set {env_name} and retry."))
        }
    }
}

/// `askSecret`'s raw-mode body: echo '*' per character, handle backspace,
/// treat Ctrl-C as 'Setup cancelled.'. Requires both stdin and stdout to be
/// TTYs, with the Node error text otherwise.
#[cfg(unix)]
fn read_secret_raw(label: &str, env_name: &str, optional: bool) -> Result<String, String> {
    use std::io::Read as _;
    let tty = unsafe {
        libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1
    };
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    if !tty || unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } != 0 {
        return Err(format!("Cannot read {label} securely here; set {env_name} and retry."));
    }
    print!("{label}{}: ", if optional { " (optional)" } else { "" });
    let _ = std::io::stdout().flush();

    let saved = term;
    // Node's `setRawMode(true)`: no echo, no line buffering, no signal keys;
    // output post-processing stays on so the final '\n' still works.
    term.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
    term.c_iflag &= !(libc::IXON | libc::ICRNL);
    unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) };

    let mut bytes: Vec<u8> = Vec::new();
    let outcome = loop {
        let mut byte = [0u8; 1];
        match std::io::stdin().read(&mut byte) {
            Ok(0) => break Ok(()),
            Err(e) => break Err(e.to_string()),
            Ok(_) => {}
        }
        match byte[0] {
            0x03 => break Err("Setup cancelled.".to_string()),
            b'\r' | b'\n' => break Ok(()),
            0x7f | 0x08 => {
                if !bytes.is_empty() {
                    // Pop one CHARACTER: drop trailing UTF-8 continuation
                    // bytes, then the lead byte, and erase one '*'.
                    while let Some(last) = bytes.pop() {
                        if last & 0xC0 != 0x80 {
                            break;
                        }
                    }
                    print!("\x08 \x08");
                    let _ = std::io::stdout().flush();
                }
            }
            b if b >= b' ' => {
                // One '*' per character, as the reference echoes — UTF-8
                // continuation bytes join the value silently.
                let continuation = b & 0xC0 == 0x80;
                bytes.push(b);
                if !continuation {
                    print!("*");
                    let _ = std::io::stdout().flush();
                }
            }
            _ => {}
        }
    };

    unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved) };
    println!();
    outcome?;
    let value = String::from_utf8_lossy(&bytes).to_string();
    if value.is_empty() && !optional {
        return Err(format!("{label} is required."));
    }
    Ok(value)
}

/// The prompt/env plumbing every wizard question goes through.
pub struct Wizard<'a> {
    /// Env lookup (injectable, like `BridgePaths::resolve`).
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub non_interactive: bool,
    pub prompter: &'a mut dyn Prompter,
    /// `process.cwd()` — the BRIDGE_WORKDIR default.
    pub cwd: String,
    /// `homedir()` — seeds the worker's SSH-key default.
    pub home: PathBuf,
}

impl Wizard<'_> {
    /// `ask()` — non-interactive silently takes the default.
    fn ask(&mut self, label: &str, default: &str) -> Result<String, String> {
        if self.non_interactive {
            return Ok(default.to_string());
        }
        self.prompter.ask(label, default)
    }

    /// `askRequired()` — `process.env[envName] || await ask(...)`, JS `||`,
    /// so a set-but-empty env var falls through to the prompt.
    fn ask_required(&mut self, label: &str, env_name: &str, default: &str) -> Result<String, String> {
        let value = match (self.env)(env_name).filter(|v| !v.is_empty()) {
            Some(v) => v,
            None => self.ask(label, default)?,
        };
        if value.is_empty() {
            return Err(format!(
                "{label} is required (set {env_name} for non-interactive setup)."
            ));
        }
        Ok(value)
    }

    /// `askSecret()` — the env check is `!== undefined`, so a SET-but-empty
    /// env var is used as-is (and later rejected by the strict validator).
    fn ask_secret(&mut self, label: &str, env_name: &str, optional: bool) -> Result<String, String> {
        if let Some(v) = (self.env)(env_name) {
            return Ok(v);
        }
        if self.non_interactive {
            if optional {
                return Ok(String::new());
            }
            return Err(format!("{env_name} is required for non-interactive setup."));
        }
        self.prompter.ask_secret(label, env_name, optional)
    }

    /// `executablePrompt()` — env/PATH detection for the default, then the
    /// answer must resolve to something executable.
    fn executable_prompt(
        &mut self,
        label: &str,
        env_name: &str,
        command_name: &str,
    ) -> Result<String, String> {
        let detected = (self.env)(env_name)
            .filter(|v| !v.is_empty())
            .or_else(|| config::find_executable(command_name).map(|p| p.display().to_string()))
            .unwrap_or_default();
        let value = self.ask_required(label, env_name, &detected)?;
        match config::find_executable(&value) {
            Some(resolved) => Ok(resolved.display().to_string()),
            None => Err(format!("{label} is not executable: {value}")),
        }
    }

    /// The shared permission-mode question + gate.
    fn permission_mode(&mut self) -> Result<String, String> {
        let default = (self.env)("BRIDGE_PERMISSION_MODE")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "default".into());
        let mode = self.ask("Permission mode (default or bypassPermissions)", &default)?;
        if mode != "default" && mode != "bypassPermissions" {
            return Err("Invalid permission mode.".to_string());
        }
        Ok(mode)
    }

    /// `coordinatorConfig()` — question order, key order and defaults are the
    /// Node's, including `defaultTarget: 'gcp'` and the 512 MiB media cap.
    pub fn coordinator_config(&mut self) -> Result<Value, String> {
        let token = self.ask_secret("Telegram bot token", "TELEGRAM_BOT_TOKEN", false)?;
        let raw_chat_id = self.ask_required("Authorized Telegram chat ID", "TELEGRAM_CHAT_ID", "")?;
        let chat_id =
            js_safe_integer(&raw_chat_id).ok_or("Telegram chat ID must be an integer.")?;
        let cwd_default = self.cwd.clone();
        let cwd = self.ask_required("Linux agent working directory", "BRIDGE_WORKDIR", &cwd_default)?;
        ensure_workdir(&cwd)?;
        let claude_bin = self.executable_prompt("Claude Code executable", "CLAUDE_BIN", "claude")?;
        let codex_bin = self.executable_prompt("Codex executable", "CODEX_BIN", "codex")?;
        let permission_mode = self.permission_mode()?;
        let eleven_labs_api_key = self.ask_secret("ElevenLabs API key", "ELEVENLABS_API_KEY", true)?;
        let path_env = (self.env)("PATH").unwrap_or_default();
        let extra_path = config::merged_path(&[
            &binary_path(&claude_bin),
            &binary_path(&codex_bin),
            &path_env,
        ]);
        Ok(json!({
            "token": token,
            "chatId": chat_id,
            "defaultTarget": "gcp",
            "maxMediaBytes": 512u64 * 1024 * 1024,
            "elevenLabsApiKey": eleven_labs_api_key,
            "targets": {
                "gcp": {
                    "label": "Linux",
                    "type": "local",
                    "cwd": cwd,
                    "claudeBin": claude_bin,
                    "codexBin": codex_bin,
                    "extraPath": extra_path,
                    "permissionMode": permission_mode,
                    "model": null,
                    "codexModel": null
                },
                "mac": { "label": "Mac", "type": "remote", "permissionMode": permission_mode }
            }
        }))
    }

    /// `workerConfig()` — macOS-only, SSH-key readability checked up front.
    pub fn worker_config(&mut self) -> Result<Value, String> {
        if !cfg!(target_os = "macos") {
            return Err("The worker installer currently targets macOS.".to_string());
        }
        let gcp_ssh = self.ask_required("Linux SSH destination (user@host)", "BRIDGE_GCP_SSH", "")?;
        let ssh_user = if gcp_ssh.contains('@') {
            gcp_ssh.split('@').next().unwrap_or_default().to_string()
        } else {
            username()
        };
        let default_remote = if ssh_user == "root" {
            "/root/.local/share/stackhour/bridge".to_string()
        } else {
            format!("/home/{ssh_user}/.local/share/stackhour/bridge")
        };
        let key_default = self.home.join(".ssh").join("id_ed25519").display().to_string();
        let gcp_key = self.ask_required("SSH private key", "BRIDGE_GCP_KEY", &key_default)?;
        // `accessSync(gcpKey, R_OK)` — an open() probe is the same question.
        if std::fs::File::open(&gcp_key).is_err() {
            return Err(format!("SSH key is not readable: {gcp_key}"));
        }
        let remote_dir =
            self.ask_required("Remote bridge runtime directory", "BRIDGE_REMOTE_DIR", &default_remote)?;
        let remote_node =
            self.ask_required("Remote Node.js executable", "BRIDGE_REMOTE_NODE", "/usr/local/bin/node")?;
        let cwd_default = self.cwd.clone();
        let cwd = self.ask_required("Mac agent working directory", "BRIDGE_WORKDIR", &cwd_default)?;
        ensure_workdir(&cwd)?;
        let claude_bin = self.executable_prompt("Claude Code executable", "CLAUDE_BIN", "claude")?;
        let codex_bin = self.executable_prompt("Codex executable", "CODEX_BIN", "codex")?;
        let permission_mode = self.permission_mode()?;
        let path_env = (self.env)("PATH").unwrap_or_default();
        Ok(json!({
            "gcpSsh": gcp_ssh,
            "gcpKey": gcp_key,
            "remoteDir": remote_dir,
            "remoteNode": remote_node,
            "claudeBin": claude_bin,
            "codexBin": codex_bin,
            "cwd": cwd,
            "extraPath": config::merged_path(&[
                &binary_path(&claude_bin),
                &binary_path(&codex_bin),
                &path_env,
            ]),
            "permissionMode": permission_mode,
            "model": null,
            "codexModel": null
        }))
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// `binaryPath` — dirname when absolute, `''` otherwise.
fn binary_path(binary: &str) -> String {
    if Path::new(binary).is_absolute() {
        Path::new(binary)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    }
}

/// `ensureWorkdir` — must exist and be a directory.
fn ensure_workdir(path: &str) -> Result<(), String> {
    if std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false) {
        return Ok(());
    }
    Err(format!("Working directory does not exist: {path}"))
}

/// `Number(raw)` + `Number.isSafeInteger` for the chat-id answer.
fn js_safe_integer(raw: &str) -> Option<i64> {
    let n = stackhour_core::jsnum::js_number(&Value::String(raw.to_string()));
    if !n.is_finite() || n.fract() != 0.0 || n.abs() > 9_007_199_254_740_991.0 {
        return None;
    }
    Some(n as i64)
}

/// PARITY: `userInfo().username`; `$USER`/`$LOGNAME` is what that reads on
/// every box this runs on.
fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default()
}

/// `ensureDir` — mkdir -p then chmod, applied to a pre-existing dir too.
fn ensure_dir(path: &Path, mode: u32) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// `run()` with stdio inherit: an io failure surfaces its own message, a
/// non-zero exit is the Node wording `"<program> <args> failed."`.
pub(crate) fn run_cmd(program: &str, args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("{program}: {e}"))?;
    if !status.success() {
        return Err(format!("{program} {} failed.", args.join(" ")));
    }
    Ok(())
}

/// `getuid()` for the launchd `gui/<uid>` domain.
#[cfg(unix)]
pub(crate) fn uid() -> u32 {
    unsafe { libc::getuid() }
}
#[cfg(not(unix))]
pub(crate) fn uid() -> u32 {
    0
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// `saveConfig` — timestamped 0600 backup, then an atomic 0600 pretty-JSON
/// write with a trailing newline (`JSON.stringify(config, null, 2) + '\n'`).
pub fn save_config(path: &Path, config: &Value) -> Result<(), String> {
    if path.exists() {
        // `toISOString().replace(/[:.]/g, '-')`.
        let stamp = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string()
            .replace([':', '.'], "-");
        let backup = PathBuf::from(format!("{}.backup-{stamp}", path.display()));
        std::fs::copy(path, &backup).map_err(|e| format!("{}: {e}", backup.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("{}: {e}", backup.display()))?;
        }
        println!("Backed up existing config to {}", backup.display());
    }
    let mut body =
        serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    body.push('\n');
    fsutil::atomic_write_0600(path, body.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// `copyRuntime` — the Rust runtime install.
///
/// PARITY (deliberate divergence): the Node installer copies
/// coordinator.mjs/worker.mjs plus helpers into the runtime dir. The Rust
/// runtime is ONE binary, so this installs the *running* stackhour binary,
/// canonicalized — the same convention as `stackhour install`: whichever
/// runtime the user invoked is the one that gets installed — plus
/// node-runnable shims (claim/return/tg-send for the coordinator, tg-send
/// for the worker) so a Node counterpart can keep calling
/// `<remoteNode> <remoteDir>/claim.mjs` over SSH unchanged.
pub fn copy_runtime(role: &str, runtime_dir: &Path) -> Result<(), String> {
    ensure_dir(runtime_dir, 0o700)?;
    install_binary(runtime_dir)?;
    let shims: &[&str] = if role == "coordinator" {
        &["claim", "return", "tg-send"]
    } else {
        &["tg-send"]
    };
    for verb in shims {
        let dest = runtime_dir.join(format!("{verb}.mjs"));
        fsutil::atomic_write(&dest, node_shim(verb).as_bytes(), 0o755)
            .map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    Ok(())
}

/// Copy the running (canonicalized) binary to `<runtime_dir>/stackhour`.
fn install_binary(runtime_dir: &Path) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate the running binary: {e}"))?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dest = runtime_dir.join("stackhour");
    // `realpathOrSelf(source) !== realpathOrSelf(destination)` — re-running
    // the installer from the installed binary must not copy onto itself.
    let same = std::fs::canonicalize(&dest).map(|d| d == exe).unwrap_or(false);
    if !same {
        // tmp+rename, NOT copy-in-place: overwriting a binary the service is
        // executing is ETXTBSY on Linux; a rename just swaps the inode.
        let bytes = std::fs::read(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
        fsutil::atomic_write(&dest, &bytes, 0o755).map_err(|e| format!("{}: {e}", dest.display()))?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("{}: {e}", dest.display()))?;
        }
    }
    Ok(dest)
}

/// One node-invokable shim body.
///
/// claim/return pin `--runtime-dir` to the shim's own directory (the Node
/// scripts derive their dir from `import.meta.url` the same way); tg-send
/// instead pins `CLAUDE_REMOTE_CONFIG` at the adjacent config.json, which is
/// where tg-send.mjs read its credentials.
pub fn node_shim(verb: &str) -> String {
    let argv = match verb {
        // claim forwards its argv (the optional target for `bridge claim
        // <target>`); the ORIGINAL Node claim.mjs ignored argv, so this line
        // is what lets a targeted worker actually filter.
        "claim" => "['bridge', 'claim', ...process.argv.slice(2), '--runtime-dir', here]",
        "return" => "['bridge', 'return', ...process.argv.slice(2), '--runtime-dir', here]",
        _ => "['bridge', 'tg-send', ...process.argv.slice(2)]",
    };
    let env = if verb == "tg-send" {
        "{ ...process.env, CLAUDE_REMOTE_CONFIG: process.env.CLAUDE_REMOTE_CONFIG || join(here, 'config.json') }"
    } else {
        "process.env"
    };
    format!(
        "#!/usr/bin/env node\n\
         // Written by `stackhour bridge install` — a node-invokable shim around the\n\
         // adjacent stackhour binary, kept so a Node counterpart can keep running\n\
         // `node {verb}.mjs` while this side runs the Rust port.\n\
         import {{ spawnSync }} from 'node:child_process';\n\
         import {{ dirname, join }} from 'node:path';\n\
         import {{ fileURLToPath }} from 'node:url';\n\
         const here = dirname(fileURLToPath(import.meta.url));\n\
         const result = spawnSync(join(here, 'stackhour'), {argv}, {{ stdio: 'inherit', env: {env} }});\n\
         process.exit(result.status ?? 1);\n"
    )
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

/// `installCoordinatorService` — user systemd unit + daemon-reload +
/// enable --now + the linger note.
fn install_coordinator_service(
    config: &Value,
    runtime_dir: &Path,
    home: &Path,
    start: bool,
) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("The coordinator service installer requires Linux with systemd.".to_string());
    }
    if config::find_executable("systemctl").is_none() {
        return Err("systemctl was not found.".to_string());
    }
    // PARITY (deliberate divergence): the Node installer also demanded a node
    // binary ('Node.js was not found.') because its unit ran
    // `node coordinator.mjs`. This unit execs the installed stackhour binary,
    // so node is not a hard requirement here; `bridge doctor` still probes it
    // for the claim/return shims' sake.
    let unit_dir = home.join(".config").join("systemd").join("user");
    ensure_dir(&unit_dir, 0o700)?;
    let unit_path = unit_dir.join(SERVICE_NAME);
    let exec = runtime_dir.join("stackhour");
    let extra_path = config
        .pointer("/targets/gcp/extraPath")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let unit = config::render_systemd_unit(
        &exec.display().to_string(),
        &runtime_dir.display().to_string(),
        &home.display().to_string(),
        extra_path,
    )
    .map_err(|e| e.message().to_string())?;
    fsutil::atomic_write_0600(&unit_path, unit.as_bytes())
        .map_err(|e| format!("{}: {e}", unit_path.display()))?;
    run_cmd("systemctl", &["--user", "daemon-reload"])?;
    if start {
        run_cmd("systemctl", &["--user", "enable", "--now", SERVICE_NAME])?;
    }
    println!("Installed user service: {}", unit_path.display());
    if config::find_executable("loginctl").is_some() {
        let user = username();
        // allowFailure + captured output: any failure just means "not yes".
        let lingering = std::process::Command::new("loginctl")
            .args(["show-user", &user, "-p", "Linger", "--value"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
            .unwrap_or(false);
        if !lingering {
            println!(
                "Note: run \"sudo loginctl enable-linger {user}\" once to keep the coordinator running after logout."
            );
        }
    }
    Ok(())
}

/// `installWorkerService` — LaunchAgent plist + plutil lint +
/// bootout(ignored)/bootstrap/kickstart.
fn install_worker_service(
    config: &Value,
    runtime_dir: &Path,
    home: &Path,
    start: bool,
) -> Result<(), String> {
    let agents = home.join("Library").join("LaunchAgents");
    ensure_dir(&agents, 0o755)?;
    let plist_path = agents.join(format!("{LAUNCHD_LABEL}.plist"));
    let exec = runtime_dir.join("stackhour");
    let extra_path = config.get("extraPath").and_then(Value::as_str).unwrap_or_default();
    let plist = config::render_launch_agent(
        &exec.display().to_string(),
        &runtime_dir.display().to_string(),
        &home.display().to_string(),
        extra_path,
    );
    fsutil::atomic_write_0600(&plist_path, plist.as_bytes())
        .map_err(|e| format!("{}: {e}", plist_path.display()))?;
    let plist_str = plist_path.display().to_string();
    run_cmd("plutil", &["-lint", &plist_str])?;
    if start {
        let domain = format!("gui/{}", uid());
        // bootout is allowFailure: it fails when nothing is loaded yet.
        let _ = run_cmd("launchctl", &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
        run_cmd("launchctl", &["bootstrap", &domain, &plist_str])?;
        run_cmd("launchctl", &["kickstart", "-k", &format!("{domain}/{LAUNCHD_LABEL}")])?;
    }
    println!("Installed LaunchAgent: {}", plist_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    /// A prompter that must never be reached: proves the non-interactive path
    /// resolves everything from env vars alone.
    struct NoPrompts;
    impl Prompter for NoPrompts {
        fn ask(&mut self, label: &str, _default: &str) -> Result<String, String> {
            panic!("non-interactive setup asked interactively: {label}");
        }
        fn ask_secret(&mut self, label: &str, _env: &str, _optional: bool) -> Result<String, String> {
            panic!("non-interactive setup asked for a secret: {label}");
        }
    }

    /// Scripted answers, popped in question order; an empty answer takes the
    /// default, exactly like pressing Enter.
    struct Scripted {
        answers: VecDeque<&'static str>,
        secrets: VecDeque<&'static str>,
    }
    impl Prompter for Scripted {
        fn ask(&mut self, label: &str, default: &str) -> Result<String, String> {
            let answer = self.answers.pop_front().unwrap_or_else(|| panic!("no scripted answer for {label}"));
            Ok(if answer.is_empty() { default.to_string() } else { answer.to_string() })
        }
        fn ask_secret(&mut self, label: &str, _env: &str, optional: bool) -> Result<String, String> {
            let answer = self.secrets.pop_front().unwrap_or_else(|| panic!("no scripted secret for {label}"));
            if answer.is_empty() && !optional {
                return Err(format!("{label} is required."));
            }
            Ok(answer.to_string())
        }
    }

    /// A 0755 stand-in executable.
    fn fake_bin(dir: &Path, name: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p.display().to_string()
    }

    // ---- usage + argument parsing ---------------------------------------

    /// The banner is a user-visible contract shared with the Node CLI.
    #[test]
    fn the_usage_banner_matches_the_node_template() {
        assert_eq!(
            USAGE,
            "stackhour bridge — install and operate the Telegram Claude + Codex bridge\n\
             \n\
             Usage:\n\
             \x20 stackhour bridge install <coordinator|worker> [--runtime-dir PATH] [--reconfigure] [--no-start]\n\
             \x20 stackhour bridge doctor <coordinator|worker> [--runtime-dir PATH]\n\
             \x20 stackhour bridge status <coordinator|worker>\n\
             \x20 stackhour bridge restart <coordinator|worker>\n\
             \n\
             Non-interactive setup:\n\
             \x20 Add --non-interactive and provide the environment variables documented in README.md."
        );
    }

    #[test]
    fn flags_parse_in_any_position_with_both_runtime_dir_forms() {
        let p = parse_bridge_args(&argv(&[
            "install", "--runtime-dir", "/rt", "coordinator", "--no-start", "--non-interactive",
        ]))
        .unwrap();
        assert_eq!(p.positionals, vec!["install", "coordinator"]);
        assert_eq!(p.runtime_dir.as_deref(), Some("/rt"));
        assert!(p.no_start && p.non_interactive && !p.reconfigure && !p.help);

        let p = parse_bridge_args(&argv(&["doctor", "worker", "--runtime-dir=/x", "--reconfigure"])).unwrap();
        assert_eq!(p.runtime_dir.as_deref(), Some("/x"));
        assert!(p.reconfigure);

        for help in [["-h"], ["--help"]] {
            assert!(parse_bridge_args(&argv(&help)).unwrap().help);
        }
    }

    #[test]
    fn unknown_options_and_a_missing_runtime_dir_value_are_errors() {
        assert!(parse_bridge_args(&argv(&["install", "--frobnicate"]))
            .unwrap_err()
            .contains("Unknown option '--frobnicate'"));
        assert!(parse_bridge_args(&argv(&["install", "coordinator", "--runtime-dir"]))
            .unwrap_err()
            .contains("argument missing"));
    }

    /// An explicit flag beats env beats the default, with JS `||` falsiness.
    #[test]
    fn an_empty_runtime_dir_flag_falls_through() {
        assert_eq!(resolve_runtime_dir(Some("/explicit")), PathBuf::from("/explicit"));
        // "" is falsy in `values['runtime-dir'] || ...`.
        let fallback = resolve_runtime_dir(Some(""));
        assert_ne!(fallback, PathBuf::from(""));
    }

    // ---- non-interactive validation (before any write) -------------------

    fn wizard_err(pairs: &[(&str, &str)]) -> String {
        let env = env_of(pairs);
        let mut prompter = NoPrompts;
        let mut wiz = Wizard {
            env: &env,
            non_interactive: true,
            prompter: &mut prompter,
            cwd: "/".into(),
            home: PathBuf::from("/h"),
        };
        wiz.coordinator_config().expect_err("must reject")
    }

    #[test]
    fn the_missing_env_var_errors_match_the_node_wording() {
        assert_eq!(
            wizard_err(&[]),
            "TELEGRAM_BOT_TOKEN is required for non-interactive setup."
        );
        assert_eq!(
            wizard_err(&[("TELEGRAM_BOT_TOKEN", "t")]),
            "Authorized Telegram chat ID is required (set TELEGRAM_CHAT_ID for non-interactive setup)."
        );
    }

    #[test]
    fn the_chat_id_must_be_a_safe_integer() {
        for bad in ["abc", "1.5", "9007199254740993"] {
            assert_eq!(
                wizard_err(&[("TELEGRAM_BOT_TOKEN", "t"), ("TELEGRAM_CHAT_ID", bad)]),
                "Telegram chat ID must be an integer.",
                "accepted {bad}"
            );
        }
        // A large negative supergroup id is fine.
        assert!(js_safe_integer("-1001234567890").is_some());
    }

    #[test]
    fn a_missing_workdir_and_a_non_executable_binary_are_rejected() {
        assert_eq!(
            wizard_err(&[
                ("TELEGRAM_BOT_TOKEN", "t"),
                ("TELEGRAM_CHAT_ID", "1"),
                ("BRIDGE_WORKDIR", "/definitely/not/here"),
            ]),
            "Working directory does not exist: /definitely/not/here"
        );
        assert_eq!(
            wizard_err(&[
                ("TELEGRAM_BOT_TOKEN", "t"),
                ("TELEGRAM_CHAT_ID", "1"),
                ("BRIDGE_WORKDIR", "/"),
                ("CLAUDE_BIN", "/definitely/not/claude"),
            ]),
            "Claude Code executable is not executable: /definitely/not/claude"
        );
    }

    #[test]
    fn an_invalid_permission_mode_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = fake_bin(tmp.path(), "claude");
        let codex = fake_bin(tmp.path(), "codex");
        let err = wizard_err(&[
            ("TELEGRAM_BOT_TOKEN", "t"),
            ("TELEGRAM_CHAT_ID", "1"),
            ("BRIDGE_WORKDIR", "/"),
            ("CLAUDE_BIN", &claude),
            ("CODEX_BIN", &codex),
            ("BRIDGE_PERMISSION_MODE", "yolo"),
        ]);
        assert_eq!(err, "Invalid permission mode.");
    }

    /// The full env-driven coordinator config: exact key order, strict-valid,
    /// PATH assembly from the two binary dirs.
    #[test]
    fn a_fully_specified_environment_yields_the_node_config_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = fake_bin(tmp.path(), "claude");
        let codex = fake_bin(tmp.path(), "codex");
        let workdir = tmp.path().join("work");
        std::fs::create_dir(&workdir).unwrap();
        let workdir = workdir.display().to_string();
        let pairs = [
            ("TELEGRAM_BOT_TOKEN", "tok-123"),
            ("TELEGRAM_CHAT_ID", "-100123"),
            ("BRIDGE_WORKDIR", workdir.as_str()),
            ("CLAUDE_BIN", claude.as_str()),
            ("CODEX_BIN", codex.as_str()),
            ("PATH", "/usr/bin:/bin"),
        ];
        let env = env_of(&pairs);
        let mut prompter = NoPrompts;
        let mut wiz = Wizard {
            env: &env,
            non_interactive: true,
            prompter: &mut prompter,
            cwd: "/".into(),
            home: PathBuf::from("/h"),
        };
        let cfg = wiz.coordinator_config().expect("builds");

        assert!(validate_coordinator_config(&cfg).is_empty());
        let keys: Vec<&str> = cfg.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["token", "chatId", "defaultTarget", "maxMediaBytes", "elevenLabsApiKey", "targets"]
        );
        assert_eq!(cfg["token"], "tok-123");
        assert_eq!(cfg["chatId"], -100123);
        assert_eq!(cfg["maxMediaBytes"], 512 * 1024 * 1024);
        // Optional secret, absent from env, non-interactive -> ''.
        assert_eq!(cfg["elevenLabsApiKey"], "");
        let gcp_keys: Vec<&str> = cfg["targets"]["gcp"].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            gcp_keys,
            ["label", "type", "cwd", "claudeBin", "codexBin", "extraPath", "permissionMode", "model", "codexModel"]
        );
        assert_eq!(cfg["targets"]["gcp"]["label"], "Linux");
        assert_eq!(cfg["targets"]["gcp"]["permissionMode"], "default");
        assert!(cfg["targets"]["gcp"]["model"].is_null());
        assert_eq!(cfg["targets"]["mac"]["type"], "remote");
        // mergedPath(binDir(claude), binDir(codex), PATH) — bin dirs first,
        // deduped (both fakes share a dir).
        let extra = cfg["targets"]["gcp"]["extraPath"].as_str().unwrap();
        assert_eq!(
            extra,
            format!("{}:/usr/bin:/bin", tmp.path().display())
        );
    }

    /// The interactive path: scripted answers, Enter-for-default behaviour.
    #[test]
    fn the_interactive_wizard_walks_the_node_question_order() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = fake_bin(tmp.path(), "claude");
        let codex = fake_bin(tmp.path(), "codex");
        let claude_static: &'static str = Box::leak(claude.into_boxed_str());
        let codex_static: &'static str = Box::leak(codex.into_boxed_str());
        let env = env_of(&[]);
        let mut prompter = Scripted {
            // chat id, workdir (Enter = cwd default), claude, codex,
            // permission mode (Enter = default).
            answers: VecDeque::from(["42", "", claude_static, codex_static, ""]),
            // token, elevenlabs (optional, empty).
            secrets: VecDeque::from(["sekrit", ""]),
        };
        let mut wiz = Wizard {
            env: &env,
            non_interactive: false,
            prompter: &mut prompter,
            cwd: tmp.path().display().to_string(),
            home: PathBuf::from("/h"),
        };
        let cfg = wiz.coordinator_config().expect("builds");
        assert_eq!(cfg["token"], "sekrit");
        assert_eq!(cfg["chatId"], 42);
        assert_eq!(cfg["targets"]["gcp"]["cwd"], tmp.path().display().to_string());
        assert_eq!(cfg["targets"]["gcp"]["permissionMode"], "default");
        assert!(validate_coordinator_config(&cfg).is_empty());
    }

    /// The worker wizard is macOS-only, with the Node error text.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_worker_wizard_refuses_to_run_off_macos() {
        let env = env_of(&[]);
        let mut prompter = NoPrompts;
        let mut wiz = Wizard {
            env: &env,
            non_interactive: true,
            prompter: &mut prompter,
            cwd: "/".into(),
            home: PathBuf::from("/h"),
        };
        assert_eq!(
            wiz.worker_config().unwrap_err(),
            "The worker installer currently targets macOS."
        );
    }

    // ---- writes ----------------------------------------------------------

    #[test]
    fn save_config_backs_the_old_file_up_with_mode_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        save_config(&path, &json!({ "v": 1 })).unwrap();
        save_config(&path, &json!({ "v": 2 })).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "{\n  \"v\": 2\n}\n", "pretty JSON + trailing newline");
        let backups: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("config.json.backup-"))
            .collect();
        assert_eq!(backups.len(), 1, "one timestamped backup");
        let backup = backups[0].path();
        assert!(std::fs::read_to_string(&backup).unwrap().contains("\"v\": 1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&path, &backup] {
                let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{p:?}");
            }
            // The backup NAME must not contain ':' or '.' beyond the fixed
            // `.backup-` and `.json` — launchd/systemd unit paths choke on
            // colons and the reference strips them.
            let name = backup.file_name().unwrap().to_string_lossy().to_string();
            let stamp = name.strip_prefix("config.json.backup-").unwrap();
            assert!(!stamp.contains(':') && !stamp.contains('.'), "{stamp}");
        }
    }

    #[test]
    fn copy_runtime_installs_the_binary_and_the_role_shims() {
        let tmp = tempfile::tempdir().unwrap();
        copy_runtime("coordinator", tmp.path()).unwrap();
        for f in ["stackhour", "claim.mjs", "return.mjs", "tg-send.mjs"] {
            let p = tmp.path().join(f);
            assert!(p.is_file(), "missing {f}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o755, "{f} must be executable");
            }
        }
        // Idempotent: re-running the installer is the documented upgrade path.
        copy_runtime("coordinator", tmp.path()).unwrap();

        let worker = tempfile::tempdir().unwrap();
        copy_runtime("worker", worker.path()).unwrap();
        assert!(worker.path().join("stackhour").is_file());
        assert!(worker.path().join("tg-send.mjs").is_file());
        assert!(!worker.path().join("claim.mjs").exists(), "claim/return are coordinator-side");
    }

    /// Each shim must be a plausible ESM node script wrapping the right verb.
    #[test]
    fn the_shims_wrap_the_matching_hidden_verbs() {
        for verb in ["claim", "return", "tg-send"] {
            let shim = node_shim(verb);
            assert!(shim.starts_with("#!/usr/bin/env node\n"), "{verb}");
            assert!(shim.contains("spawnSync(join(here, 'stackhour')"), "{verb}");
            assert!(shim.contains(&format!("'bridge', '{verb}'")), "{verb}");
            assert!(shim.contains("process.exit(result.status ?? 1);"), "{verb}");
        }
        // claim/return pin the runtime dir; tg-send instead pins the adjacent
        // config.json (and must NOT smuggle --runtime-dir into its message).
        assert!(node_shim("claim").contains("'--runtime-dir', here"));
        assert!(
            node_shim("claim").contains("...process.argv.slice(2)"),
            "the claim shim must forward argv so `claim.mjs <target>` reaches `bridge claim <target>`"
        );
        assert!(node_shim("return").contains("'--runtime-dir', here"));
        assert!(!node_shim("tg-send").contains("--runtime-dir"));
        assert!(node_shim("tg-send").contains("CLAUDE_REMOTE_CONFIG"));
    }
}
