//! `stackhour bridge migrate` — legacy Node coordinator config -> config dir.
//!
//! One direction only: it READS `~/.claude-remote/config.json` (the Node
//! coordinator's single settings file) and WRITES the Rust bridge's two
//! homes. It never writes back to the legacy file, and it never reads the
//! new layout to produce a legacy one.
//!
//! ```text
//! <config-dir>/                     registry root (StoragePaths::config_dir)
//!   config.json          0644   MERGED: adds only the "bridge" object
//!   engines/claude.toml  0644   frozen copy of the built-in engine
//!   engines/codex.toml   0644
//!   agents/orwell/…      0644   the ORWELL_RULES system prompt
//!   prompts/help.md      0644   the legacy HELP text, verbatim
//!   prompts/ship.md      0644   the /ship prompt body
//!   commands/*.toml      0644   one per migratable legacy verb
//! <runtime-dir>/
//!   config.json          0600   token/chatId/elevenLabsApiKey + ship{} +
//!                               targets{} + maxMediaBytes — everything
//!                               `load_coordinator_cfg` actually reads, so a
//!                               freshly migrated runtime dir boots.
//! ```
//!
//! Secrets land in exactly one file — the runtime `config.json`, mode 0600,
//! the file the coordinator reads them from — and are masked in every line
//! this module prints. (An earlier revision wrote a separate `secrets.json`;
//! nothing ever read it, so it is gone.) `--dry-run` performs zero
//! filesystem writes — no mkdir, no temp file — so reading the plan is
//! always safe.
//!
//! ## Deviations from the migration brief, and why
//!
//! * **`prompts/system-claude.md` became `agents/orwell/`.** Nothing in the
//!   registry reads a prompt file into an engine's `system_prompt_args`: that
//!   template only carries `{{system_prompt}}`, and the text comes from the
//!   ACTIVE AGENT's soul (see `souls::compose_system_prompt`). A prompt file
//!   would have been dead weight; an agent plus `bridge.defaultAgent` is what
//!   actually reproduces `--append-system-prompt ORWELL_RULES`.
//! * **`<config-dir>/config.json` is merged, not created.** It is the
//!   tracker's own settings file (`stackhour_core::config`), and on a real box
//!   it already holds `server` and `agent` sections. Creating it would delete
//!   them. Only the `bridge` object is touched.
//! * **Five command files, not ten.** `help`, `menu`, `stop` are `RESERVED`
//!   and `where`, `new` are `kind = "builtin"`, which `CommandDef::parse`
//!   refuses in a user file. All five already ship with byte-identical
//!   descriptions, so nothing is lost — but they cannot be frozen to disk.
//! * **`button_order` copies the built-in table's values** (10/11/20/21/…)
//!   rather than the brief's 10/20/30/…: both lay out the same 3x2 grid, and
//!   matching the built-ins keeps a frozen file diffable against them.

use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use stackhour_core::fsutil;
use stackhour_core::registry::engine::{builtin_claude, builtin_codex};
use stackhour_core::registry::{EngineDef, PromptDelivery, ResumeStyle, StreamKind};
use std::path::{Path, PathBuf};

/// `MAX_MEDIA_BYTES` when the legacy config does not set `maxMediaBytes`
/// (coordinator.mjs:33 — `CONFIG.maxMediaBytes || 512 * 1024 * 1024`).
pub const LEGACY_MAX_MEDIA_BYTES: u64 = 512 * 1024 * 1024;

/// The two runnable targets the registry accepts (`registry::TARGETS`).
const KNOWN_TARGETS: &[&str] = &["gcp", "mac"];

/// The legacy `/help` body (coordinator.mjs:338), joined with `\n`.
const LEGACY_HELP: &str = "<b>Claude + Codex bridge</b> (distributed)\n\n\
🧠 /claude — use Claude Code\n\
🛠 /codex — use Codex\n\
🖥️ /mac — run on the Mac\n\
☁️ /gcp — run on the GCP box\n\
🚀 /ship — ship a Blort task (Notion→PR)\n\
ℹ️ /where — active engine, target &amp; session\n\
🆕 /new — fresh session for this engine + target\n\
⏹ /stop — kill/cancel the running job\n\
🎛 /menu — tap-button controls\n\n\
<i>Anything else → selected engine on the active target.</i>";

/// `ORWELL_RULES` (coordinator.mjs:227-235), joined with `\n`.
const ORWELL_RULES: &str = "Follow Orwell's writing rules in every reply:\n\
1. Never use a metaphor, simile, or other figure of speech you are used to seeing in print.\n\
2. Never use a long word where a short one will do.\n\
3. If it is possible to cut a word out, always cut it out.\n\
4. Never use the passive where the active will do.\n\
5. Never use a foreign phrase, a scientific word, or jargon where plain English will do.\n\
6. Break any of these rules sooner than say anything outright barbarous.";

/// The agent directory that carries [`ORWELL_RULES`].
const ORWELL_AGENT: &str = "orwell";

/// The legacy codex fallback path (coordinator.mjs:244), with the owner's
/// home replaced by the migrating user's — see [`resolve_codex_bin`].
const LEGACY_CODEX_BIN_SUFFIX: &str = ".local/bin/codex";

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// What the caller asked for. `from` has NO default on purpose: auto-
/// discovering `~/.claude-remote/config.json` invites an accidental run
/// against the owner's live bridge.
#[derive(Debug, Clone)]
pub struct MigrateOptions {
    /// The legacy `config.json` to read.
    pub from: PathBuf,
    /// Registry root to write (`StoragePaths::config_dir`).
    pub to: PathBuf,
    /// Bridge runtime dir (`BridgePaths::runtime_dir`).
    pub runtime_dir: PathBuf,
    /// Print the plan, write nothing.
    pub dry_run: bool,
    /// Back up and overwrite conflicting files.
    pub force: bool,
    /// Freeze `engines/claude.toml` + `engines/codex.toml` to disk.
    pub engines: bool,
    /// The migrating user's home, used to expand the legacy codex fallback.
    pub home: PathBuf,
}

// ---------------------------------------------------------------------------
// Legacy config
// ---------------------------------------------------------------------------

/// The parsed legacy coordinator config, plus the set of top-level keys seen
/// (so `--verify` can report a key that reached no destination).
#[derive(Debug, Clone)]
pub struct LegacyConfig {
    pub token: String,
    pub chat_id: Value,
    pub default_target: Option<String>,
    pub max_media_bytes: Option<u64>,
    pub eleven_labs_api_key: Option<String>,
    /// `targets`, verbatim and in file order.
    pub targets: IndexMap<String, Value>,
    /// Every top-level key present in the file, in order.
    pub keys: Vec<String>,
}

