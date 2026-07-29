use semver::Version;
use serde::Serialize;
use sha2::{Digest, Sha256};
use stackhour_core::{Error, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const RELEASE_ROOT: &str = "https://github.com/NikitaVoitik/stackhour/releases/latest/download";
const MAX_RELEASE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
struct UpdateInfo {
    current_version: String,
    latest_version: String,
    update_available: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Hub,
    Node,
    All,
}

pub fn run(args: &[String]) -> Result<()> {
    let check = args.iter().any(|arg| arg == "--check");
    let json = args.iter().any(|arg| arg == "--json");
    let force = args.iter().any(|arg| arg == "--force");
    let role = option(args, "role")
        .as_deref()
        .map(parse_role)
        .transpose()?
        .unwrap_or(Role::All);
    let target_version = option(args, "target-version");
    reject_unknown(args)?;

    let info = discover_release()?;
    if let Some(target) = target_version {
        if target != info.latest_version {
            return Err(Error::msg(format!(
                "requested update {target} is not the latest official release {}",
                info.latest_version
            )));
        }
    }
    if check {
        if json {
            println!("{}", serde_json::to_string(&info)?);
        } else if info.update_available {
            println!(
                "Stackhour {} is available (installed {}).",
                info.latest_version, info.current_version
            );
        } else {
            println!("Stackhour {} is current.", info.current_version);
        }
        return Ok(());
    }
    if !force && !info.update_available {
        println!("Stackhour {} is already current.", info.current_version);
        return Ok(());
    }

    install_release(&info.latest_version)?;
    restart(role)?;
    println!("Updated Stackhour to {}.", info.latest_version);
    Ok(())
}

fn option(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("--{name}=");
    args.iter()
        .find_map(|arg| arg.strip_prefix(&prefix).map(str::to_string))
}

fn reject_unknown(args: &[String]) -> Result<()> {
    for arg in args {
        if matches!(arg.as_str(), "--check" | "--json" | "--force")
            || arg.starts_with("--role=")
            || arg.starts_with("--target-version=")
        {
            continue;
        }
        return Err(Error::msg(format!("unknown update option: {arg}")));
    }
    Ok(())
}

fn parse_role(value: &str) -> Result<Role> {
    match value {
        "hub" => Ok(Role::Hub),
        "node" => Ok(Role::Node),
        "all" => Ok(Role::All),
        _ => Err(Error::msg("update role must be hub, node, or all")),
    }
}

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("stackhour/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|error| Error::msg(format!("cannot create update client: {error}")))
}

fn download_text(client: &reqwest::blocking::Client, name: &str) -> Result<String> {
    let response = client
        .get(format!("{RELEASE_ROOT}/{name}"))
        .send()
        .map_err(|error| Error::msg(format!("cannot download {name}: {error}")))?
        .error_for_status()
        .map_err(|error| Error::msg(format!("cannot download {name}: {error}")))?;
    response
        .text()
        .map_err(|error| Error::msg(format!("cannot read {name}: {error}")))
}

fn discover_release() -> Result<UpdateInfo> {
    let latest_text = download_text(&client()?, "VERSION")?;
    let latest = Version::parse(latest_text.trim())
        .map_err(|error| Error::msg(format!("official release VERSION is invalid: {error}")))?;
    let current = Version::parse(stackhour_core::VERSION)
        .map_err(|error| Error::msg(format!("installed version is invalid: {error}")))?;
    Ok(UpdateInfo {
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        update_available: latest > current,
    })
}

fn release_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Err(Error::msg("Stackhour does not support Intel macOS")),
        (os, arch) => Err(Error::msg(format!(
            "Stackhour updates do not support {os}/{arch}"
        ))),
    }
}

fn expected_checksum(sums: &str, asset: &str) -> Result<String> {
    let hashes: Vec<&str> = sums
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let hash = fields.next()?;
            let name = fields.next()?;
            (name == asset && fields.next().is_none()).then_some(hash)
        })
        .collect();
    if hashes.len() != 1 || hashes[0].len() != 64 || !hashes[0].bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::msg(format!(
            "official release checksums do not contain exactly one valid entry for {asset}"
        )));
    }
    Ok(hashes[0].to_ascii_lowercase())
}

