//! Soul documents: COMPOSITION ORDER and HOT RELOAD, from config alone.
//!
//! Two promises are pinned here, both of which are user-facing and neither of
//! which any other test asserts exactly:
//!
//!   1. Composition order. `composed_soul` documents the order as: the
//!      `extends` chain ROOT-FIRST, each agent contributing its own soul
//!      followed by its overlays in declaration order, joined by a blank line,
//!      with empty documents contributing nothing. `layering.rs` only checks
//!      that the pieces are all present somewhere; this file checks the exact
//!      string, so a reordering (base overriding the child, say) fails loudly.
//!
//!   2. Hot reload. Editing a `.md` on disk must change the NEXT composition
//!      with no registry reload and no daemon restart. Every assertion here
//!      reuses the same `Registry` value that was loaded before the edit.
//!
//! Nothing in this file touches Rust source: the whole agent tree is markdown
//! and TOML written into a tempdir, which is exactly what a user writes.

use indexmap::IndexMap;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use stackhour_bridge::engines::RunRequest;
use stackhour_bridge::souls;
use stackhour_bridge::state::BridgeState;
use stackhour_core::registry::{self, Registry};

/// Write a config tree and load it through the daemon's own entry point.
fn config(files: &[(&str, &str)]) -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (rel, body) in files {
        write(dir.path(), rel, body);
    }
    let reg = registry::load_with(dir.path(), registry::EnvSource::fixed(&[]));
    (dir, reg)
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
    fs::write(&path, body).expect("write");
}

/// Rewrite a file and force a strictly newer mtime, so the test cannot pass or
/// fail by accident on a coarse-timestamp filesystem.
fn edit(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
    fs::write(&path, body).expect("write");
    let f = fs::File::options().write(true).open(&path).expect("open");
    f.set_modified(SystemTime::now() + Duration::from_secs(5))
        .expect("set_modified");
}

/// A three-deep chain, each level carrying a soul and (except the leaf's
/// parent) overlays, plus one deliberately EMPTY document that must not leave
/// a blank paragraph behind.
fn agent_tree() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "engines/ollama.toml",
            r#"
label = "Ollama"
bin = "ollama"
kind = "plain-lines"
args = ["run"]
system_prompt_args = ["--system", "{{system_prompt}}"]
"#,
        ),
        // root of the chain
        (
            "agents/base/agent.toml",
            r#"
label = "Base"
engine = "ollama"
overlays = ["tone.md", "empty.md"]
"#,
        ),
        ("agents/base/soul.md", "BASE SOUL\n"),
        ("agents/base/tone.md", "BASE TONE\n"),
        ("agents/base/empty.md", "   \n\n"),
        // middle
        (
            "agents/mid/agent.toml",
            r#"
extends = "base"
soul = "soul.md"
"#,
        ),
        ("agents/mid/soul.md", "MID SOUL\n"),
        // leaf
        (
            "agents/leaf/agent.toml",
            r#"
extends = "mid"
overlays = ["a.md", "b.md"]
"#,
        ),
        ("agents/leaf/soul.md", "LEAF SOUL\n"),
        ("agents/leaf/a.md", "LEAF OVERLAY A\n"),
        ("agents/leaf/b.md", "LEAF OVERLAY B\n"),
    ]
}

fn state() -> BridgeState {
    BridgeState {
        offset: 0,
        active: "gcp".into(),
        engine: "ollama".into(),
        agent: None,
        sessions: IndexMap::new(),
        raw: json!({}),
    }
}

fn system_prompt(reg: &Registry, agent: &str) -> String {
    let def = reg.agents.get(agent).expect("agent loaded");
    souls::compose_system_prompt(def, reg).expect("compose")
}

/// The documented order, asserted as an exact string.
#[test]
fn a_base_soul_and_its_overlays_compose_root_first_in_declaration_order() {
    let (_d, reg) = config(&agent_tree());
    assert!(reg.errors.is_empty(), "registry errors: {:?}", reg.errors);

    assert_eq!(
        system_prompt(&reg, "leaf"),
        [
            "BASE SOUL",
            "BASE TONE",
            // base/empty.md is whitespace-only and contributes nothing.
            "MID SOUL",
            "LEAF SOUL",
            "LEAF OVERLAY A",
            "LEAF OVERLAY B",
        ]
        .join("\n\n"),
        "soul composition order drifted from the documented root-first, \
         soul-then-overlays order"
    );

    // The middle of the chain composes on its own terms: it must not see the
    // leaf's documents.
    assert_eq!(
        system_prompt(&reg, "mid"),
        ["BASE SOUL", "BASE TONE", "MID SOUL"].join("\n\n"),
    );
}