impl LegacyConfig {
    /// Parse a legacy `config.json`.
    ///
    /// Deliberately lenient about everything except `targets`: a missing
    /// token is a warning, not a parse failure, because a half-configured
    /// legacy file should still produce a plan the owner can read.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
        let obj = value
            .as_object()
            .ok_or_else(|| "top level must be a JSON object".to_string())?;

        let mut targets = IndexMap::new();
        match obj.get("targets") {
            None | Some(Value::Null) => {}
            Some(Value::Object(map)) => {
                for (name, entry) in map {
                    if !entry.is_object() {
                        return Err(format!("targets.{name} must be an object"));
                    }
                    targets.insert(name.clone(), entry.clone());
                }
            }
            Some(_) => return Err("targets must be an object".to_string()),
        }

        let string =
            |key: &str| -> Option<String> { obj.get(key).and_then(Value::as_str).map(str::to_string) };

        Ok(LegacyConfig {
            token: string("token").unwrap_or_default(),
            chat_id: obj.get("chatId").cloned().unwrap_or(Value::Null),
            default_target: string("defaultTarget").filter(|s| !s.trim().is_empty()),
            max_media_bytes: obj.get("maxMediaBytes").and_then(Value::as_u64),
            eleven_labs_api_key: string("elevenLabsApiKey"),
            targets,
            keys: obj.keys().cloned().collect(),
        })
    }

    /// Read and parse, with the path in the error message.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// How a planned file meets an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// The file must not exist. If it does, that is a conflict.
    Create,
    /// The file may exist; `contents` is already the merged result. Only a
    /// genuine value collision (recorded separately) is a conflict.
    Merge,
}

/// One file the migration would write.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub path: PathBuf,
    /// Path as shown in the transcript (relative to its root when possible).
    pub label: String,
    pub mode: u32,
    pub contents: Vec<u8>,
    pub disposition: Disposition,
    /// Right-hand column of the transcript line.
    pub summary: String,
    /// Extra indented lines (masked secrets, key names).
    pub detail: Vec<String>,
    /// True when this file already exists and `Merge` folded it in.
    pub merged_existing: bool,
}

/// The whole migration, computed without touching the destination.
#[derive(Debug, Clone)]
pub struct Plan {
    pub from: PathBuf,
    pub to: PathBuf,
    pub runtime_dir: PathBuf,
    pub files: Vec<PlannedFile>,
    pub warnings: Vec<String>,
    /// Legacy top-level keys that reached no destination.
    pub unmapped: Vec<String>,
}