fn download_file(client: &reqwest::blocking::Client, name: &str, path: &Path) -> Result<()> {
    let response = client
        .get(format!("{RELEASE_ROOT}/{name}"))
        .send()
        .map_err(|error| Error::msg(format!("cannot download {name}: {error}")))?
        .error_for_status()
        .map_err(|error| Error::msg(format!("cannot download {name}: {error}")))?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RELEASE_BYTES)
    {
        return Err(Error::msg(format!("{name} exceeds the update size limit")));
    }
    let mut file =
        std::fs::File::create(path).map_err(|error| Error::msg(format!("{}: {error}", path.display())))?;
    let copied = std::io::copy(&mut response.take(MAX_RELEASE_BYTES + 1), &mut file)
        .map_err(|error| Error::msg(format!("cannot save {name}: {error}")))?;
    if copied > MAX_RELEASE_BYTES {
        return Err(Error::msg(format!("{name} exceeds the update size limit")));
    }
    Ok(())
}

fn sha256(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).map_err(|error| Error::msg(format!("{}: {error}", path.display())))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| Error::msg(format!("{}: {error}", path.display())))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn install_release(version: &str) -> Result<()> {
    Version::parse(version).map_err(|error| Error::msg(format!("invalid update version: {error}")))?;
    let target = release_target()?;
    let asset = format!("stackhour-{target}.tar.gz");
    let client = client()?;
    let sums = download_text(&client, "SHA256SUMS")?;
    let expected = expected_checksum(&sums, &asset)?;
    let temp = tempfile::Builder::new()
        .prefix("stackhour-update.")
        .tempdir()
        .map_err(|error| Error::msg(format!("cannot create update directory: {error}")))?;
    let archive = temp.path().join(&asset);
    download_file(&client, &asset, &archive)?;
    let actual = sha256(&archive)?;
    if actual != expected {
        return Err(Error::msg("the Stackhour release checksum is invalid"));
    }
    let status = Command::new("tar")
        .args(["-xzf"])
        .arg(&archive)
        .arg("-C")
        .arg(temp.path())
        .status()
        .map_err(|error| Error::msg(format!("cannot extract update: {error}")))?;
    if !status.success() {
        return Err(Error::msg(format!("cannot extract update: tar exited {status}")));
    }
    let source = temp.path().join(format!("stackhour-{target}/stackhour"));
    let bytes = std::fs::read(&source)
        .map_err(|error| Error::msg(format!("cannot read {}: {error}", source.display())))?;
    if bytes.is_empty() {
        return Err(Error::msg("the release binary is empty"));
    }
    let target_path = installed_binary()?;
    stackhour_core::fsutil::atomic_write(&target_path, &bytes, 0o755)
        .map_err(|error| Error::msg(format!("cannot install {}: {error}", target_path.display())))
}

fn installed_binary() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| Error::msg("HOME is required for updates"))?;
    Ok(PathBuf::from(home).join(".local/bin/stackhour"))
}

fn restart(role: Role) -> Result<()> {
    if cfg!(target_os = "linux") {
        if matches!(role, Role::Node | Role::All) {
            let result = try_restart(
                "systemctl",
                &["--user", "try-restart", "stackhour-control-node.service"],
            );
            if role == Role::Node {
                result?;
            }
        }
        if matches!(role, Role::Hub | Role::All) {
            try_restart(
                "systemctl",
                &["--user", "try-restart", "stackhour-control-hub.service"],
            )?;
        }
    } else if cfg!(target_os = "macos") {
        let uid = rustix::process::getuid().as_raw();
        if matches!(role, Role::Node | Role::All) {
            let result = try_restart(
                "launchctl",
                &[
                    "kickstart",
                    "-k",
                    &format!("gui/{uid}/com.stackhour.control-node"),
                ],
            );
            if role == Role::Node {
                result?;
            }
        }
        if matches!(role, Role::Hub | Role::All) {
            try_restart(
                "launchctl",
                &["kickstart", "-k", &format!("gui/{uid}/com.stackhour.control-hub")],
            )?;
        }
    }
    Ok(())
}

fn try_restart(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| Error::msg(format!("cannot restart Stackhour: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::msg(format!(
            "cannot restart Stackhour: {program} exited {status}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_manifest_requires_one_exact_asset() {
        let hash = "a".repeat(64);
        assert_eq!(
            expected_checksum(&format!("{hash}  stackhour-x.tar.gz\n"), "stackhour-x.tar.gz").unwrap(),
            hash
        );
        assert!(expected_checksum("bad stackhour-x.tar.gz\n", "stackhour-x.tar.gz").is_err());
        assert!(expected_checksum(
            &format!("{hash} stackhour-x.tar.gz\n{hash} stackhour-x.tar.gz\n"),
            "stackhour-x.tar.gz"
        )
        .is_err());
    }

    #[test]
    fn update_arguments_do_not_accept_urls_or_unknown_flags() {
        assert!(reject_unknown(&["--release-root=https://evil.example".to_string()]).is_err());
        assert_eq!(parse_role("node").unwrap(), Role::Node);
        assert!(parse_role("remote command").is_err());
    }
}
