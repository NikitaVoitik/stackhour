//! The generalized topology, end to end through the REAL `Runtime`:
//!
//! * a LEADER-ONLY config — zero `type == "local"` targets, arbitrary
//!   names — passes the loose gate, boots a runtime with one worker lane
//!   per target, and routes every prompt to a worker lane;
//! * two worker lanes stamp their own `target` into the jobs they dispatch,
//!   and a targeted `try_claim` surfaces only matching jobs;
//! * a target with NO lane (the `/ship` destination absent from config, a
//!   stale state.json) falls back to the DEFAULT target's lane — worker or
//!   local.
//!
//! SAFETY: the transport points at the in-process mock in
//! `common/mock_bot_api.rs`; the queue directories are tempdirs. Nothing
//! here may ever touch `api.telegram.org` or the owner's live queue.

#[path = "common/mock_bot_api.rs"]
mod mock;

use mock::{MockApi, Reply};
use serde_json::{json, Value};
use stackhour_bridge::coordinator::{target_specs, Runtime};
use stackhour_bridge::registry_ctx::RegistryCtx;
use stackhour_bridge::telegram::{Tg, TgConfig};
use stackhour_bridge::{jobs, BridgePaths};
use std::fs;

const CHAT: i64 = 4242;
const TOKEN: &str = "test-token-not-a-real-bot";

struct Fixture {
    _config: tempfile::TempDir,
    runtime: tempfile::TempDir,
    api: MockApi,
    rt: Runtime,
    paths: BridgePaths,
}

/// Boot a real runtime around `config_json` + `state_json`, loading the
/// registry with the roster derived from the config — exactly what
/// `run_coordinator` does.
fn fixture(config_json: Value, state_json: Value) -> Fixture {
    let config = tempfile::tempdir().expect("config tempdir");
    let runtime = tempfile::tempdir().expect("runtime tempdir");
    fs::write(runtime.path().join("config.json"), config_json.to_string()).unwrap();
    fs::write(runtime.path().join("state.json"), state_json.to_string()).unwrap();

    let api = MockApi::start();
    let paths = BridgePaths::from_runtime_dir(runtime.path());
    paths.ensure_dirs().expect("ensure dirs");
    let cfg = stackhour_bridge::config::load_coordinator_cfg(&paths.config_path).expect("config loads");

    let reg = stackhour_core::registry::load_with_targets(
        config.path(),
        stackhour_core::registry::EnvSource::fixed(&[]),
        &target_specs(&cfg),
        &cfg.default_target,
    );
    assert!(reg.errors.is_empty(), "fixture registry errors: {:?}", reg.errors);

    let tg = Tg::with_config(TgConfig::new(TOKEN, CHAT).with_api_root(&api.base));
    let rt = Runtime::new(cfg, paths.clone(), tg, RegistryCtx::from_registry(reg));
    Fixture {
        _config: config,
        runtime,
        api,
        rt,
        paths,
    }
}

fn text_update(update_id: i64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": 77,
            "chat": { "id": CHAT },
            "from": { "id": 1, "is_bot": false },
            "text": text,
        }
    })
}

/// Every job file currently queued, as (target, prompt) pairs.
fn queued_jobs(paths: &BridgePaths) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(&paths.jobs_dir).unwrap().flatten() {
        let body = fs::read_to_string(entry.path()).unwrap();
        let job: Value = serde_json::from_str(&body).unwrap();
        out.push((
            job["target"].as_str().unwrap_or_default().to_string(),
            job["prompt"].as_str().unwrap_or_default().to_string(),
        ));
    }
    out.sort();
    out
}

fn leader_only_config() -> Value {
    json!({
        "token": TOKEN,
        "chatId": CHAT,
        "defaultTarget": "pi",
        "targets": {
            "pi": { "label": "🥧 Pi" },
            "attic": { "label": "🏠 Attic", "type": "remote" },
        },
    })
}

