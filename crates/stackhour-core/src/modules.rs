//! The module gate: which optional feature modules are compiled into this
//! binary and which are
//! enabled by the user's `modules` config block.
//!
//! DELIBERATE DIVERGENCE (no Node original): the Node CLI has no notion of
//! modules. The whole file is new surface. The two rules that keep it
//! byte-compatible with today are (a) `modules` is ABSENT from the DEFAULTS
//! table, so an absent key is indistinguishable from today, and (b) an
//! absent, null, or malformed block resolves to "everything enabled" —
//! this gate fails OPEN, always.
//!
//! NOT to be confused with `crate::registry`, the config-DIRECTORY registry
//! (engines/agents/skills/commands/prompts). Different thing entirely.

use crate::jsnum::js_truthy;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One optional feature module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Module {
    Tracker,
    Agent,
    Bridge,
    Control,
}

impl Module {
    /// Iteration order for every user-visible list.
    pub const ALL: [Module; 4] = [Module::Tracker, Module::Agent, Module::Bridge, Module::Control];

    /// The config sub-key AND the Cargo feature name — deliberately the
    /// same string, so one identifier names both layers.
    pub fn name(self) -> &'static str {
        match self {
            Module::Tracker => "tracker",
            Module::Agent => "agent",
            Module::Bridge => "bridge",
            Module::Control => "control",
        }
    }

    /// The dotted config key an error message must name.
    pub fn config_key(self) -> &'static str {
        match self {
            Module::Tracker => "modules.tracker",
            Module::Agent => "modules.agent",
            Module::Bridge => "modules.bridge",
            Module::Control => "modules.control",
        }
    }
}

/// Which modules are on. Used for BOTH the compile-time set and the runtime
/// set — same shape, different provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleSet {
    pub tracker: bool,
    pub agent: bool,
    pub bridge: bool,
    pub control: bool,
}

impl ModuleSet {
    /// The fail-open value: an absent or malformed `modules` block, an
    /// unreadable config file, and a default build all resolve to this.
    pub const ALL: ModuleSet = ModuleSet {
        tracker: true,
        agent: true,
        bridge: true,
        control: true,
    };

    pub const fn new(tracker: bool, agent: bool, bridge: bool) -> Self {
        ModuleSet {
            tracker,
            agent,
            bridge,
            control: true,
        }
    }

    pub const fn with_control(mut self, control: bool) -> Self {
        self.control = control;
        self
    }

    pub fn contains(&self, m: Module) -> bool {
        match m {
            Module::Tracker => self.tracker,
            Module::Agent => self.agent,
            Module::Bridge => self.bridge,
            Module::Control => self.control,
        }
    }

    /// The off modules, in `Module::ALL` order.
    pub fn disabled(&self) -> Vec<Module> {
        Module::ALL.into_iter().filter(|m| !self.contains(*m)).collect()
    }
}

impl Default for ModuleSet {
    fn default() -> Self {
        ModuleSet::ALL
    }
}

/// Read `modules` out of a MERGED config value (`Config.raw`).
///
/// Rules, in order:
///   * `raw["modules"]` missing, null, or any non-object -> everything on
///     (`Value::as_object` returns None for every non-object, so arrays,
///     scalars and `null` all fall out through one branch);
///   * a sub-key that is missing or null -> that module is ON;
///   * any other present sub-value -> `js_truthy(v)`.
///
/// Only an explicitly present, JS-falsy value (`false`, `0`, `""`) turns a
/// module off. Note the JS-parity consequence: `"bridge": "false"` is a
/// non-empty string and therefore ENABLES bridge.
pub fn from_raw(raw: &Value) -> ModuleSet {
    let Some(block) = raw.get("modules").and_then(Value::as_object) else {
        // Missing, null, scalar, or array: fail OPEN.
        return ModuleSet::ALL;
    };
    // PARITY: `js_truthy` is the workspace's JS `Boolean(v)`. The coercion is
    // applied here deliberately, so `"modules": {"bridge": "false"}` ENABLES
    // bridge — a non-empty string is truthy in JS and every other config
    // toggle in this workspace reads the same way. No warning is emitted.
    let on = |m: Module| match block.get(m.name()) {
        None | Some(Value::Null) => true,
        Some(v) => js_truthy(v),
    };
    ModuleSet::new(on(Module::Tracker), on(Module::Agent), on(Module::Bridge))
        .with_control(on(Module::Control))
}

