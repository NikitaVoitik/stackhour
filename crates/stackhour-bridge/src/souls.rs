//! Named-agent runtime (NEW).
//!
//! Resolves the active AgentDef from state+registry, composes the system
//! prompt via the PromptStore 'system' template (soul text hot-reloaded from
//! disk on every composition + skill bodies under '## Skills' + optional
//! standing instructions), and maps agent settings onto the engine's
//! declared flags. Engines WITHOUT system_prompt_args get the composed text
//! prepended to the user prompt with a separator. When no agent is active:
//! no system prompt composed, plain session keys — behaviour byte-identical
//! to today.

use crate::engines::RunRequest;
use crate::state::BridgeState;
use stackhour_core::registry::{AgentDef, EngineDef, Registry};
use stackhour_core::Result;

/// Compose the full system prompt for an agent (soul + skills via the
/// 'system' template: `{{soul}}\n\n## Skills\n{{skills}}`).
pub fn compose_system_prompt(agent: &AgentDef, reg: &Registry) -> Result<String> {
    let _ = (agent, reg);
    todo!()
}

/// Apply an (optional) agent's model / permission_mode / ToolPolicy / system
/// prompt onto a run request for the given engine.
pub fn apply_agent(def: &EngineDef, agent: Option<&AgentDef>, reg: &Registry, req: &mut RunRequest) {
    let _ = (def, agent, reg, req);
    todo!()
}

/// The active AgentDef for the current state, when any (and still valid in
/// the registry).
pub fn agent_for<'r>(state: &BridgeState, reg: &'r Registry) -> Option<&'r AgentDef> {
    let _ = (state, reg);
    todo!()
}
