//! Compile-time and runtime gates for Stackhour's optional products.
//!
//! `modules` is absent from the default configuration. A missing, malformed,
//! or unreadable block therefore fails open and enables every compiled module.

use crate::jsnum::js_truthy;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Module {
    Tracker,
    Agent,
    Control,
}

impl Module {
    pub const ALL: [Module; 3] = [Module::Tracker, Module::Agent, Module::Control];

    pub fn name(self) -> &'static str {
        match self {
            Module::Tracker => "tracker",
            Module::Agent => "agent",
            Module::Control => "control",
        }
    }

    pub fn config_key(self) -> &'static str {
        match self {
            Module::Tracker => "modules.tracker",
            Module::Agent => "modules.agent",
            Module::Control => "modules.control",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleSet {
    pub tracker: bool,
    pub agent: bool,
    pub control: bool,
}

impl ModuleSet {
    pub const ALL: ModuleSet = ModuleSet {
        tracker: true,
        agent: true,
        control: true,
    };

    pub const fn new(tracker: bool, agent: bool, control: bool) -> Self {
        Self {
            tracker,
            agent,
            control,
        }
    }

    pub fn contains(&self, module: Module) -> bool {
        match module {
            Module::Tracker => self.tracker,
            Module::Agent => self.agent,
            Module::Control => self.control,
        }
    }

    pub fn disabled(&self) -> Vec<Module> {
        Module::ALL
            .into_iter()
            .filter(|module| !self.contains(*module))
            .collect()
    }
}

impl Default for ModuleSet {
    fn default() -> Self {
        Self::ALL
    }
}

pub fn from_raw(raw: &Value) -> ModuleSet {
    let Some(block) = raw.get("modules").and_then(Value::as_object) else {
        return ModuleSet::ALL;
    };
    let enabled = |module: Module| match block.get(module.name()) {
        None | Some(Value::Null) => true,
        Some(value) => js_truthy(value),
    };
    ModuleSet::new(
        enabled(Module::Tracker),
        enabled(Module::Agent),
        enabled(Module::Control),
    )
}

pub fn from_config_file(path: &Path) -> ModuleSet {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ModuleSet::ALL;
    };
    let Ok(user) = serde_json::from_str::<Value>(&text) else {
        return ModuleSet::ALL;
    };
    from_raw(&user)
}

pub fn module_for(verb: &str, sub: Option<&str>) -> Option<Module> {
    match verb {
        "serve" | "status" | "token" | "data" | "backup" | "migrate" | "import-wakatime" => {
            Some(Module::Tracker)
        }
        "agent" => Some(Module::Agent),
        "control" => Some(Module::Control),
        "init" | "install" => match sub {
            Some("server") => Some(Module::Tracker),
            Some("agent") => Some(Module::Agent),
            _ => None,
        },
        _ => None,
    }
}

pub fn service_roles_for(role: &str) -> &'static [(&'static str, Module)] {
    match role {
        "server" => &[("server", Module::Tracker), ("agent", Module::Agent)],
        "agent" => &[("agent", Module::Agent)],
        _ => &[],
    }
}