/// Lenient read of the USER config file for the dispatch gate.
///
/// NEVER fails and never allocates an Error: a missing file, an unreadable
/// file, or malformed JSON all resolve to `ModuleSet::ALL`. This is what lets
/// the gate run before `load_config` without changing the corrupt-config
/// ordering contract — a broken config still fails inside the verb with
/// today's message and exit 1. Reading the user file rather than the merged
/// config is exact, because `modules` is absent from DEFAULTS.
pub fn from_config_file(path: &Path) -> ModuleSet {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ModuleSet::ALL;
    };
    let Ok(user) = serde_json::from_str::<Value>(&text) else {
        return ModuleSet::ALL;
    };
    from_raw(&user)
}

/// Verb -> owning module. `sub` is `tail.first()`.
///
///   serve, status, token, data, backup, import-wakatime -> Tracker
///   agent                                               -> Agent
///   bridge (every sub-verb, including none)             -> Bridge
///   init|install with sub "server"                      -> Tracker
///   init|install with sub "agent"                       -> Agent
///   everything else (doctor, "", unknown verbs,
///     init/install with a bad or missing role)          -> None
///
/// `None` means NEVER gated: doctor is the diagnostic of last resort, and an
/// unknown verb must keep falling through to the exit-0 usage banner.
///
/// This answers "which module does the INVOCATION belong to", which is the
/// only question the dispatch gate asks. It is deliberately NOT the same as
/// "which modules does the invocation touch": `install server` installs two
/// services owned by two different modules. See `service_roles_for`.
pub fn module_for(verb: &str, sub: Option<&str>) -> Option<Module> {
    match verb {
        "serve" | "status" | "token" | "data" | "backup" | "import-wakatime" => Some(Module::Tracker),
        "agent" => Some(Module::Agent),
        "bridge" => Some(Module::Bridge),
        "control" => Some(Module::Control),
        "init" | "install" => match sub {
            Some("server") => Some(Module::Tracker),
            Some("agent") => Some(Module::Agent),
            // A missing or unknown role is NOT gated: the verb owns that
            // error, and today it is `usage: stackhour init <server|agent>`
            // with exit 1.
            _ => None,
        },
        _ => None,
    }
}

/// The service roles that `install <role>` — and `init <role> --install` —
/// actually installs and STARTS, in install order, each paired with the module
/// that owns the resulting unit. An unknown role has no roles at all.
///
/// This is the second question the registry has to answer and the one
/// `module_for`'s `-> Option<Module>` signature structurally cannot express:
/// `install server` installs BOTH stackhour-server and stackhour-agent, so the
/// module the invocation belongs to (Tracker) is not the whole story. Gating
/// the verb on Tracker alone would let `install server` start a
/// stackhour-agent unit whose every start the gate refuses — and the unit is
/// `Restart=always` / `RestartSec=10`, so that is an endless crash loop,
/// reported by the install command as success.
///
/// Deliberately NOT folded into `module_for` (i.e. `install server` is not
/// gated on Agent as well): refusing the whole command on a tracker-only box
/// would block the very install the operator wants. The verb stays gated on
/// the module it belongs to; this table decides which UNITS may be started.
/// Anyone adding a fourth module adds its roles here, in one place.
pub fn service_roles_for(role: &str) -> &'static [(&'static str, Module)] {
    match role {
        "server" => &[("server", Module::Tracker), ("agent", Module::Agent)],
        "agent" => &[("agent", Module::Agent)],
        _ => &[],
    }
}