/// Editing any document in the chain changes the very next composition, with
/// the SAME already-loaded registry.
#[test]
fn editing_a_soul_on_disk_changes_the_next_invocation_without_a_reload() {
    let (dir, reg) = config(&agent_tree());
    assert!(reg.errors.is_empty(), "registry errors: {:?}", reg.errors);

    let before = system_prompt(&reg, "leaf");
    assert!(before.contains("LEAF SOUL"), "{before}");

    // 1. Edit the leaf's own soul.
    edit(dir.path(), "agents/leaf/soul.md", "LEAF SOUL v2\n");
    let after = system_prompt(&reg, "leaf");
    assert!(
        after.contains("LEAF SOUL v2") && !after.contains("LEAF SOUL\n\n"),
        "the edited soul was not picked up without a reload:\n{after}"
    );

    // 2. Edit an INHERITED document two levels up.
    edit(dir.path(), "agents/base/soul.md", "BASE SOUL v2\n");
    let after = system_prompt(&reg, "leaf");
    assert!(
        after.starts_with("BASE SOUL v2"),
        "an edit to an inherited soul did not reach the child:\n{after}"
    );

    // 3. Edit an overlay.
    edit(dir.path(), "agents/leaf/b.md", "LEAF OVERLAY B v2\n");
    let after = system_prompt(&reg, "leaf");
    assert_eq!(
        after,
        [
            "BASE SOUL v2",
            "BASE TONE",
            "MID SOUL",
            "LEAF SOUL v2",
            "LEAF OVERLAY A",
            "LEAF OVERLAY B v2",
        ]
        .join("\n\n"),
        "hot reload changed the content but disturbed the order"
    );

    // 4. A document that did not exist at load time appears once written:
    //    `empty.md` was whitespace-only, so it contributed nothing until now.
    edit(dir.path(), "agents/base/empty.md", "BASE LATE ADDITION\n");
    let after = system_prompt(&reg, "leaf");
    assert_eq!(
        after,
        [
            "BASE SOUL v2",
            "BASE TONE",
            "BASE LATE ADDITION",
            "MID SOUL",
            "LEAF SOUL v2",
            "LEAF OVERLAY A",
            "LEAF OVERLAY B v2",
        ]
        .join("\n\n"),
        "a newly-filled overlay did not appear in its declared slot"
    );

    // 5. Deleting a document empties it rather than failing the turn.
    fs::remove_file(dir.path().join("agents/leaf/a.md")).expect("rm");
    let after = system_prompt(&reg, "leaf");
    assert!(
        !after.contains("LEAF OVERLAY A"),
        "a deleted overlay was still served from cache:\n{after}"
    );
    assert!(after.contains("LEAF SOUL v2"), "{after}");
}

/// The hot-reloaded text has to reach the run request, not just the composer.
#[test]
fn a_hot_edited_soul_reaches_the_run_request() {
    let (dir, reg) = config(&agent_tree());
    let engine = reg.engines.get("ollama").expect("engine").clone();

    let mut st = state();
    st.agent = Some("leaf".into());
    let agent = souls::agent_for(&st, &reg).expect("the configured agent resolves");
    assert_eq!(agent.name, "leaf");

    let run = |agent: &_| {
        let mut req = RunRequest {
            prompt: "hello".into(),
            ..RunRequest::default()
        };
        souls::apply_agent(&engine, Some(agent), &reg, &mut req);
        req.system_prompt.clone().unwrap_or_default()
    };

    assert!(run(agent).contains("LEAF SOUL"));

    edit(dir.path(), "agents/leaf/soul.md", "LEAF SOUL rewritten\n");
    let sp = run(agent);
    assert!(
        sp.contains("LEAF SOUL rewritten"),
        "the daemon path did not see the edit:\n{sp}"
    );
    assert!(
        sp.starts_with("BASE SOUL"),
        "order lost on the daemon path:\n{sp}"
    );
}

/// A snapshot taken for an in-flight job keeps reading its own files: cloning
/// an agent must not freeze the text, and must not be frozen BY the clone.
#[test]
fn a_cloned_agent_snapshot_still_hot_reloads() {
    let (dir, reg) = config(&agent_tree());
    let snapshot = reg.agents.get("leaf").expect("agent").clone();

    assert!(souls::compose_system_prompt(&snapshot, &reg)
        .expect("compose")
        .contains("LEAF SOUL"));

    edit(dir.path(), "agents/leaf/soul.md", "LEAF SOUL for the clone\n");
    assert!(
        souls::compose_system_prompt(&snapshot, &reg)
            .expect("compose")
            .contains("LEAF SOUL for the clone"),
        "a cloned agent stopped tracking its soul file"
    );
}