pub fn invocation_label(verb: &str, sub: Option<&str>) -> String {
    match (verb, sub) {
        ("init" | "install", Some(role @ ("server" | "agent"))) | ("migrate", Some(role @ "tempo")) => {
            format!("{verb} {role}")
        }
        _ => verb.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Allowed,
    NotCompiled(Module),
    Disabled(Module),
}

pub fn gate(verb: &str, sub: Option<&str>, compiled: ModuleSet, runtime: ModuleSet) -> Gate {
    let Some(module) = module_for(verb, sub) else {
        return Gate::Allowed;
    };
    if !compiled.contains(module) {
        return Gate::NotCompiled(module);
    }
    if !runtime.contains(module) {
        return Gate::Disabled(module);
    }
    Gate::Allowed
}

pub const GATED_EXIT_CODE: u8 = 2;

impl Gate {
    pub fn message(&self, invocation: &str, config_path: &Path) -> Option<String> {
        match self {
            Gate::Allowed => None,
            Gate::Disabled(module) => Some(format!(
                "stackhour: {invocation} needs the {} module, which is disabled by \"{}\": false in {}",
                module.name(),
                module.config_key(),
                config_path.display(),
            )),
            Gate::NotCompiled(module) => Some(format!(
                "stackhour: {invocation} needs the {} module, which was not compiled into this binary (rebuild with --features {})",
                module.name(),
                module.name(),
            )),
        }
    }

    pub fn skip_line(&self, unit: &str, config_path: &Path) -> Option<String> {
        match self {
            Gate::Allowed => None,
            Gate::Disabled(module) => Some(format!(
                "Skipped {unit}: the {} module is disabled by \"{}\": false in {}",
                module.name(),
                module.config_key(),
                config_path.display(),
            )),
            Gate::NotCompiled(module) => Some(format!(
                "Skipped {unit}: the {} module was not compiled into this binary (rebuild with --features {})",
                module.name(),
                module.name(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateContext {
    pub compiled: ModuleSet,
    pub runtime: ModuleSet,
    pub config_path: PathBuf,
}

impl GateContext {
    pub fn all_enabled() -> Self {
        Self {
            compiled: ModuleSet::ALL,
            runtime: ModuleSet::ALL,
            config_path: PathBuf::new(),
        }
    }

    pub fn from_config_file(compiled: ModuleSet, config_path: PathBuf) -> Self {
        Self {
            compiled,
            runtime: from_config_file(&config_path),
            config_path,
        }
    }

    pub fn gate(&self, verb: &str, sub: Option<&str>) -> Gate {
        gate(verb, sub, self.compiled, self.runtime)
    }

    pub fn refusal(&self, verb: &str, sub: Option<&str>) -> Option<String> {
        self.gate(verb, sub)
            .message(&invocation_label(verb, sub), &self.config_path)
    }

    pub fn allows(&self, module: Module) -> bool {
        self.compiled.contains(module) && self.runtime.contains(module)
    }

    pub fn skip_line_for(&self, module: Module, unit: &str) -> Option<String> {
        let outcome = if !self.compiled.contains(module) {
            Gate::NotCompiled(module)
        } else if !self.runtime.contains(module) {
            Gate::Disabled(module)
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
    fn missing_or_malformed_modules_enable_everything() {
        for raw in [
            json!({}),
            json!({"modules": null}),
            json!({"modules": false}),
            json!({"modules": []}),
        ] {
            assert_eq!(from_raw(&raw), ModuleSet::ALL);
        }
    }

    #[test]
    fn explicit_falsy_values_disable_only_named_modules() {
        assert_eq!(
            from_raw(&json!({"modules": {"tracker": false, "control": 0}})),
            ModuleSet::new(false, true, false)
        );
        assert_eq!(
            from_raw(&json!({"modules": {"agent": ""}})),
            ModuleSet::new(true, false, true)
        );
    }

    #[test]
    fn unknown_module_keys_are_ignored() {
        assert_eq!(
            from_raw(&json!({"modules": {"removed-product": false}})),
            ModuleSet::ALL
        );
    }

    #[test]
    fn verb_ownership_is_complete() {
        assert_eq!(module_for("serve", None), Some(Module::Tracker));
        assert_eq!(module_for("agent", Some("--once")), Some(Module::Agent));
        assert_eq!(module_for("control", Some("hub")), Some(Module::Control));
        assert_eq!(module_for("doctor", None), None);
        assert_eq!(module_for("unknown", None), None);
    }

    #[test]
    fn install_server_consults_tracker_and_agent() {
        assert_eq!(
            service_roles_for("server"),
            &[("server", Module::Tracker), ("agent", Module::Agent)]
        );
        assert_eq!(service_roles_for("agent"), &[("agent", Module::Agent)]);
    }

    #[test]
    fn compile_time_refusal_precedes_runtime_refusal() {
        let compiled = ModuleSet::new(true, true, false);
        let runtime = ModuleSet::new(true, true, false);
        assert_eq!(
            gate("control", Some("hub"), compiled, runtime),
            Gate::NotCompiled(Module::Control)
        );
        assert_eq!(
            gate("control", Some("hub"), ModuleSet::ALL, runtime),
            Gate::Disabled(Module::Control)
        );
    }

    #[test]
    fn lenient_file_read_fails_open() {
        let directory = tempfile::TempDir::new().unwrap();
        let missing = directory.path().join("missing.json");
        assert_eq!(from_config_file(&missing), ModuleSet::ALL);
        let corrupt = directory.path().join("corrupt.json");
        std::fs::write(&corrupt, "{").unwrap();
        assert_eq!(from_config_file(&corrupt), ModuleSet::ALL);
    }

    #[test]
    fn every_config_key_matches_the_module_name() {
        for module in Module::ALL {
            assert_eq!(module.config_key(), format!("modules.{}", module.name()));
        }
    }

    #[test]
    fn gate_messages_name_the_remedy() {
        let path = Path::new("/tmp/config.json");
        assert_eq!(
            Gate::Disabled(Module::Agent).message("agent", path).unwrap(),
            "stackhour: agent needs the agent module, which is disabled by \"modules.agent\": false in /tmp/config.json"
        );
        assert_eq!(
            Gate::NotCompiled(Module::Control)
                .message("control", path)
                .unwrap(),
            "stackhour: control needs the control module, which was not compiled into this binary (rebuild with --features control)"
        );
    }

    #[test]
    fn gate_context_checks_both_layers() {
        let context = GateContext {
            compiled: ModuleSet::ALL,
            runtime: ModuleSet::new(true, false, true),
            config_path: PathBuf::from("/tmp/config.json"),
        };
        assert!(!context.allows(Module::Agent));
        assert!(context.allows(Module::Control));
        assert!(context.refusal("agent", None).is_some());
        assert!(context.refusal("control", Some("hub")).is_none());
    }
}