/// What the user typed, for the message: "serve", "init server", "bridge".
/// The role is included only for the two role-dependent verbs.
pub fn invocation_label(verb: &str, sub: Option<&str>) -> String {
    match (verb, sub) {
        ("init" | "install", Some(role @ ("server" | "agent"))) => format!("{verb} {role}"),
        _ => verb.to_string(),
    }
}

/// The gate outcome. The compile-time layer is checked FIRST: recompiling is
/// the only remedy, so a module that is neither compiled nor enabled must
/// report the compile-time message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Allowed,
    NotCompiled(Module),
    Disabled(Module),
}

pub fn gate(verb: &str, sub: Option<&str>, compiled: ModuleSet, runtime: ModuleSet) -> Gate {
    let Some(m) = module_for(verb, sub) else {
        return Gate::Allowed;
    };
    if !compiled.contains(m) {
        return Gate::NotCompiled(m);
    }
    if !runtime.contains(m) {
        return Gate::Disabled(m);
    }
    Gate::Allowed
}

/// The exit code for a gated verb. It CANNOT travel through
/// `stackhour_core::Error` (which has no exit-code channel and whose
/// `deferred()` path always yields 1), so the caller must return
/// `ExitCode::from(GATED_EXIT_CODE)` directly.
///
/// Collision note: `bridge migrate` already uses 2 for "destinations exist",
/// `bridge return` for a missing job id, and `bridge tg-send` for an
/// unreadable bridge config. The CODE alone is ambiguous; the MESSAGES below
/// are deliberately disjoint from all three.
pub const GATED_EXIT_CODE: u8 = 2;

impl Gate {
    /// The single-line stderr message, or `None` for `Allowed`.
    pub fn message(&self, invocation: &str, config_path: &Path) -> Option<String> {
        match self {
            Gate::Allowed => None,
            Gate::Disabled(m) => Some(format!(
                "stackhour: {invocation} needs the {} module, which is disabled by \"{}\": false in {}",
                m.name(),
                m.config_key(),
                config_path.display(),
            )),
            Gate::NotCompiled(m) => Some(format!(
                "stackhour: {invocation} needs the {} module, which was not compiled into this binary (rebuild with --features {})",
                m.name(),
                m.name(),
            )),
        }
    }

    /// The STDOUT line for a service unit that was SKIPPED rather than
    /// refused. `install server` installs two units and only one of them may
    /// be off, so the command still succeeds — the wording says "Skipped",
    /// never "needs", so nobody greps this as a failure or confuses it with
    /// the exit-2 refusal above.
    pub fn skip_line(&self, unit: &str, config_path: &Path) -> Option<String> {
        match self {
            Gate::Allowed => None,
            Gate::Disabled(m) => Some(format!(
                "Skipped {unit}: the {} module is disabled by \"{}\": false in {}",
                m.name(),
                m.config_key(),
                config_path.display(),
            )),
            Gate::NotCompiled(m) => Some(format!(
                "Skipped {unit}: the {} module was not compiled into this binary (rebuild with --features {})",
                m.name(),
                m.name(),
            )),
        }
    }
}

/// Everything one gate decision needs: the compile-time set (Layer 1), the
/// runtime set (Layer 2), and the config file the runtime set was read from,
/// so a message can name it.
///
/// Bundled rather than passed as three arguments because both layers and the
/// path travel together through dispatch AND through the install path, and a
/// caller that forgets one of them silently gates on the wrong layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateContext {
    pub compiled: ModuleSet,
    pub runtime: ModuleSet,
    pub config_path: PathBuf,
}

impl GateContext {
    /// Everything on. What a machine with no config.json resolves to, and
    /// what tests that are not exercising the gate should pass. The path is
    /// empty because nothing can be refused, so nothing can name it.
    pub fn all_enabled() -> Self {
        GateContext {
            compiled: ModuleSet::ALL,
            runtime: ModuleSet::ALL,
            config_path: PathBuf::new(),
        }
    }

