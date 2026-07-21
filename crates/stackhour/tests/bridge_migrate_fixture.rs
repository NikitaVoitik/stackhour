//! Parity test for `stackhour bridge migrate` over the COMMITTED fixture.
//!
//! The fixture at `tests-fixtures/bridge/legacy-config.json` mirrors the shape
//! of the owner's live `~/.claude-remote/config.json` — same keys, same
//! nesting, same optionality — with every value replaced by an obvious fake.
//! The point of this file is the cutover promise: if the owner stops the Node
//! coordinator and migrates, he must lose NOTHING. So every legacy setting is
//! asserted individually rather than by golden-file compare, because a golden
//! file tells you "something moved" and this tells you "the blort target's
//! extraPath specifically survived".
//!
//! These tests spawn the built binary. They touch no network and start no
//! Telegram poller, so they are safe to run beside the live Node bridge.

use serde_json::Value;
use std::path::{Path, PathBuf};
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

/// Repo root, from this crate's manifest dir (`crates/stackhour`).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn fixture(name: &str) -> PathBuf {
    repo_root().join("tests-fixtures/bridge").join(name)
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("spawn stackhour")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn read_json(path: &PathBuf) -> Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path:?}: {e}"))
}

#[cfg(unix)]
fn mode_of(path: &PathBuf) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Migrate the fixture into a throwaway tree and hand back (config, runtime).
fn migrate_fixture(name: &str) -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("config");
    let rt = dir.path().join("run");
    let out = run(&[
        "bridge",
        "migrate",
        "--from",
        fixture(name).to_str().unwrap(),
        "--to",
        cfg.to_str().unwrap(),
        "--runtime-dir",
        rt.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (dir, cfg, rt)
}

/// `--dry-run` must be inert: it prints a plan and creates not one directory.
/// This is the flag people reach for when pointing the tool at a live config,
/// so "writes nothing" has to be mechanically true, not merely intended.
#[test]
fn dry_run_writes_absolutely_nothing() {
    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("config");
    let rt = dir.path().join("run");
    let out = run(&[
        "bridge",
        "migrate",
        "--from",
        fixture("legacy-config.json").to_str().unwrap(),
        "--to",
        cfg.to_str().unwrap(),
        "--runtime-dir",
        rt.to_str().unwrap(),
        "--dry-run",
    ]);

    assert!(out.status.success());
    assert!(!cfg.exists(), "--dry-run created the config dir");
    assert!(!rt.exists(), "--dry-run created the runtime dir");
    assert!(stdout(&out).contains("nothing written (--dry-run)"));
}

/// The secret values must never appear in a dry-run plan, because the plan is
/// what people paste into a terminal transcript or an issue.
#[test]
fn dry_run_masks_every_secret() {
    let dir = TempDir::new().unwrap();
    let out = run(&[
        "bridge",
        "migrate",
        "--from",
        fixture("legacy-config.json").to_str().unwrap(),
        "--to",
        dir.path().join("config").to_str().unwrap(),
        "--runtime-dir",
        dir.path().join("run").to_str().unwrap(),
        "--dry-run",
    ]);

    let plan = stdout(&out);
    let legacy = read_json(&fixture("legacy-config.json"));
    let token = legacy["token"].as_str().unwrap();
    let eleven = legacy["elevenLabsApiKey"].as_str().unwrap();

    assert!(!plan.contains(token), "dry-run plan leaked the bot token");
    assert!(!plan.contains(eleven), "dry-run plan leaked the ElevenLabs key");
    // Masked, but still identifiable by length so a wrong key is spottable.
    assert!(plan.contains("(51 chars)"), "token not summarised: {plan}");
}

/// ALL THREE legacy targets survive with every per-target field intact.
///
/// `blort` matters most here: it is the `/ship` destination, it is not in the
/// Telegram command table, and it is the target a "gcp|mac only" reading of
/// the config would quietly drop.
#[test]
fn all_three_targets_survive_with_every_field() {
    let (_dir, _cfg, rt) = migrate_fixture("legacy-config.json");
    let legacy = read_json(&fixture("legacy-config.json"));
    let runtime = read_json(&rt.join("config.json"));

    let legacy_targets = legacy["targets"].as_object().unwrap();
    let runtime_targets = runtime["targets"].as_object().unwrap();

    assert_eq!(
        legacy_targets.len(),
        3,
        "fixture should still carry three targets"
    );
    assert_eq!(
        runtime_targets.len(),
        legacy_targets.len(),
        "a target was dropped: {:?} -> {:?}",
        legacy_targets.keys().collect::<Vec<_>>(),
        runtime_targets.keys().collect::<Vec<_>>()
    );

    // Every field the Node coordinator reads off a target in spawnLocal().
    for (name, legacy_target) in legacy_targets {
        let got = runtime_targets
            .get(name)
            .unwrap_or_else(|| panic!("target '{name}' vanished in migration"));
        for field in [
            "label",
            "type",
            "cwd",
            "claudeBin",
            "extraPath",
            "permissionMode",
            "model",
        ] {
            let Some(want) = legacy_target.get(field) else {
                continue; // absent in the legacy target (e.g. the worker)
            };
            assert_eq!(
                got.get(field),
                Some(want),
                "target '{name}' field '{field}' changed in migration"
            );
        }
    }

    // Spot-check the one that is easiest to lose, by value not by loop.
    assert_eq!(runtime_targets["blort"]["type"], "local");
    assert_eq!(runtime_targets["blort"]["cwd"], "/home/testuser/projects/example");
    assert_eq!(
        runtime_targets["blort"]["extraPath"],
        "/home/testuser/.local/bin:/usr/local/bin:/usr/bin:/bin"
    );
    assert_eq!(runtime_targets["blort"]["permissionMode"], "bypassPermissions");
}

/// `defaultTarget` survives, including when it points at the WORKER target —
/// the maximal fixture parks it on `mac` precisely to cover that branch.
#[test]
fn default_target_survives_for_both_fixtures() {
    for (name, want) in [
        ("legacy-config.json", "gcp"),
        ("legacy-config-maximal.json", "mac"),
    ] {
        let (_dir, cfg, _rt) = migrate_fixture(name);
        let committed = read_json(&cfg.join("config.json"));
        assert_eq!(
            committed["bridge"]["defaultTarget"], want,
            "{name}: defaultTarget did not survive"
        );
    }
}

/// coordinator.mjs:33 — `CONFIG.maxMediaBytes || 512 * 1024 * 1024`.
///
/// The legacy default is INVISIBLE (it lives in JS, not in the file), so the
/// migrator writes it out explicitly. Both branches are covered: absent in the
/// base fixture, present-and-equal in the maximal one.
#[test]
fn media_cap_survives_including_the_invisible_default() {
    const LEGACY_DEFAULT: i64 = 512 * 1024 * 1024;

    let (_d1, _c1, rt) = migrate_fixture("legacy-config.json");
    let base = read_json(&rt.join("config.json"));
    assert!(
        read_json(&fixture("legacy-config.json"))
            .get("maxMediaBytes")
            .is_none(),
        "base fixture is supposed to OMIT maxMediaBytes"
    );
    assert_eq!(
        base["maxMediaBytes"], LEGACY_DEFAULT,
        "the implicit 512 MiB cap was not materialised"
    );

    let (_d2, _c2, rt_max) = migrate_fixture("legacy-config-maximal.json");
    let legacy_max = read_json(&fixture("legacy-config-maximal.json"));
    let max = read_json(&rt_max.join("config.json"));
    assert_eq!(
        max["maxMediaBytes"], legacy_max["maxMediaBytes"],
        "an explicit maxMediaBytes was not carried over"
    );
}

/// The ElevenLabs key drives voice transcription (coordinator.mjs:137). A
/// populated key must land in the runtime config — the file
/// `load_coordinator_cfg` reads it from; an EMPTY one must be omitted rather
/// than written as `""`, so the "is transcription configured?" check keeps
/// reading false.
#[test]
fn elevenlabs_key_survives_and_an_empty_one_is_omitted() {
    let (_d1, _cfg, rt) = migrate_fixture("legacy-config.json");
    let runtime = read_json(&rt.join("config.json"));
    let legacy = read_json(&fixture("legacy-config.json"));
    assert_eq!(
        runtime["elevenLabsApiKey"], legacy["elevenLabsApiKey"],
        "the ElevenLabs key did not survive"
    );

    let (_d2, _cfg_max, rt_max) = migrate_fixture("legacy-config-maximal.json");
    let runtime_max = read_json(&rt_max.join("config.json"));
    assert!(
        runtime_max.get("elevenLabsApiKey").is_none(),
        "an empty ElevenLabs key should be omitted, not written as an empty string"
    );
}

/// Token and chat id reach the runtime config under the SAME keys the Node
/// coordinator used, and the chat id keeps its NUMERIC type — Telegram's API
/// is forgiving about a stringified id but the local "is this message from
/// the owner?" comparison is not.
#[test]
fn token_and_chat_id_survive_with_their_types() {
    let (_dir, _cfg, rt) = migrate_fixture("legacy-config.json");
    let legacy = read_json(&fixture("legacy-config.json"));
    let runtime = read_json(&rt.join("config.json"));

    assert_eq!(runtime["token"], legacy["token"]);
    assert_eq!(runtime["chatId"], legacy["chatId"]);
    assert!(
        runtime["chatId"].is_number(),
        "chatId must stay a number"
    );
}

/// The safety property that lets the config dir be committed: secrets live in
/// exactly one 0600 file — the runtime config the coordinator reads — and
/// nothing else in either tree contains them.
#[test]
fn secrets_are_isolated_in_one_mode_0600_file() {
    let (_dir, cfg, rt) = migrate_fixture("legacy-config.json");
    let legacy = read_json(&fixture("legacy-config.json"));
    let token = legacy["token"].as_str().unwrap();
    let eleven = legacy["elevenLabsApiKey"].as_str().unwrap();

    let runtime_cfg = rt.join("config.json");
    assert!(runtime_cfg.exists(), "runtime config.json was not written");

    #[cfg(unix)]
    {
        assert_eq!(
            mode_of(&runtime_cfg),
            0o600,
            "the runtime config holds the bot token and must not be group- or world-readable"
        );
    }

    // No secret anywhere else in the migrated tree, committed or runtime.
    for path in walk(&cfg).into_iter().chain(walk(&rt)) {
        if path == runtime_cfg {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        assert!(!body.contains(token), "{path:?} leaked the bot token");
        assert!(!body.contains(eleven), "{path:?} leaked the ElevenLabs key");
    }
}

/// The committed-safe config carries settings and only settings.
#[test]
fn committed_config_holds_no_credential_keys() {
    let (_dir, cfg, _rt) = migrate_fixture("legacy-config.json");
    let raw = std::fs::read_to_string(cfg.join("config.json")).unwrap();
    for needle in ["token", "Token", "chatId", "apiKey", "ApiKey"] {
        assert!(
            !raw.contains(needle),
            "committed config.json mentions '{needle}': {raw}"
        );
    }
}

/// A second run over a populated tree must refuse rather than clobber, and
/// must exit 2 so a script can tell "already migrated" from "broken".
#[test]
fn a_second_migration_refuses_with_exit_2() {
    let (_dir, cfg, rt) = migrate_fixture("legacy-config.json");
    let before = std::fs::read_to_string(rt.join("config.json")).unwrap();

    let out = run(&[
        "bridge",
        "migrate",
        "--from",
        fixture("legacy-config.json").to_str().unwrap(),
        "--to",
        cfg.to_str().unwrap(),
        "--runtime-dir",
        rt.to_str().unwrap(),
    ]);

    assert_eq!(out.status.code(), Some(2), "expected the conflict exit code");
    assert_eq!(
        std::fs::read_to_string(rt.join("config.json")).unwrap(),
        before,
        "a refused migration still modified the runtime config.json"
    );
}

/// `--verify` is clean immediately after a real migration.
#[test]
fn verify_is_clean_right_after_migrating() {
    let (_dir, cfg, rt) = migrate_fixture("legacy-config.json");
    let out = run(&[
        "bridge",
        "migrate",
        "--from",
        fixture("legacy-config.json").to_str().unwrap(),
        "--to",
        cfg.to_str().unwrap(),
        "--runtime-dir",
        rt.to_str().unwrap(),
        "--verify",
    ]);

    assert!(
        out.status.success(),
        "verify reported drift on a fresh migration: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains("every legacy key reached a destination"));
}

/// `/ship` keeps its destination through the migration.
///
/// coordinator.mjs:363 hardcodes `state.active = 'blort'` for `/ship`, while
/// the Rust `CoordCtx::ship_cfg` seeds the ship target from `default_target`
/// unless a `ship.target` runtime key overrides it. This used to be a KNOWN
/// PARITY GAP (the migration never wrote that key, so `/ship` parked on gcp
/// after cutover); the migration now materialises the live destination. The
/// blort target must still survive in `targets`, and the migrator must still
/// disclose that blort is reachable only through `/ship`.
#[test]
fn the_ship_destination_is_carried_into_the_runtime_config() {
    let dir = TempDir::new().unwrap();
    let cfg = dir.path().join("config");
    let rt = dir.path().join("run");
    let out = run(&[
        "bridge",
        "migrate",
        "--from",
        fixture("legacy-config.json").to_str().unwrap(),
        "--to",
        cfg.to_str().unwrap(),
        "--runtime-dir",
        rt.to_str().unwrap(),
    ]);
    assert!(out.status.success());

    let runtime = read_json(&rt.join("config.json"));
    assert!(
        runtime.get("targets").and_then(|t| t.get("blort")).is_some(),
        "the blort target itself should still survive in the config"
    );
    assert_eq!(runtime["ship"]["target"], "blort");
    assert_eq!(runtime["ship"]["engine"], "claude");

    // blort's second-class status has to stay disclosed on the way out.
    assert!(
        stdout(&out).contains("only /ship reaches it"),
        "the migration stopped disclosing that blort is /ship-only"
    );
}

/// Every recursive file under `root`.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}