impl Plan {
    /// Destinations that already exist and would be clobbered.
    pub fn conflicts(&self) -> Vec<&PlannedFile> {
        self.files
            .iter()
            .filter(|f| f.disposition == Disposition::Create && f.path.exists())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Building the plan
// ---------------------------------------------------------------------------

/// Compute the full migration plan. Performs reads only.
pub fn build_plan(legacy: &LegacyConfig, opts: &MigrateOptions) -> Plan {
    let mut files: Vec<PlannedFile> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut consumed: Vec<&str> = Vec::new();

    let cfg = &opts.to;
    let rt = &opts.runtime_dir;

    // ---- runtime secrets -------------------------------------------------
    // `load_coordinator_cfg` reads `token`, `chatId` and `elevenLabsApiKey`
    // from the RUNTIME config.json — the same keys, in the same file, as the
    // Node coordinator. They are folded into that file (written 0600 below)
    // rather than a separate secrets.json no loader ever consulted.
    let mut rt_secrets = Map::new();
    let mut rt_detail: Vec<String> = Vec::new();
    if !legacy.token.is_empty() {
        rt_secrets.insert("token".into(), json!(legacy.token));
        rt_detail.push(format!("token                          {}", mask(&legacy.token)));
        consumed.push("token");
    } else {
        warnings.push("'token' is missing or empty in the source; the bridge will not authenticate until you add 'token' to the runtime config.json".into());
    }
    if !legacy.chat_id.is_null() {
        rt_secrets.insert("chatId".into(), legacy.chat_id.clone());
        rt_detail.push(format!("chatId                         {}", legacy.chat_id));
        consumed.push("chatId");
    } else {
        warnings
            .push("'chatId' is missing in the source; the bridge would accept messages from nobody".into());
    }
    match legacy.eleven_labs_api_key.as_deref() {
        Some("") => {
            warnings.push(
                "'elevenLabsApiKey' is empty in the source; the key is omitted and voice transcription stays off"
                    .into(),
            );
            consumed.push("elevenLabsApiKey");
        }
        Some(key) => {
            rt_secrets.insert("elevenLabsApiKey".into(), json!(key));
            rt_detail.push(format!("elevenLabsApiKey               {}", mask(key)));
            consumed.push("elevenLabsApiKey");
        }
        None => {}
    }

    // ---- config.json (MERGED: the tracker owns this file) --------------
    let (existing_cfg, cfg_existed) = read_json_object(&cfg.join("config.json"));
    let mut merged_cfg = existing_cfg.clone();
    let mut bridge_obj = merged_cfg
        .get("bridge")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut bridge_summary: Vec<String> = Vec::new();
    let mut collisions: Vec<String> = Vec::new();

    let mut set_bridge = |obj: &mut Map<String, Value>, key: &str, want: &str| {
        if let Some(old) = obj.get(key).and_then(Value::as_str) {
            if old != want {
                collisions.push(format!("bridge.{key} is already {old:?}, would become {want:?}"));
            }
        }
        obj.insert(key.into(), json!(want));
        bridge_summary.push(format!("bridge.{key} = {want:?}"));
    };

    set_bridge(&mut bridge_obj, "defaultEngine", "claude");
    if let Some(target) = &legacy.default_target {
        set_bridge(&mut bridge_obj, "defaultTarget", target);
        consumed.push("defaultTarget");
        if !KNOWN_TARGETS.contains(&target.as_str()) {
            warnings.push(format!(
                "defaultTarget '{target}' is not one of {} — the registry will report it as an error and fall back to 'gcp'",
                KNOWN_TARGETS.join("|")
            ));
        }
    }
    set_bridge(&mut bridge_obj, "defaultAgent", ORWELL_AGENT);
    merged_cfg.insert("bridge".into(), Value::Object(bridge_obj));
    files.push(PlannedFile {
        path: cfg.join("config.json"),
        label: "config.json".into(),
        mode: 0o644,
        contents: pretty_json(&Value::Object(merged_cfg)),
        disposition: Disposition::Merge,
        summary: bridge_summary.join(", "),
        detail: Vec::new(),
        merged_existing: cfg_existed,
    });
    for c in &collisions {
        warnings.push(format!("config.json: {c} (existing value is replaced)"));
    }


    // ---- engines/ -------------------------------------------------------
    if opts.engines {
        for def in [builtin_claude(), builtin_codex()] {
            let text = engine_toml(&def);
            files.push(PlannedFile {
                path: cfg.join("engines").join(format!("{}.toml", def.name)),
                label: format!("engines/{}.toml", def.name),
                mode: 0o644,
                contents: text.clone().into_bytes(),
                disposition: Disposition::Create,
                summary: size_label(text.len()),
                detail: Vec::new(),
                merged_existing: false,
            });
        }
        warnings.push(
            "engines/*.toml are FROZEN copies of the built-ins: they no longer pick up upstream argv fixes. Re-run with --no-engines to track the built-ins instead."
                .into(),
        );
    }

    // ---- agents/orwell (the --append-system-prompt text) ----------------
    files.push(PlannedFile {
        path: cfg.join("agents").join(ORWELL_AGENT).join("agent.toml"),
        label: format!("agents/{ORWELL_AGENT}/agent.toml"),
        mode: 0o644,
        contents: orwell_agent_toml().into_bytes(),
        disposition: Disposition::Create,
        summary: "engine = \"claude\"".into(),
        detail: Vec::new(),
        merged_existing: false,
    });
    let soul = format!("{ORWELL_RULES}\n");
    files.push(PlannedFile {
        path: cfg.join("agents").join(ORWELL_AGENT).join("soul.md"),
        label: format!("agents/{ORWELL_AGENT}/soul.md"),
        mode: 0o644,
        contents: soul.clone().into_bytes(),
        disposition: Disposition::Create,
        summary: format!("{} lines", soul.lines().count()),
        detail: Vec::new(),
        merged_existing: false,
    });

    // ---- prompts/ -------------------------------------------------------
    let help = format!("{LEGACY_HELP}\n");
    files.push(PlannedFile {
        path: cfg.join("prompts").join("help.md"),
        label: "prompts/help.md".into(),
        mode: 0o644,
        contents: help.clone().into_bytes(),
        disposition: Disposition::Create,
        summary: format!("{} lines", help.lines().count()),
        detail: Vec::new(),
        merged_existing: false,
    });
    let ship_prompt = "/ship {{args}}\n";
    files.push(PlannedFile {
        path: cfg.join("prompts").join("ship.md"),
        label: "prompts/ship.md".into(),
        mode: 0o644,
        contents: ship_prompt.as_bytes().to_vec(),
        disposition: Disposition::Create,
        summary: "1 line".into(),
        detail: Vec::new(),
        merged_existing: false,
    });

    // ---- commands/ ------------------------------------------------------
    for (name, text) in migratable_commands() {
        files.push(PlannedFile {
            path: cfg.join("commands").join(format!("{name}.toml")),
            label: format!("commands/{name}.toml"),
            mode: 0o644,
            contents: text.clone().into_bytes(),
            disposition: Disposition::Create,
            summary: first_description(&text),
            detail: Vec::new(),
            merged_existing: false,
        });
    }
    warnings.push(
        "/help /menu /stop are RESERVED and /where /new are kind=\"builtin\": a user file cannot define them, so they keep the shipped definitions (whose descriptions already match the legacy setMyCommands strings byte for byte)"
            .into(),
    );

    // ---- runtime config.json (secrets + ship + targets + media cap) ------
    let codex_bin = resolve_codex_bin(&opts.home);
    let mut rt_targets = Map::new();
    for (name, entry) in &legacy.targets {
        let mut obj = entry.as_object().cloned().unwrap_or_default();
        if obj.get("type").and_then(Value::as_str) == Some("local") && !obj.contains_key("codexBin") {
            obj.insert("codexBin".into(), json!(codex_bin.display().to_string()));
            warnings.push(format!(
                "target '{name}' had no codexBin and relied on coordinator.mjs:244's hardcoded fallback; materialised as {}",
                codex_bin.display()
            ));
        }
        if !KNOWN_TARGETS.contains(&name.as_str()) {
            warnings.push(format!(
                "target '{name}' is outside the registry's switch surface (targets are pinned to {}); only /ship reaches it, via the runtime 'ship' key",
                KNOWN_TARGETS.join("|")
            ));
        }
        rt_targets.insert(name.clone(), Value::Object(obj));
    }
    if !legacy.targets.is_empty() {
        consumed.push("targets");
    }
    let max_media = legacy.max_media_bytes.unwrap_or(LEGACY_MAX_MEDIA_BYTES);
    if legacy.max_media_bytes.is_none() {
        warnings.push(format!(
            "'maxMediaBytes' absent in source; writing the legacy default {max_media} explicitly so it stops being invisible"
        ));
    } else {
        consumed.push("maxMediaBytes");
    }
    // PARITY coordinator.mjs:361-366: `/ship` hardcodes `state.active =
    // 'blort'; state.engine = 'claude'`. The Rust `ship_cfg` seeds the ship
    // target from `defaultTarget` unless this runtime key exists, so the
    // migration materialises the live destination explicitly — without it,
    // `/ship` would park on gcp after cutover.
    let mut rt_obj = rt_secrets;
    rt_obj.insert("ship".into(), json!({ "target": "blort", "engine": "claude" }));
    rt_detail.push("ship                           target=blort engine=claude".into());
    rt_obj.insert("maxMediaBytes".into(), json!(max_media));
    rt_obj.insert("targets".into(), Value::Object(rt_targets));
    let rt_doc = Value::Object(rt_obj);
    files.push(PlannedFile {
        path: rt.join("config.json"),
        label: format!("{}", rt.join("config.json").display()),
        mode: 0o600,
        contents: pretty_json(&rt_doc),
        disposition: Disposition::Create,
        summary: format!("{} target{} + secrets", legacy.targets.len(), plural(legacy.targets.len())),
        detail: rt_detail,
        merged_existing: false,
    });

    let unmapped = legacy
        .keys
        .iter()
        .filter(|k| !consumed.contains(&k.as_str()))
        .cloned()
        .collect();

    Plan {
        from: opts.from.clone(),
        to: opts.to.clone(),
        runtime_dir: opts.runtime_dir.clone(),
        files,
        warnings,
        unmapped,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The `--dry-run` transcript. Secrets are ALWAYS masked; there is no flag
/// that unmasks them.
pub fn render_plan(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "migrate {} → {}\n\n",
        plan.from.display(),
        plan.to.display()
    ));
    let conflicts = plan.conflicts();
    for f in &plan.files {
        let verb = if f.disposition == Disposition::Merge && f.merged_existing {
            "update"
        } else if f.path.exists() {
            "CLOBBER"
        } else {
            "create"
        };
        out.push_str(&format!(
            "  {verb:<7} {:<34} {:04o}  {}\n",
            f.label, f.mode, f.summary
        ));
        for d in &f.detail {
            out.push_str(&format!("            {d}\n"));
        }
    }
    if !plan.warnings.is_empty() {
        out.push('\n');
        for w in &plan.warnings {
            out.push_str(&format!("  warn    {w}\n"));
        }
    }
    if !plan.unmapped.is_empty() {
        out.push('\n');
        for key in &plan.unmapped {
            out.push_str(&format!("  warn    legacy key '{key}' reached no destination\n"));
        }
    }
    out.push_str(&format!(
        "\nnothing written (--dry-run). {} file{} would be written, {} conflict{}.\n",
        plan.files.len(),
        plural(plan.files.len()),
        conflicts.len(),
        plural(conflicts.len())
    ));
    out
}

/// The exit-2 message printed when destinations already exist.
pub fn render_conflicts(plan: &Plan) -> String {
    let conflicts = plan.conflicts();
    let mut out = String::from("stackhour: refusing to migrate — these files already exist:\n");
    for f in &conflicts {
        out.push_str(&format!("  {}\n", f.path.display()));
    }
    out.push_str(
        "\nNothing was written. Three ways forward:\n\
         \x20 --to <a fresh directory>   migrate somewhere else\n\
         \x20 --force                    back each file up to <name>.bak-<unix-ts>, then overwrite\n\
         \x20 delete the files above, then re-run\n",
    );
    out
}

// ---------------------------------------------------------------------------
// Applying
// ---------------------------------------------------------------------------

/// What `apply` did.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub written: Vec<PathBuf>,
    pub backups: Vec<(PathBuf, PathBuf)>,
}

/// Write the plan. Two-phase and all-or-nothing about CONFLICTS: the caller
/// must have checked [`Plan::conflicts`] first (or passed `--force`).
///
/// Every write goes through [`fsutil::atomic_write`] (tmp + rename, mode set
/// on create AND re-chmodded on an existing file), so a crash leaves either
/// the old file or the new one, never a truncated hybrid.
pub fn apply(plan: &Plan, force: bool) -> Result<Applied, String> {
    let mut applied = Applied::default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Phase 1: back up everything that would be clobbered, before writing a
    // single byte. A backup name that is itself taken aborts the run.
    if force {
        for f in plan.conflicts() {
            let backup = f.path.with_file_name(format!(
                "{}.bak-{now}",
                f.path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            ));
            if backup.exists() {
                return Err(format!(
                    "backup path {} already exists; refusing to overwrite a backup",
                    backup.display()
                ));
            }
            std::fs::copy(&f.path, &backup).map_err(|e| format!("{}: {e}", backup.display()))?;
            applied.backups.push((f.path.clone(), backup));
        }
    }

    // Phase 2: write.
    for f in &plan.files {
        if let Some(parent) = f.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        fsutil::atomic_write(&f.path, &f.contents, f.mode)
            .map_err(|e| format!("{}: {e}", f.path.display()))?;
        applied.written.push(f.path.clone());
    }
    Ok(applied)
}

// ---------------------------------------------------------------------------
// Verify
// ---------------------------------------------------------------------------

/// One thing `--verify` found wrong.
pub type Discrepancy = String;

/// Re-read both sides and assert the mapping still holds.
///
/// This is the check none of the `bridge show --…` diffs can do: it catches a
/// legacy key that reached NO destination, and a destination file that has
/// drifted from what the legacy config says it should contain.
pub fn verify(plan: &Plan) -> Vec<Discrepancy> {
    let mut out: Vec<Discrepancy> = Vec::new();
    for f in &plan.files {
        match std::fs::read(&f.path) {
            Err(e) => out.push(format!("{}: missing ({e})", f.path.display())),
            Ok(actual) => {
                if actual != f.contents {
                    out.push(format!(
                        "{}: on-disk contents differ from what the legacy config maps to",
                        f.path.display()
                    ));
                }
                #[cfg(unix)]
                if let Ok(meta) = std::fs::metadata(&f.path) {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = meta.permissions().mode() & 0o777;
                    if mode != f.mode {
                        out.push(format!(
                            "{}: mode is {mode:04o}, expected {:04o}",
                            f.path.display(),
                            f.mode
                        ));
                    }
                }
            }
        }
    }
    for key in &plan.unmapped {
        out.push(format!("legacy key '{key}' reached no destination"));
    }
    out
}

// ---------------------------------------------------------------------------
// Generated file bodies
// ---------------------------------------------------------------------------

/// The command files a user file is actually ALLOWED to define, in the order
/// coordinator.mjs registered them. Descriptions are byte-identical to
/// `registerCommands()` (coordinator.mjs:163-169), trailing emoji included.
fn migratable_commands() -> Vec<(&'static str, String)> {
    let header = "# Frozen by `stackhour bridge migrate` from coordinator.mjs registerCommands().\n\
                  # The description is byte-identical to the legacy setMyCommands entry.\n";
    vec![
        (
            "claude",
            format!(
                "{header}\n\
                 description = \"Use Claude Code 🧠\"\n\
                 kind = \"engine\"\n\
                 engine = \"claude\"\n\
                 keyboard = true\n\
                 button = \"🧠 Claude\"\n\
                 button_order = 10\n"
            ),
        ),
        (
            "codex",
            format!(
                "{header}\n\
                 description = \"Use Codex 🛠\"\n\
                 kind = \"engine\"\n\
                 engine = \"codex\"\n\
                 keyboard = true\n\
                 button = \"🛠 Codex\"\n\
                 button_order = 11\n"
            ),
        ),
        (
            "mac",
            format!(
                "{header}\n\
                 description = \"Run on the Mac 🖥️\"\n\
                 kind = \"target\"\n\
                 target = \"mac\"\n\
                 aliases = [\"local\"]\n\
                 keyboard = true\n\
                 button = \"🖥️ Mac\"\n\
                 button_order = 20\n"
            ),
        ),
        (
            "gcp",
            format!(
                "{header}\n\
                 description = \"Run on the GCP box ☁️\"\n\
                 kind = \"target\"\n\
                 target = \"gcp\"\n\
                 aliases = [\"remote\"]\n\
                 keyboard = true\n\
                 button = \"☁️ GCP\"\n\
                 button_order = 21\n"
            ),
        ),
        (
            "ship",
            format!(
                "{header}\
                 #\n\
                 # coordinator.mjs:361-366 also set `state.active = 'blort'`. There is no\n\
                 # `target = \"blort\"` here because the registry pins targets to gcp|mac and\n\
                 # would drop this whole file. /ship therefore runs on the ACTIVE target\n\
                 # until the target list becomes data. This is the one place the Rust bridge\n\
                 # is a strict subset of the Node one.\n\
                 \n\
                 description = \"Ship a Blort task 🚀\"\n\
                 kind = \"prompt\"\n\
                 template = \"ship\"\n\
                 engine = \"claude\"\n\
                 keyboard = false\n\
                 \n\
                 [[args]]\n\
                 name = \"task\"\n\
                 rest = true\n\
                 description = \"text, an ECM-xxxx id, or a Slack link\"\n"
            ),
        ),
    ]
}

/// `agents/orwell/agent.toml`.
fn orwell_agent_toml() -> String {
    "# Frozen by `stackhour bridge migrate`.\n\
     #\n\
     # coordinator.mjs passed ORWELL_RULES to claude as `--append-system-prompt`, and\n\
     # prepended it to the first codex prompt. In the Rust bridge that text is an\n\
     # agent's SOUL: the engine's system_prompt_args template carries\n\
     # {{system_prompt}}, and the active agent supplies the body. `bridge.defaultAgent`\n\
     # in config.json selects this agent, which reproduces the legacy behaviour for\n\
     # both engines.\n\
     #\n\
     # `model` and `permission_mode` are deliberately unset: those stay per-target, as\n\
     # they were in the legacy targets{} map.\n\
     \n\
     label = \"Orwell\"\n\
     engine = \"claude\"\n\
     soul = \"soul.md\"\n"
        .to_string()
}

/// Render an [`EngineDef`] as a `engines/<name>.toml` document that
/// `EngineDef::from_toml` parses back into an identical value.
pub fn engine_toml(def: &EngineDef) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Frozen by `stackhour bridge migrate` from the built-in '{}' engine, whose argv\n\
         # is byte-identical to coordinator.mjs spawnLocal(). Freezing it makes the\n\
         # migration auditable; the cost is that upstream argv fixes no longer reach it.\n\
         # Delete this file to go back to tracking the built-in.\n\
         #\n\
         # `bin` is the bare name resolved on PATH. A per-target claudeBin/codexBin in\n\
         # the runtime config still wins over it.\n\n",
        def.name
    ));
    out.push_str(&format!("label = {}\n", toml_string(&def.label)));
    out.push_str(&format!("emoji = {}\n", toml_string(&def.emoji)));
    out.push_str(&format!("bin = {}\n", toml_string(&def.bin)));
    out.push_str(&format!("kind = {}\n", toml_string(stream_kind_str(def.kind))));
    out.push_str(&format!("args = {}\n", toml_array(&def.args)));
    out.push_str(&format!(
        "prompt = {}\n",
        toml_string(match def.prompt_delivery {
            PromptDelivery::Stdin => "stdin",
            PromptDelivery::LastArg => "last-arg",
        })
    ));
    for (key, value) in [
        ("model_args", &def.model_args),
        ("permission_args", &def.permission_args),
        ("system_prompt_args", &def.system_prompt_args),
        ("effort_args", &def.effort_args),
        ("allowed_tools_args", &def.allowed_tools_args),
        ("disallowed_tools_args", &def.disallowed_tools_args),
    ] {
        if let Some(v) = value {
            out.push_str(&format!("{key} = {}\n", toml_array(v)));
        }
    }
    if let Some(flag) = &def.partial_messages_flag {
        out.push_str(&format!("partial_messages_flag = {}\n", toml_string(flag)));
    }
    match &def.resume {
        ResumeStyle::Flag { args } if args.is_empty() => {}
        ResumeStyle::Flag { args } => {
            out.push_str(&format!("\n[resume]\nflag = {}\n", toml_array(args)));
        }
        ResumeStyle::Subcommand { insert } => {
            out.push_str(&format!("\n[resume]\nsubcommand = {}\n", toml_string(insert)));
        }
    }
    if !def.env.is_empty() {
        out.push_str("\n[env]\n");
        for (k, v) in &def.env {
            out.push_str(&format!("{k} = {}\n", toml_string(v)));
        }
    }
    out
}