    /// Resolve the runtime half from `config_path`, leniently (a missing,
    /// unreadable, or corrupt file yields `ModuleSet::ALL`).
    pub fn from_config_file(compiled: ModuleSet, config_path: PathBuf) -> Self {
        let runtime = from_config_file(&config_path);
        GateContext {
            compiled,
            runtime,
            config_path,
        }
    }

    pub fn gate(&self, verb: &str, sub: Option<&str>) -> Gate {
        gate(verb, sub, self.compiled, self.runtime)
    }

    /// The single stderr line for a refused invocation, or `None` when it is
    /// allowed.
    pub fn refusal(&self, verb: &str, sub: Option<&str>) -> Option<String> {
        self.gate(verb, sub)
            .message(&invocation_label(verb, sub), &self.config_path)
    }

    /// Is `m` usable — compiled in AND switched on?
    pub fn allows(&self, m: Module) -> bool {
        self.compiled.contains(m) && self.runtime.contains(m)
    }

    /// Why a service role owned by `m` cannot be installed, as a stdout
    /// "Skipped <unit>: ..." line. `None` when the role is fine to install.
    pub fn skip_line_for(&self, m: Module, unit: &str) -> Option<String> {
        let outcome = if !self.compiled.contains(m) {
            Gate::NotCompiled(m)
        } else if !self.runtime.contains(m) {
            Gate::Disabled(m)
        } else {
            Gate::Allowed
        };
        outcome.skip_line(unit, &self.config_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_missing_modules_block_enables_everything() {
        assert_eq!(from_raw(&json!({})), ModuleSet::ALL);
        assert_eq!(from_raw(&json!({ "server": { "port": 4040 } })), ModuleSet::ALL);
        assert_eq!(ModuleSet::default(), ModuleSet::ALL);
    }

    #[test]
    fn a_null_or_scalar_or_array_modules_value_enables_everything() {
        assert_eq!(from_raw(&json!({ "modules": null })), ModuleSet::ALL);
        assert_eq!(from_raw(&json!({ "modules": 3 })), ModuleSet::ALL);
        assert_eq!(from_raw(&json!({ "modules": "bridge" })), ModuleSet::ALL);
        assert_eq!(from_raw(&json!({ "modules": false })), ModuleSet::ALL);
        assert_eq!(from_raw(&json!({ "modules": [] })), ModuleSet::ALL);
        assert_eq!(
            from_raw(&json!({ "modules": ["tracker", "agent"] })),
            ModuleSet::ALL
        );
    }

    #[test]
    fn an_explicit_false_disables_only_that_module() {
        let set = from_raw(&json!({ "modules": { "bridge": false } }));
        assert_eq!(set, ModuleSet::new(true, true, false));
        assert!(set.contains(Module::Tracker));
        assert!(!set.contains(Module::Bridge));
        assert_eq!(set.disabled(), vec![Module::Bridge]);

        let set = from_raw(&json!({ "modules": { "tracker": false, "agent": false } }));
        assert_eq!(set, ModuleSet::new(false, false, true));
        assert_eq!(set.disabled(), vec![Module::Tracker, Module::Agent]);
    }

    #[test]
    fn a_null_sub_value_leaves_the_module_enabled() {
        let set = from_raw(&json!({ "modules": { "tracker": null, "bridge": false } }));
        assert_eq!(set, ModuleSet::new(true, true, false));
    }

    #[test]
    fn a_present_sub_value_goes_through_js_truthiness() {
        let enabled = |v: serde_json::Value| from_raw(&json!({ "modules": { "bridge": v } })).bridge;
        // Falsy in JS -> module OFF.
        assert!(!enabled(json!(0)));
        assert!(!enabled(json!("")));
        assert!(!enabled(json!(false)));
        // Truthy in JS -> module ON. `"false"` is a NON-EMPTY STRING.
        assert!(enabled(json!("false")));
        assert!(enabled(json!([])));
        assert!(enabled(json!({})));
        assert!(enabled(json!("no")));
        assert!(enabled(json!(1)));
        assert!(enabled(json!(true)));
    }

    #[test]
    fn an_unknown_modules_sub_key_is_ignored() {
        let set = from_raw(&json!({ "modules": { "frobnicate": false, "tracker": false } }));
        assert_eq!(set, ModuleSet::new(false, true, true));
    }

    #[test]
    fn the_verb_table_maps_every_advertised_verb() {
        for verb in ["serve", "status", "token", "data", "backup", "import-wakatime"] {
            assert_eq!(module_for(verb, None), Some(Module::Tracker), "{verb}");
        }
        assert_eq!(module_for("agent", None), Some(Module::Agent));
        assert_eq!(module_for("agent", Some("--once")), Some(Module::Agent));
        for sub in [
            None,
            Some("install"),
            Some("migrate"),
            Some("status"),
            Some("tg-send"),
        ] {
            assert_eq!(module_for("bridge", sub), Some(Module::Bridge), "{sub:?}");
        }
        assert_eq!(module_for("control", Some("hub")), Some(Module::Control));
        assert_eq!(module_for("control", Some("node")), Some(Module::Control));
    }

    #[test]
    fn init_and_install_are_mapped_by_their_role() {
        assert_eq!(module_for("init", Some("server")), Some(Module::Tracker));
        assert_eq!(module_for("init", Some("agent")), Some(Module::Agent));
        assert_eq!(module_for("install", Some("server")), Some(Module::Tracker));
        assert_eq!(module_for("install", Some("agent")), Some(Module::Agent));
        assert_eq!(invocation_label("init", Some("server")), "init server");
        assert_eq!(invocation_label("install", Some("agent")), "install agent");
        assert_eq!(invocation_label("serve", None), "serve");
        assert_eq!(invocation_label("bridge", Some("status")), "bridge");
    }

    /// The regression this table exists for: `install server` starts a
    /// stackhour-agent unit too, so the AGENT module has to be consulted even
    /// though the invocation belongs to the tracker.
    #[test]
    fn install_server_touches_the_agent_module_as_well_as_the_tracker() {
        assert_eq!(
            service_roles_for("server"),
            &[("server", Module::Tracker), ("agent", Module::Agent)]
        );
        assert_eq!(service_roles_for("agent"), &[("agent", Module::Agent)]);
        assert!(service_roles_for("").is_empty());
        assert!(service_roles_for("frobnicate").is_empty());
    }

    /// Whatever `module_for` gates an `install <role>` on must be one of the
    /// roles that install actually starts — otherwise the verb is gated on a
    /// module it never touches. Pins the two tables together for module four.
    #[test]
    fn every_gated_install_role_is_a_role_that_install_actually_starts() {
        for role in ["server", "agent"] {
            let owner = module_for("install", Some(role)).expect("a known role is gated");
            let roles = service_roles_for(role);
            assert!(
                roles.iter().any(|(_, m)| *m == owner),
                "install {role} is gated on {owner:?} but starts {roles:?}"
            );
            assert_eq!(module_for("init", Some(role)), Some(owner));
        }
    }

    #[test]
    fn doctor_and_unknown_and_roleless_verbs_are_never_gated() {
        assert_eq!(module_for("doctor", None), None);
        assert_eq!(module_for("doctor", Some("--json")), None);
        assert_eq!(module_for("", None), None);
        assert_eq!(module_for("frobnicate", None), None);
        assert_eq!(module_for("init", None), None);
        assert_eq!(module_for("install", Some("frobnicate")), None);
        // And the gate agrees, even with everything off.
        let none = ModuleSet::new(false, false, false);
        for (verb, sub) in [
            ("doctor", None),
            ("", None),
            ("frobnicate", None),
            ("init", None),
            ("install", Some("frobnicate")),
        ] {
            assert_eq!(gate(verb, sub, none, none), Gate::Allowed, "{verb}");
        }
    }

    #[test]
    fn the_compile_time_layer_is_reported_before_the_runtime_layer() {
        let compiled = ModuleSet::new(true, true, false);
        let runtime = ModuleSet::new(true, true, false);
        assert_eq!(
            gate("bridge", Some("status"), compiled, runtime),
            Gate::NotCompiled(Module::Bridge)
        );
        // Compiled in but switched off at runtime is the OTHER message.
        assert_eq!(
            gate("bridge", Some("status"), ModuleSet::ALL, runtime),
            Gate::Disabled(Module::Bridge)
        );
        assert_eq!(gate("serve", None, ModuleSet::ALL, ModuleSet::ALL), Gate::Allowed);
    }

    fn sandbox() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn a_missing_config_file_enables_everything() {
        let dir = sandbox();
        assert_eq!(from_config_file(&dir.path().join("nope.json")), ModuleSet::ALL);
    }

    #[test]
    fn a_corrupt_config_file_enables_everything() {
        let dir = sandbox();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ not json at all").unwrap();
        assert_eq!(from_config_file(&path), ModuleSet::ALL);
    }

    #[test]
    fn an_unreadable_config_file_enables_everything() {
        let dir = sandbox();
        let path = dir.path().join("config.json");
        std::fs::create_dir(&path).unwrap();
        assert_eq!(from_config_file(&path), ModuleSet::ALL);
    }

    #[test]
    fn a_config_file_modules_block_is_read_leniently_but_exactly() {
        let dir = sandbox();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"modules":{"tracker":false}}"#).unwrap();
        assert_eq!(from_config_file(&path), ModuleSet::new(false, true, true));
    }

    /// `config_key()` is a second hand-written string table restating
    /// `name()` under a `modules.` prefix. The exhaustive match makes a
    /// MISSING arm a compile error; nothing but this catches a typo'd one,
    /// and a typo'd key would ship both an exit-2 message and a `module-*`
    /// doctor line pointing an operator at a config key that does nothing.
    #[test]
    fn every_config_key_is_the_module_name_under_modules() {
        for m in Module::ALL {
            assert_eq!(m.config_key(), format!("modules.{}", m.name()));
        }
    }

    #[test]
    fn the_disabled_message_names_the_config_key_and_the_file() {
        let path = Path::new("/home/nikita/.config/stackhour/config.json");
        let msg = Gate::Disabled(Module::Tracker)
            .message("serve", path)
            .expect("a disabled gate has a message");
        assert_eq!(
            msg,
            "stackhour: serve needs the tracker module, which is disabled by \"modules.tracker\": false in /home/nikita/.config/stackhour/config.json"
        );
        assert!(!msg.contains('\n'));
        assert_eq!(Gate::Allowed.message("serve", path), None);
    }

    #[test]
    fn the_not_compiled_message_names_the_cargo_feature() {
        let path = Path::new("/home/nikita/.config/stackhour/config.json");
        let msg = Gate::NotCompiled(Module::Bridge)
            .message("bridge", path)
            .expect("an uncompiled gate has a message");
        assert_eq!(
            msg,
            "stackhour: bridge needs the bridge module, which was not compiled into this binary (rebuild with --features bridge)"
        );
        assert!(!msg.contains('\n'));
        // The compile-time layer never mentions the config file.
        assert!(!msg.contains("config.json"));
    }

    #[test]
    fn neither_gate_message_collides_with_an_existing_exit_two_message() {
        let path = Path::new("/tmp/config.json");
        for gate in [Gate::Disabled(Module::Tracker), Gate::NotCompiled(Module::Bridge)] {
            let msg = gate.message("serve", path).unwrap();
            assert!(!msg.contains("not yet implemented"), "{msg}");
            assert!(!msg.starts_with("tg-send: "), "{msg}");
            // Both are greppable by one pattern...
            assert!(msg.contains("needs the "), "{msg}");
            assert!(msg.contains(" module, which "), "{msg}");
        }
        // ...and only the compile-time one carries the Layer-1 marker.
        assert!(!Gate::Disabled(Module::Tracker)
            .message("serve", path)
            .unwrap()
            .contains("not compiled into this binary"));
        assert!(Gate::NotCompiled(Module::Bridge)
            .message("bridge", path)
            .unwrap()
            .contains("not compiled into this binary"));
        assert_eq!(GATED_EXIT_CODE, 2);
    }

    #[test]
    fn a_skip_line_reads_as_a_skip_and_never_as_a_refusal() {
        let path = Path::new("/home/nikita/.config/stackhour/config.json");
        assert_eq!(
            Gate::Disabled(Module::Agent)
                .skip_line("stackhour-agent", path)
                .unwrap(),
            "Skipped stackhour-agent: the agent module is disabled by \"modules.agent\": false in /home/nikita/.config/stackhour/config.json"
        );
        assert_eq!(
            Gate::NotCompiled(Module::Agent)
                .skip_line("stackhour-agent", path)
                .unwrap(),
            "Skipped stackhour-agent: the agent module was not compiled into this binary (rebuild with --features agent)"
        );
        assert_eq!(Gate::Allowed.skip_line("stackhour-agent", path), None);
        // Never the exit-2 refusal wording, and never multi-line.
        for gate in [Gate::Disabled(Module::Agent), Gate::NotCompiled(Module::Agent)] {
            let line = gate.skip_line("stackhour-agent", path).unwrap();
            assert!(!line.contains("needs the "), "{line}");
            assert!(!line.starts_with("stackhour: "), "{line}");
            assert!(!line.contains('\n'), "{line}");
        }
    }

    #[test]
    fn a_gate_context_reports_both_layers_and_prefers_the_compile_time_one() {
        let path = PathBuf::from("/tmp/config.json");
        let ctx = GateContext {
            compiled: ModuleSet::ALL,
            runtime: ModuleSet::new(true, false, true),
            config_path: path.clone(),
        };
        assert!(!ctx.allows(Module::Agent));
        assert!(ctx.allows(Module::Tracker));
        assert_eq!(ctx.gate("agent", None), Gate::Disabled(Module::Agent));
        assert_eq!(
            ctx.refusal("agent", None).unwrap(),
            "stackhour: agent needs the agent module, which is disabled by \"modules.agent\": false in /tmp/config.json"
        );
        assert_eq!(ctx.refusal("serve", None), None);
        assert!(ctx
            .skip_line_for(Module::Agent, "stackhour-agent")
            .unwrap()
            .contains("is disabled by"));
        assert_eq!(ctx.skip_line_for(Module::Tracker, "stackhour-server"), None);

        // Not compiled beats switched-off, in the skip line as in the refusal.
        let ctx = GateContext {
            compiled: ModuleSet::new(true, false, true),
            runtime: ModuleSet::new(true, false, true),
            config_path: path,
        };
        assert_eq!(ctx.gate("agent", None), Gate::NotCompiled(Module::Agent));
        assert!(ctx
            .skip_line_for(Module::Agent, "stackhour-agent")
            .unwrap()
            .contains("not compiled into this binary"));
    }

    #[test]
    fn an_all_enabled_gate_context_refuses_and_skips_nothing() {
        let ctx = GateContext::all_enabled();
        for m in Module::ALL {
            assert!(ctx.allows(m));
            assert_eq!(ctx.skip_line_for(m, "stackhour-x"), None);
        }
        for (verb, sub) in [
            ("serve", None),
            ("agent", None),
            ("bridge", Some("status")),
            ("control", Some("hub")),
        ] {
            assert_eq!(ctx.gate(verb, sub), Gate::Allowed);
            assert_eq!(ctx.refusal(verb, sub), None);
        }
    }

    #[test]
    fn a_gate_context_reads_its_runtime_half_leniently_from_the_config_file() {
        let dir = sandbox();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"modules":{"agent":false}}"#).unwrap();
        let ctx = GateContext::from_config_file(ModuleSet::ALL, path.clone());
        assert_eq!(ctx.runtime, ModuleSet::new(true, false, true));
        assert_eq!(ctx.config_path, path);

        let missing = dir.path().join("nope.json");
        let ctx = GateContext::from_config_file(ModuleSet::ALL, missing);
        assert_eq!(ctx.runtime, ModuleSet::ALL);
    }
}
