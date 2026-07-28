#![cfg(feature = "control")]

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

fn bin() -> PathBuf {
    let mut path = std::env::current_exe().expect("test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("stackhour")
}

struct Sandbox {
    home: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
        }
    }

    fn config_path(&self) -> PathBuf {
        self.home.path().join("config.json")
    }

    fn run(&self, args: &[&str], node_token: Option<&str>) -> Output {
        self.run_with_path(args, node_token, &std::env::var("PATH").unwrap_or_default())
    }

    fn run_with_path(&self, args: &[&str], node_token: Option<&str>, path: &str) -> Output {
        let mut command = Command::new(bin());
        command
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("STACKHOUR_CONFIG", self.config_path())
            .env("PATH", path);
        if let Some(token) = node_token {
            command.env("STACKHOUR_CONTROL_NODE_TOKEN", token);
        }
        command.output().expect("run stackhour")
    }

    fn config(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.config_path()).unwrap()).unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn installer_help_works_without_a_config() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["control", "install", "--help"], None);
    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("control install hub"));
    assert!(stdout.contains("control install node"));
    assert!(stdout.contains("control install ssh"));
    assert!(!sandbox.config_path().exists());
}

#[test]
fn hub_install_writes_private_config_stable_binary_and_service() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &[
            "control",
            "install",
            "hub",
            "--bind=127.0.0.1:4500",
            "--public-url=https://control.example.com",
            "--node-token=node-test",
            "--client-token=client-test",
            "--no-start",
        ],
        None,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let config = sandbox.config();
    assert_eq!(config["control"]["hub"]["bind"], "127.0.0.1:4500");
    assert_eq!(config["control"]["hub"]["nodeToken"], "node-test");
    assert_eq!(config["control"]["publicUrl"], "https://control.example.com");
    let binary = sandbox.home.path().join(".local/bin/stackhour");
    let service = sandbox
        .home
        .path()
        .join(".config/systemd/user/stackhour-control-hub.service");
    assert!(binary.is_file());
    assert!(service.is_file());
    let unit = std::fs::read_to_string(service).unwrap();
    assert!(unit.contains(&format!("ExecStart=\"{}\" control hub", binary.display())));
    assert!(unit.contains("Environment=\"PATH="));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(sandbox.config_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(binary).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

#[test]
fn hub_reinstall_keeps_existing_settings_and_tokens() {
    let sandbox = Sandbox::new();
    let first = sandbox.run(
        &[
            "control",
            "install",
            "hub",
            "--bind=127.0.0.1:4501",
            "--db=/tmp/control-test.db",
            "--public-url=https://control.example.com",
            "--node-token=node-test",
            "--client-token=client-test",
            "--no-start",
        ],
        None,
    );
    assert!(first.status.success(), "{}", stderr(&first));
    let second = sandbox.run(&["control", "install", "hub", "--no-start"], None);
    assert!(second.status.success(), "{}", stderr(&second));
    let config = sandbox.config();
    assert_eq!(config["control"]["hub"]["bind"], "127.0.0.1:4501");
    assert_eq!(config["control"]["hub"]["db"], "/tmp/control-test.db");
    assert_eq!(config["control"]["hub"]["nodeToken"], "node-test");
    assert_eq!(config["control"]["hub"]["clientToken"], "client-test");
    assert_eq!(config["control"]["publicUrl"], "https://control.example.com");
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(!stdout.contains("Client token:"));
    assert!(!stdout.contains("Node token:"));
}

#[test]
fn node_install_accepts_a_private_environment_token_and_merges_hub_config() {
    let sandbox = Sandbox::new();
    let hub = sandbox.run(
        &[
            "control",
            "install",
            "hub",
            "--node-token=node-test",
            "--client-token=client-test",
            "--no-start",
        ],
        None,
    );
    assert!(hub.status.success(), "{}", stderr(&hub));
    let node = sandbox.run(
        &[
            "control",
            "install",
            "node",
            "--hub-url=ws://127.0.0.1:4050/v1/node/connect",
            "--id=coordinator",
            "--workspace=/srv/work",
            "--no-start",
        ],
        Some("node-test"),
    );
    assert!(node.status.success(), "{}", stderr(&node));
    let config = sandbox.config();
    assert_eq!(config["control"]["hub"]["clientToken"], "client-test");
    assert_eq!(config["control"]["node"]["id"], "coordinator");
    assert_eq!(config["control"]["node"]["token"], "node-test");
    assert_eq!(config["control"]["node"]["workspace"], "/srv/work");
}

#[test]
fn invalid_config_and_unknown_options_write_no_install_files() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.config_path(), "{ bad json").unwrap();
    let bad_config = sandbox.run(&["control", "install", "hub", "--no-start"], None);
    assert!(!bad_config.status.success());
    assert!(stderr(&bad_config).contains("cannot parse"));
    assert!(!sandbox.home.path().join(".local/bin/stackhour").exists());

    std::fs::remove_file(sandbox.config_path()).unwrap();
    let bad_option = sandbox.run(
        &["control", "install", "hub", "--unknown=yes", "--no-start"],
        None,
    );
    assert!(!bad_option.status.success());
    assert!(stderr(&bad_option).contains("unknown option"));
    assert!(!sandbox.home.path().join(".local/bin/stackhour").exists());
}

#[test]
fn ssh_install_uses_the_remote_release_without_exposing_the_token() {
    let sandbox = Sandbox::new();
    let fake_bin = sandbox.home.path().join("fake-bin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    let fake_ssh = fake_bin.join("ssh");
    std::fs::write(
        &fake_ssh,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >\"$HOME/ssh-args\"\ncat >\"$HOME/ssh-stdin\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = sandbox.run_with_path(
        &[
            "control",
            "install",
            "ssh",
            "--host=devbox",
            "--user=nikita",
            "--port=2222",
            "--hub-url=wss://control.example.com/v1/node/connect",
            "--id=remote-devbox",
            "--workspace=/srv/work",
        ],
        Some("node-secret"),
        &path,
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let ssh_args = std::fs::read_to_string(sandbox.home.path().join("ssh-args")).unwrap();
    let ssh_stdin = std::fs::read_to_string(sandbox.home.path().join("ssh-stdin")).unwrap();
    assert!(ssh_args.contains("nikita@devbox"));
    assert!(ssh_args.contains("StrictHostKeyChecking=yes"));
    assert!(ssh_args
        .contains("https://github.com/NikitaVoitik/stackhour/releases/latest/download/install-stackhour.sh"));
    assert!(ssh_args.contains("control install node"));
    assert!(ssh_args.contains("--workspace='/srv/work'"));
    assert!(!ssh_args.contains("node-secret"));
    assert_eq!(ssh_stdin, "node-secret\n");
}