fn stream_kind_str(kind: StreamKind) -> &'static str {
    match kind {
        StreamKind::ClaudeStreamJson => "claude-stream-json",
        StreamKind::CodexJsonl => "codex-jsonl",
        StreamKind::PlainLines => "plain-lines",
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Mask a secret as first-4 + last-4 + length. Short secrets show nothing but
/// their length — four of eight characters is most of a short key.
pub fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    let n = chars.len();
    if n <= 8 {
        return format!("…                 ({n} chars)");
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[n - 4..].iter().collect();
    format!("{head}…{tail:<12} ({n} chars)")
}

/// Where `codex` actually is, for the target entries that relied on
/// coordinator.mjs:244's hardcoded `<one developer's home>/.local/bin/codex`.
///
/// PATH first, so a migration on any box materialises a path that works
/// there; the legacy layout under the MIGRATING user's home as the fallback,
/// because baking one developer's absolute path into shared code is the bug
/// this is working around, not a thing to copy.
fn resolve_codex_bin(home: &Path) -> PathBuf {
    if let Some(found) = find_on_path("codex") {
        return found;
    }
    home.join(LEGACY_CODEX_BIN_SUFFIX)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn read_json_object(path: &Path) -> (Map<String, Value>, bool) {
    match std::fs::read_to_string(path) {
        Err(_) => (Map::new(), false),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => (map, true),
            _ => (Map::new(), true),
        },
    }
}

fn pretty_json(v: &Value) -> Vec<u8> {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into());
    s.push('\n');
    s.into_bytes()
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn size_label(bytes: usize) -> String {
    if bytes < 1000 {
        format!("{bytes} B")
    } else {
        format!("{:.1} kB", bytes as f64 / 1000.0)
    }
}

/// The `description = "…"` value of a generated command file, for the
/// transcript's right-hand column.
fn first_description(toml_text: &str) -> String {
    toml_text
        .lines()
        .find(|l| l.starts_with("description = "))
        .map(|l| l.trim_start_matches("description = ").to_string())
        .unwrap_or_default()
}

/// A TOML basic string. Only the escapes TOML requires.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_array(items: &[String]) -> String {
    let body: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
    format!("[{}]", body.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stackhour_core::registry;

    const FIXTURE: &str = include_str!("../../../tests-fixtures/bridge/legacy-config.json");
    const FIXTURE_MAX: &str = include_str!("../../../tests-fixtures/bridge/legacy-config-maximal.json");

    fn opts(dir: &Path) -> MigrateOptions {
        MigrateOptions {
            from: PathBuf::from("legacy-config.json"),
            to: dir.join("config"),
            runtime_dir: dir.join("runtime"),
            dry_run: false,
            force: false,
            engines: true,
            home: dir.join("home"),
        }
    }

    fn plan_of(text: &str, dir: &Path) -> Plan {
        let legacy = LegacyConfig::parse(text).expect("fixture parses");
        build_plan(&legacy, &opts(dir))
    }

    fn file<'a>(plan: &'a Plan, label: &str) -> &'a PlannedFile {
        plan.files
            .iter()
            .find(|f| f.label == label)
            .unwrap_or_else(|| panic!("no planned file {label}"))
    }

    // ---- parsing --------------------------------------------------------

    #[test]
    fn the_owners_shape_parses_with_all_three_targets() {
        let legacy = LegacyConfig::parse(FIXTURE).unwrap();
        assert_eq!(legacy.default_target.as_deref(), Some("gcp"));
        assert_eq!(
            legacy.targets.keys().collect::<Vec<_>>(),
            vec!["gcp", "mac", "blort"]
        );
        // coordinator.mjs:33 defaults this in CODE; the real file omits it.
        assert_eq!(legacy.max_media_bytes, None);
        assert!(!legacy.token.is_empty());
    }

    #[test]
    fn the_maximal_shape_parses_every_optional_key() {
        let legacy = LegacyConfig::parse(FIXTURE_MAX).unwrap();
        assert_eq!(legacy.max_media_bytes, Some(536_870_912));
        assert_eq!(legacy.default_target.as_deref(), Some("mac"));
        assert_eq!(legacy.eleven_labs_api_key.as_deref(), Some(""));
    }

    #[test]
    fn a_non_object_config_is_rejected_rather_than_half_migrated() {
        assert!(LegacyConfig::parse("[]").is_err());
        assert!(LegacyConfig::parse("{").is_err());
        assert!(LegacyConfig::parse(r#"{"targets": 7}"#).is_err());
    }

    // ---- secrets --------------------------------------------------------

    #[test]
    fn every_secret_lands_in_one_0600_file_and_nowhere_else() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        let legacy = LegacyConfig::parse(FIXTURE).unwrap();

        // The runtime config.json — the file the coordinator reads — is the
        // single secret-bearing file.
        let runtime = plan.files.last().unwrap();
        assert_eq!(runtime.mode, 0o600);
        let body = String::from_utf8(runtime.contents.clone()).unwrap();
        assert!(body.contains(&legacy.token));
        assert!(body.contains("sk_TEST-ELEVENLABS-KEY-NOT-REAL"));

        for f in &plan.files {
            if f.path == runtime.path {
                continue;
            }
            let text = String::from_utf8_lossy(&f.contents);
            assert!(!text.contains(&legacy.token), "{} leaked the bot token", f.label);
            assert!(
                !text.contains("sk_TEST-ELEVENLABS-KEY"),
                "{} leaked the ElevenLabs key",
                f.label
            );
        }
    }

    #[test]
    fn an_empty_elevenlabs_key_is_omitted_rather_than_written_blank() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE_MAX, dir.path());
        let body = String::from_utf8(plan.files.last().unwrap().contents.clone()).unwrap();
        assert!(!body.contains("elevenLabsApiKey"));
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("voice transcription stays off")));
    }

    #[test]
    fn the_transcript_never_prints_a_secret_in_the_clear() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        let legacy = LegacyConfig::parse(FIXTURE).unwrap();
        let out = render_plan(&plan);
        assert!(!out.contains(&legacy.token));
        assert!(!out.contains("sk_TEST-ELEVENLABS-KEY-NOT-REAL"));
        // ...but enough of it to tell WHICH token was read.
        assert!(out.contains("1111…AAAA"));
        assert!(out.contains(&format!("({} chars)", legacy.token.chars().count())));
    }

    #[test]
    fn masking_a_short_secret_reveals_nothing_but_its_length() {
        assert_eq!(mask("abcd").trim(), "…                 (4 chars)".trim());
        assert!(!mask("abcdefgh").contains("abcd"));
    }

    /// No file under the (committable) config dir carries a secret, so no
    /// .gitignore entry is needed — the secrets live in the runtime dir.
    #[test]
    fn the_config_dir_carries_no_secret_and_needs_no_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        let legacy = LegacyConfig::parse(FIXTURE).unwrap();
        assert!(plan.files.iter().all(|f| f.label != ".gitignore"));
        for f in plan.files.iter().filter(|f| f.path.starts_with(dir.path().join("config"))) {
            let text = String::from_utf8_lossy(&f.contents);
            assert!(!text.contains(&legacy.token), "{} leaked the bot token", f.label);
        }
    }

    // ---- config.json is shared with the tracker -------------------------

    #[test]
    fn the_trackers_own_config_keys_survive_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("config.json"),
            r#"{"server":{"port":41234},"agent":{"machine":"gcp"}}"#,
        )
        .unwrap();

        let plan = plan_of(FIXTURE, dir.path());
        let merged: Value = serde_json::from_slice(&file(&plan, "config.json").contents).unwrap();
        assert_eq!(merged["server"]["port"], json!(41234));
        assert_eq!(merged["agent"]["machine"], json!("gcp"));
        assert_eq!(merged["bridge"]["defaultTarget"], json!("gcp"));
        assert_eq!(merged["bridge"]["defaultEngine"], json!("claude"));
        assert_eq!(merged["bridge"]["defaultAgent"], json!("orwell"));
    }

    #[test]
    fn config_json_carries_no_secret_and_stays_world_readable() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        let f = file(&plan, "config.json");
        assert_eq!(f.mode, 0o644);
        let text = String::from_utf8(f.contents.clone()).unwrap();
        assert!(!text.contains("token"));
        assert!(!text.contains("chatId"));
    }

    // ---- runtime config -------------------------------------------------

    #[test]
    fn the_runtime_config_gets_targets_credentials_and_the_ship_destination() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        let legacy = LegacyConfig::parse(FIXTURE).unwrap();
        let f = plan.files.last().unwrap();
        assert_eq!(f.mode, 0o600);
        let doc: Value = serde_json::from_slice(&f.contents).unwrap();
        assert_eq!(doc["maxMediaBytes"], json!(LEGACY_MAX_MEDIA_BYTES));
        assert_eq!(doc["targets"]["gcp"]["label"], json!("☁️ GCP"));
        assert_eq!(doc["targets"]["mac"]["type"], json!("worker"));
        assert_eq!(
            doc["targets"]["blort"]["cwd"],
            json!("/home/testuser/projects/example")
        );
        // The credentials the coordinator loader actually reads (§1.4 of the
        // migration runbook used to call their absence "the gap").
        assert_eq!(doc["token"], json!(legacy.token));
        assert_eq!(doc["chatId"], legacy.chat_id);
        // PARITY coordinator.mjs:361-366: /ship must keep parking on blort.
        assert_eq!(doc["ship"]["target"], json!("blort"));
        assert_eq!(doc["ship"]["engine"], json!("claude"));
    }

    #[test]
    fn an_absent_media_cap_is_written_out_explicitly_and_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        assert!(plan.warnings.iter().any(|w| w.contains("536870912")));
    }

    #[test]
    fn an_explicit_media_cap_is_copied_without_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE_MAX, dir.path());
        assert!(!plan.warnings.iter().any(|w| w.contains("absent in source")));
    }

    /// coordinator.mjs:244 falls back to a hardcoded absolute codex path that
    /// no target sets. Leaving it implicit breaks codex after cutover.
    #[test]
    fn local_targets_get_an_explicit_codex_bin() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        let doc: Value = serde_json::from_slice(&plan.files.last().unwrap().contents).unwrap();
        assert!(doc["targets"]["gcp"]["codexBin"].is_string());
        assert!(doc["targets"]["blort"]["codexBin"].is_string());
        // The worker lane runs codex on the OTHER machine; nothing local to
        // materialise.
        assert!(doc["targets"]["mac"].get("codexBin").is_none());
        assert!(plan.warnings.iter().any(|w| w.contains("coordinator.mjs:244")));
    }

    #[test]
    fn an_existing_codex_bin_is_left_exactly_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE_MAX, dir.path());
        let doc: Value = serde_json::from_slice(&plan.files.last().unwrap().contents).unwrap();
        assert_eq!(
            doc["targets"]["gcp"]["codexBin"],
            json!("/home/testuser/.local/bin/codex")
        );
    }

    #[test]
    fn the_unmigratable_third_target_is_warned_about_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        let doc: Value = serde_json::from_slice(&plan.files.last().unwrap().contents).unwrap();
        assert!(doc["targets"]["blort"].is_object(), "blort must survive");
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("'blort' is outside the registry's switch surface")));
    }

    // ---- generated registry files load ----------------------------------

    /// The load-bearing test: write the plan, then load the result through
    /// the REAL registry loader and demand zero errors.
    #[test]
    fn the_migrated_directory_loads_with_zero_errors() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        apply(&plan, false).expect("apply");

        let reg = registry::load(&dir.path().join("config"));
        let rendered: Vec<String> = reg.errors.iter().map(ToString::to_string).collect();
        assert!(
            reg.errors.is_empty(),
            "migrated config must be valid, got:\n  {}",
            rendered.join("\n  ")
        );
    }

    #[test]
    fn the_migrated_directory_defines_ship_and_keeps_the_legacy_descriptions() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        apply(&plan, false).expect("apply");
        let reg = registry::load(&dir.path().join("config"));

        let table = stackhour_core::registry::command::effective_table(&reg.commands);
        for (name, want) in [
            ("claude", "Use Claude Code 🧠"),
            ("codex", "Use Codex 🛠"),
            ("mac", "Run on the Mac 🖥️"),
            ("gcp", "Run on the GCP box ☁️"),
            ("ship", "Ship a Blort task 🚀"),
            ("where", "Show active target & session"),
            ("new", "Fresh session on active target"),
            ("stop", "Kill/cancel the running job"),
            ("menu", "Show tap-button controls"),
            ("help", "Show command list"),
        ] {
            assert_eq!(
                table.get(name).map(|d| d.description.as_str()),
                Some(want),
                "/{name} description drifted from coordinator.mjs:163-169"
            );
        }
        // ...all ten legacy verbs, and the aliases the legacy handled.
        assert_eq!(table["mac"].aliases, vec!["local".to_string()]);
        assert_eq!(table["gcp"].aliases, vec!["remote".to_string()]);
        assert_eq!(table["where"].aliases, vec!["status".to_string()]);
        assert_eq!(table["new"].aliases, vec!["reset".to_string()]);
    }

    #[test]
    fn the_migrated_keyboard_reproduces_the_legacy_three_by_two_grid() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        apply(&plan, false).expect("apply");
        let reg = registry::load(&dir.path().join("config"));
        let table = stackhour_core::registry::command::effective_table(&reg.commands);

        let state = crate::state::BridgeState::default();
        let kb = crate::keyboard::control_keyboard_for(&state, &table);
        let rows = kb["inline_keyboard"].as_array().unwrap();
        let captions: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                r.as_array()
                    .unwrap()
                    .iter()
                    .map(|b| b["text"].as_str().unwrap().replace("✅ ", ""))
                    .collect()
            })
            .collect();
        assert_eq!(
            captions,
            vec![
                vec!["🧠 Claude".to_string(), "🛠 Codex".to_string()],
                vec!["🖥️ Mac".to_string(), "☁️ GCP".to_string()],
                vec!["🆕 New session".to_string(), "ℹ️ Status".to_string()],
            ]
        );
    }

    #[test]
    fn the_migrated_help_is_the_legacy_help_including_the_ship_line() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        apply(&plan, false).expect("apply");
        let reg = registry::load(&dir.path().join("config"));
        let rendered = reg.prompts.render("help", &[]);
        assert_eq!(rendered.trim_end(), LEGACY_HELP);
        assert!(rendered.contains("🚀 /ship — ship a Blort task (Notion→PR)"));
    }

    #[test]
    fn the_orwell_rules_survive_as_the_default_agents_soul() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        apply(&plan, false).expect("apply");
        let reg = registry::load(&dir.path().join("config"));

        let agent = reg.agents.get(ORWELL_AGENT).expect("orwell agent");
        assert_eq!(agent.engine, "claude");
        let soul = agent.soul.text().expect("soul body");
        assert_eq!(soul.trim_end(), ORWELL_RULES);
        assert_eq!(reg.defaults.agent.as_deref(), Some(ORWELL_AGENT));
    }

    /// A frozen engine file must parse back into exactly the built-in it was
    /// generated from, or the migration silently changes argv.
    #[test]
    fn frozen_engines_round_trip_to_the_builtins() {
        for def in [builtin_claude(), builtin_codex()] {
            let text = engine_toml(&def);
            let value: toml::Value = text.parse().expect("generated engine is valid TOML");
            let back = EngineDef::from_toml(&def.name, &value).expect("parses back");
            assert_eq!(
                format!("{back:?}"),
                format!("{def:?}"),
                "engines/{}.toml is not a faithful copy",
                def.name
            );
        }
    }

    #[test]
    fn no_engines_leaves_the_builtins_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = LegacyConfig::parse(FIXTURE).unwrap();
        let mut o = opts(dir.path());
        o.engines = false;
        let plan = build_plan(&legacy, &o);
        assert!(plan.files.iter().all(|f| !f.label.starts_with("engines/")));
        assert!(!plan.warnings.iter().any(|w| w.contains("FROZEN")));
    }

    #[test]
    fn freezing_engines_says_what_it_costs() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("no longer pick up upstream argv fixes")));
    }

    // ---- clobber protection ---------------------------------------------

    #[test]
    fn a_dry_run_writes_absolutely_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        let _ = render_plan(&plan);
        assert!(!dir.path().join("config").exists());
        assert!(!dir.path().join("runtime").exists());
    }

    #[test]
    fn an_existing_destination_is_a_conflict_and_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        std::fs::create_dir_all(cfg.join("prompts")).unwrap();
        std::fs::write(cfg.join("prompts/help.md"), "MINE\n").unwrap();
        let rt = dir.path().join("runtime");
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(rt.join("config.json"), "{}\n").unwrap();

        let plan = plan_of(FIXTURE, dir.path());
        let conflicts: Vec<String> = plan.conflicts().iter().map(|f| f.label.clone()).collect();
        assert!(conflicts.contains(&"prompts/help.md".to_string()));
        assert!(
            conflicts.iter().any(|l| l.contains("runtime")),
            "the pre-existing runtime config.json must conflict: {conflicts:?}"
        );

        let msg = render_conflicts(&plan);
        assert!(msg.contains("--force"));
        assert!(msg.contains("--to <a fresh directory>"));
        // The caller refuses on conflicts, so the hand-edit survives.
        assert_eq!(
            std::fs::read_to_string(cfg.join("prompts/help.md")).unwrap(),
            "MINE\n"
        );
    }

    #[test]
    fn a_second_run_conflicts_rather_than_silently_succeeding() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        apply(&plan, false).expect("first apply");

        let second = plan_of(FIXTURE, dir.path());
        assert!(
            !second.conflicts().is_empty(),
            "re-running must not look like a no-op success"
        );
    }

    #[test]
    fn force_backs_each_clobbered_file_up_before_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        std::fs::create_dir_all(cfg.join("prompts")).unwrap();
        std::fs::write(cfg.join("prompts/help.md"), "MINE\n").unwrap();

        let plan = plan_of(FIXTURE, dir.path());
        let applied = apply(&plan, true).expect("forced apply");
        assert_eq!(applied.backups.len(), 1);
        let (_, backup) = &applied.backups[0];
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "MINE\n");
        assert!(std::fs::read_to_string(cfg.join("prompts/help.md"))
            .unwrap()
            .contains("Claude + Codex bridge"));
    }

    #[test]
    fn force_refuses_when_the_backup_name_is_already_taken() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        std::fs::create_dir_all(cfg.join("prompts")).unwrap();
        std::fs::write(cfg.join("prompts/help.md"), "MINE\n").unwrap();
        let plan = plan_of(FIXTURE, dir.path());

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(cfg.join(format!("prompts/help.md.bak-{now}")), "OLDER\n").unwrap();

        let err = apply(&plan, true).unwrap_err();
        assert!(err.contains("refusing to overwrite a backup"));
    }

    // ---- modes ----------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn applied_modes_match_the_plan_and_repair_a_loose_secrets_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        apply(&plan, false).expect("apply");
        for f in &plan.files {
            let mode = std::fs::metadata(&f.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, f.mode, "{} has mode {mode:04o}", f.label);
        }

        // A world-readable runtime config (it holds the bot token) is
        // repaired, not trusted.
        let runtime_cfg = dir.path().join("runtime/config.json");
        std::fs::set_permissions(&runtime_cfg, std::fs::Permissions::from_mode(0o644)).unwrap();
        apply(&plan, true).expect("re-apply");
        let mode = std::fs::metadata(&runtime_cfg).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // ---- verify ---------------------------------------------------------

    #[test]
    fn verify_is_clean_immediately_after_a_migration() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        apply(&plan, false).expect("apply");
        assert_eq!(verify(&plan), Vec::<String>::new());
    }

    #[test]
    fn verify_catches_a_hand_edit_and_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(FIXTURE, dir.path());
        apply(&plan, false).expect("apply");
        std::fs::write(dir.path().join("config/prompts/help.md"), "edited\n").unwrap();
        std::fs::remove_file(dir.path().join("config/commands/ship.toml")).unwrap();

        let issues = verify(&plan);
        assert!(issues
            .iter()
            .any(|i| i.contains("help.md") && i.contains("differ")));
        assert!(issues
            .iter()
            .any(|i| i.contains("ship.toml") && i.contains("missing")));
    }

    /// The check none of the `bridge show` diffs can do: a legacy key that
    /// reached nothing at all.
    #[test]
    fn verify_reports_a_legacy_key_that_reached_no_destination() {
        let dir = tempfile::tempdir().unwrap();
        let text = FIXTURE.replacen(
            r#""defaultTarget": "gcp","#,
            r#""defaultTarget": "gcp", "webhookUrl": "https://example.invalid/hook","#,
            1,
        );
        let plan = plan_of(&text, dir.path());
        assert!(plan.unmapped.contains(&"webhookUrl".to_string()));
        apply(&plan, false).expect("apply");
        assert!(verify(&plan)
            .iter()
            .any(|i| i.contains("webhookUrl") && i.contains("no destination")));
    }

    #[test]
    fn a_fully_mapped_config_has_no_unmapped_keys() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(plan_of(FIXTURE, dir.path()).unmapped, Vec::<String>::new());
        assert_eq!(plan_of(FIXTURE_MAX, dir.path()).unmapped, Vec::<String>::new());
    }
}