/// (a) A leader-only config — zero local targets, arbitrary names — passes
/// both gates and boots a runtime whose `route_prompt` sends everything to
/// worker lanes.
#[test]
fn a_leader_only_config_boots_and_routes_everything_to_worker_lanes() {
    // Both gates first.
    assert!(
        stackhour_bridge::config::validate_coordinator_config(&leader_only_config()).is_empty(),
        "the strict gate must accept a leader-only config"
    );
    let f = fixture(
        leader_only_config(),
        json!({ "offset": 0, "active": "pi", "engine": "claude" }),
    );
    assert_eq!(
        f.rt.workers.keys().collect::<Vec<_>>(),
        vec!["pi", "attic"],
        "one lane per non-local target, in config order"
    );

    // The roster reached the registry: generated switch commands, no /gcp.
    let reg = f.rt.ctx.registry();
    assert!(reg.commands.contains_key("pi") && reg.commands.contains_key("attic"));
    assert!(!reg.commands.contains_key("gcp") && !reg.commands.contains_key("mac"));

    f.api.set_default(Reply::ok(json!({ "message_id": 1 })));
    f.rt.handle_update(&text_update(1, "hello pi"));
    assert_eq!(
        queued_jobs(&f.paths),
        vec![("pi".to_string(), "hello pi".to_string())]
    );
    assert!(!f.rt.local.is_busy(), "nothing may reach the (idle) local lane");
    assert_eq!(f.rt.local.queued(), 0);
}

/// (b) Two worker lanes dispatch jobs stamped with their own target, the
/// generated switch command moves between them, and a targeted `try_claim`
/// only surfaces matching jobs.
#[test]
fn two_worker_lanes_stamp_their_targets_and_targeted_claims_filter() {
    let f = fixture(
        leader_only_config(),
        json!({ "offset": 0, "active": "pi", "engine": "claude" }),
    );
    f.api.set_default(Reply::ok(json!({ "message_id": 1 })));

    f.rt.handle_update(&text_update(1, "for pi"));
    // The roster-generated /attic switch command, then a prompt for it.
    f.rt.handle_update(&text_update(2, "/attic"));
    f.rt.handle_update(&text_update(3, "for attic"));

    assert_eq!(
        queued_jobs(&f.paths),
        vec![
            ("attic".to_string(), "for attic".to_string()),
            ("pi".to_string(), "for pi".to_string()),
        ]
    );

    // A targeted claim surfaces ONLY its own target's job.
    let claimed = jobs::try_claim_target(&f.paths.jobs_dir, &f.paths.inprogress_dir, Some("attic"))
        .expect("attic's job");
    let job: Value = serde_json::from_str(&claimed).unwrap();
    assert_eq!(job["target"], "attic");
    assert_eq!(job["prompt"], "for attic");
    assert_eq!(
        jobs::try_claim_target(&f.paths.jobs_dir, &f.paths.inprogress_dir, Some("attic")),
        None,
        "attic has nothing else; pi's job must stay invisible to it"
    );
    assert_eq!(
        queued_jobs(&f.paths),
        vec![("pi".to_string(), "for pi".to_string())]
    );
}

/// (e) A target with no lane and no local declaration — the classic `ship`
/// destination absent from config — falls back to the default target's
/// WORKER lane in a leader-only deployment.
#[test]
fn an_unknown_target_falls_back_to_the_default_targets_worker_lane() {
    let f = fixture(
        leader_only_config(),
        json!({ "offset": 0, "active": "blort", "engine": "claude" }),
    );
    f.api.set_default(Reply::ok(json!({ "message_id": 1 })));
    f.rt.handle_update(&text_update(1, "ship it"));
    assert_eq!(
        queued_jobs(&f.paths),
        vec![("pi".to_string(), "ship it".to_string())],
        "the default target's lane owns the stray prompt"
    );
}

/// (e) …and when the default target is LOCAL, the same stray prompt goes
/// through the local lane (whose own fallback resolves it), writing no job
/// file at all.
#[test]
fn an_unknown_target_with_a_local_default_goes_through_the_local_lane() {
    let f = fixture(
        json!({
            "token": TOKEN,
            "chatId": CHAT,
            "defaultTarget": "gcp",
            "targets": {
                "gcp": { "label": "☁️ GCP", "type": "local",
                         "cwd": "/tmp", "claudeBin": "/bin/true", "codexBin": "/bin/true" },
                "mac": { "label": "🖥️ Mac", "type": "remote" },
            },
        }),
        json!({ "offset": 0, "active": "blort", "engine": "claude" }),
    );
    f.api.set_default(Reply::ok(json!({ "message_id": 1 })));
    f.rt.handle_update(&text_update(1, "stray"));
    // Dispatching to a worker lane is synchronous inside handle_update, so an
    // empty jobs/ directory proves the prompt went to the local lane instead.
    assert_eq!(queued_jobs(&f.paths), Vec::<(String, String)>::new());
    // Let the local lane drain before the tempdirs are torn down.
    for _ in 0..600 {
        if !f.rt.local.is_busy() && f.rt.local.queued() == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    drop(f.runtime);
}
