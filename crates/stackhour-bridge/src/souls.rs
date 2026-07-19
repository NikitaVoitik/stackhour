//! Named-agent runtime.
//!
//! Resolves the active [`AgentDef`] from state + registry, composes the system
//! prompt (the agent's soul chain, hot-reloaded from disk on every
//! composition, plus its skill bodies under `## Skills`), and maps the agent's
//! settings onto a [`RunRequest`] through the engine's DECLARED flag
//! templates — never through hardcoded per-engine knowledge.
//!
//! ## The backward-compatibility contract
//!
//! When no agent is active — which is the state of every user who has not
//! written an `agents/` directory — [`apply_agent`] returns without touching
//! the request: no system prompt, no model override, no permission-mode
//! override, no cwd override. The run is byte-identical to today's bridge.
//! Every test in this module that ends in `_is_untouched_without_an_agent`
//! exists to keep it that way.
//!
//! ## Precedence
//!
//! An agent can be selected from four places. Highest wins:
//!
//!   skill > command > conversation (`/agent <name>`) > configured default
//!
//! See [`resolve_agent`].
//!
//! ## Soft ignore vs hard refusal
//!
//! Both [`AgentDef::effort`] and [`AgentDef::tools`] are delivered through
//! flag templates the ENGINE declares, so an engine can simply not support
//! them. The two cases are NOT symmetric:
//!
//! * `effort` on an engine with no `effort_args` is silently ignored. The run
//!   is merely less tuned, and engines are swappable.
//! * `[tools]` on an engine with no `allowed_tools_args` /
//!   `disallowed_tools_args` is a REFUSAL
//!   ([`EngineDef::unenforceable_policy`]). Dropping a `deny` list hands the
//!   agent back a tool the user explicitly removed, and nothing downstream
//!   would ever surface that.

use crate::engines::RunRequest;
use crate::state::BridgeState;
use stackhour_core::registry::agent_def::composed_soul;
use stackhour_core::registry::{skill, AgentDef, EngineDef, Registry, SkillDef, ToolPolicy};
use stackhour_core::Result;
use std::path::PathBuf;

/// The template rendered to combine a soul with its skills.
const SYSTEM_TEMPLATE: &str = "system";
/// The template used to deliver a system prompt to engines that take no
/// system-prompt flag.
const AGENT_TURN_TEMPLATE: &str = "agent-turn";

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

/// Compose the full system prompt for an agent: the composed soul chain plus
/// the agent's skill bodies, rendered through the `system` template
/// (`{{soul}}\n\n## Skills\n{{skills}}`).
///
/// Two shortcuts keep the output clean, because a half-empty template renders
/// as visible junk in a real prompt:
///
/// * no skill bodies -> just the soul, with no dangling `## Skills` heading;
/// * no soul and no skills -> the empty string, which [`apply_agent`] treats
///   as "no system prompt at all".
///
/// Every document is read through its own mtime cache, so calling this per
/// turn is a handful of `stat`s and a hand edit to any soul, overlay or
/// skill body is live on the very next message.
pub fn compose_system_prompt(agent: &AgentDef, reg: &Registry) -> Result<String> {
    let soul = composed_soul(agent, reg)?;
    let skills = compose_skills(agent, reg)?;

    if skills.trim().is_empty() {
        return Ok(soul.trim().to_string());
    }
    Ok(reg
        .prompts
        .render(
            SYSTEM_TEMPLATE,
            &[("soul", soul.trim()), ("skills", skills.trim())],
        )
        .trim()
        .to_string())
}

/// The agent's skill bodies, `uses`-expanded and de-duplicated ACROSS skills,
/// in declaration order, joined by a blank line.
///
/// Deduplication has to span the whole agent, not each skill: two skills that
/// both `uses = ["house-style"]` must not paste the house style twice.
/// A skill the registry does not know is skipped — the loader already dropped
/// the agent or reported the dangling reference with a better message.
fn compose_skills(agent: &AgentDef, reg: &Registry) -> Result<String> {
    let mut ordered: Vec<String> = Vec::new();
    for name in &agent.skills {
        if !reg.skills.contains_key(name) {
            continue;
        }
        for step in skill::composition_order(reg, name).map_err(stackhour_core::Error::msg)? {
            if !ordered.contains(&step) {
                ordered.push(step);
            }
        }
    }

    let mut parts: Vec<String> = Vec::new();
    for name in &ordered {
        let Some(def) = reg.skills.get(name) else {
            continue;
        };
        let body = def.body()?;
        if !body.trim().is_empty() {
            parts.push(body.trim_end().to_string());
        }
    }
    Ok(parts.join("\n\n"))
}

// ---------------------------------------------------------------------------
// Applying an agent to a run
// ---------------------------------------------------------------------------

/// Apply an (optional) agent's model / permission mode / cwd / prompt template
/// / composed system prompt onto a run request for `def`.
///
/// `agent = None` is a no-op: see the module docs.
///
/// System-prompt delivery follows the ENGINE's declaration, not the engine's
/// name: an engine with `system_prompt_args` gets the text in
/// `req.system_prompt` (spliced into argv by `build_argv`); an engine without
/// one gets it prepended to the user prompt via the `agent-turn` template.
///
/// A soul that cannot be READ (an I/O error, not a missing file — a missing
/// file is an empty soul) degrades to "no system prompt" rather than failing
/// the turn. The user's message still gets answered; the registry error
/// surface is where broken configuration is reported.
pub fn apply_agent(def: &EngineDef, agent: Option<&AgentDef>, reg: &Registry, req: &mut RunRequest) {
    let Some(agent) = agent else {
        return;
    };

    if let Some(model) = &agent.model {
        req.model = Some(model.clone());
    }
    // An agent always has a permission mode (defaulted at parse time), and an
    // active agent is a deliberate choice, so it wins over the target config.
    req.permission_mode = Some(agent.permission_mode.clone());
    if let Some(cwd) = &agent.cwd {
        req.cwd = Some(expand_tilde(cwd));
    }
    // Only for engines that DECLARE `effort_args`; otherwise the agent's
    // effort is silently ignored rather than breaking the run.
    if let Some(effort) = agent_effort(def, Some(agent)) {
        req.effort = Some(effort.to_string());
    }

    // The effective `[tools]` policy, spliced through the engine's declared
    // flags. Without this the policy parsed, validated, inherited and merged
    // cleanly — and then never reached the child, so `deny = ["Bash"]` was a
    // config that reported no error while the agent kept full tool access.
    let policy = tool_policy(Some(agent), reg);
    req.allow_tools = policy.allow;
    req.deny_tools = policy.deny;

    // Per-agent prompt wrapper, by template NAME. Rendering an unknown
    // template yields "", which would silently eat the user's message, so a
    // miss leaves the prompt alone (the loader has already reported it).
    if let Some(template) = &agent.prompt_template {
        if reg.prompts.has(template) {
            let wrapped = reg.prompts.render(template, &[("prompt", &req.prompt)]);
            if !wrapped.trim().is_empty() {
                req.prompt = wrapped;
            }
        }
    }

    let system = compose_system_prompt(agent, reg).unwrap_or_default();
    if system.trim().is_empty() {
        return;
    }
    if def.system_prompt_args.is_some() {
        req.system_prompt = Some(system);
    } else {
        req.prompt = reg.prompts.render(
            AGENT_TURN_TEMPLATE,
            &[("system", &system), ("prompt", &req.prompt)],
        );
    }
}

/// The reasoning effort to splice through the engine's `effort_args`.
/// `None` for engines that declare none, so a soft ignore is the failure mode
/// (engines are swappable; an agent should not break when moved to one that
/// has no such flag).
pub fn agent_effort<'a>(def: &EngineDef, agent: Option<&'a AgentDef>) -> Option<&'a str> {
    def.effort_args.as_ref()?;
    agent.and_then(|a| a.effort.as_deref())
}

/// The effective tool policy for a run: the agent's own `[tools]` unioned with
/// those of every skill it composes, with `deny` beating `allow`.
///
/// A skill exists to grant a capability, so its allows are additive; but a
/// deny anywhere in the composition is a veto — the strict reading is the
/// only safe one when the alternative is silently handing a tool back.
pub fn tool_policy(agent: Option<&AgentDef>, reg: &Registry) -> ToolPolicy {
    let Some(agent) = agent else {
        return ToolPolicy::default();
    };
    let mut policy = agent.tools.clone();
    for name in &agent.skills {
        let Some(def) = reg.skills.get(name) else {
            continue;
        };
        // `inheriting_from` is exactly the union-with-deny-winning rule.
        policy = policy.inheriting_from(&skill_tools(def));
    }
    policy
}

fn skill_tools(def: &SkillDef) -> ToolPolicy {
    def.tools.clone()
}

/// Expand a leading `~` against `$HOME`. Left verbatim when `$HOME` is unset,
/// which is a broken environment the spawn will report far better than we can.
fn expand_tilde(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    let Ok(home) = std::env::var("HOME") else {
        return PathBuf::from(path);
    };
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        PathBuf::from(home)
    } else {
        PathBuf::from(home).join(rest)
    }
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// The active agent for the current conversation: the one `/agent <name>`
/// selected, else the configured default (config.json `bridge.defaultAgent`
/// or `STACKHOUR_AGENT`).
///
/// A name that no longer resolves — the agent was deleted or dropped for a
/// validation error since it was selected — falls back to the default rather
/// than pinning the conversation to a ghost.
pub fn agent_for<'r>(state: &BridgeState, reg: &'r Registry) -> Option<&'r AgentDef> {
    state
        .agent
        .as_deref()
        .and_then(|name| reg.agents.get(name))
        .or_else(|| {
            reg.defaults
                .agent
                .as_deref()
                .and_then(|name| reg.agents.get(name))
        })
}

/// Full precedence resolution for one turn: skill > command > conversation >
/// configured default.
///
/// The skill wins because it is the most specific thing the user asked for:
/// `/review` invoking a skill bound to the `reviewer` agent should review,
/// whatever the conversation happens to be set to.
pub fn resolve_agent<'r>(
    state: &BridgeState,
    reg: &'r Registry,
    command_agent: Option<&str>,
    skill_agent: Option<&str>,
) -> Option<&'r AgentDef> {
    for candidate in [skill_agent, command_agent] {
        if let Some(def) = candidate.and_then(|name| reg.agents.get(name)) {
            return Some(def);
        }
    }
    agent_for(state, reg)
}

/// Handle `/agent <name>`: select an agent for this conversation, or clear the
/// selection with `none` / `off` / an empty argument.
///
/// Returns the reply text on success, or the error text to send on failure —
/// both already HTML-safe. Mutates `state` only on success.
pub fn select_agent(
    state: &mut BridgeState,
    reg: &Registry,
    arg: &str,
) -> std::result::Result<String, String> {
    let name = arg.trim();
    if name.is_empty() || name.eq_ignore_ascii_case("none") || name.eq_ignore_ascii_case("off") {
        state.agent = None;
        return Ok("Agent cleared. Prompts run with no soul.".to_string());
    }
    match reg.agents.get(name) {
        Some(def) => {
            state.agent = Some(def.name.clone());
            Ok(format!(
                "Agent: <b>{}</b> ({} on {})",
                esc(&def.label),
                esc(&def.name),
                esc(&def.engine)
            ))
        }
        None => Err(format!(
            "Unknown agent <b>{}</b>.\n\n{}",
            esc(name),
            agent_list_text(reg)
        )),
    }
}

/// The `/agents` reply: every known agent with its label and engine, in
/// registry order (which is sorted, so the list is stable between reloads).
pub fn agent_list_text(reg: &Registry) -> String {
    if reg.agents.is_empty() {
        return "No agents are configured. Add one at <code>agents/&lt;name&gt;/agent.toml</code>."
            .to_string();
    }
    let mut out = String::from("<b>Agents</b>");
    for def in reg.agents.values() {
        out.push_str(&format!(
            "\n• <b>{}</b> — {} ({})",
            esc(&def.name),
            esc(&def.label),
            esc(&def.engine)
        ));
    }
    if let Some(default) = reg.defaults.agent.as_deref() {
        out.push_str(&format!("\n\nDefault: <b>{}</b>", esc(default)));
    }
    out
}

/// HTML escape for Telegram text nodes — the ONE shared implementation.
///
/// This was a local 5-entity copy, written while `render::esc` was assumed to
/// be unimplemented. It was not, and the extra two entities were a
/// divergence: coordinator.mjs escapes `&`, `<` and `>` only, and Telegram's
/// HTML parse mode does not require quote escaping outside attributes. The
/// local copy therefore rendered a literal `&quot;` to the user wherever a
/// soul label contained a quote.
use crate::render::esc;

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use serde_json::json;
    use stackhour_core::registry;
    use std::fs;
    use std::path::Path;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().expect("has parent")).unwrap();
        fs::write(path, body).unwrap();
    }

    /// A registry loaded from a real on-disk config tree, so these tests
    /// exercise the same path the daemon does.
    fn load(root: &Path) -> Registry {
        registry::load_with(root, registry::EnvSource::fixed(&[]))
    }

    /// The zero-config registry: no config dir at all.
    fn bare_registry() -> Registry {
        load(Path::new("/nonexistent-stackhour-config"))
    }

    fn state(agent: Option<&str>) -> BridgeState {
        BridgeState {
            offset: 0,
            active: "gcp".into(),
            engine: "claude".into(),
            agent: agent.map(str::to_string),
            sessions: IndexMap::new(),
            raw: json!({}),
        }
    }

    fn req(prompt: &str) -> RunRequest {
        RunRequest {
            prompt: prompt.to_string(),
            ..RunRequest::default()
        }
    }

    fn engine(reg: &Registry, name: &str) -> EngineDef {
        reg.engines.get(name).expect("built-in engine").clone()
    }

    /// A config tree with one agent, one soul and (optionally) skills.
    fn config_with_reviewer(root: &Path) {
        write(
            &root.join("agents/reviewer/agent.toml"),
            "label = \"Reviewer\"\nengine = \"claude\"\n",
        );
        write(&root.join("agents/reviewer/soul.md"), "Lead with the verdict.\n");
    }

    // ---- backward compatibility: no agent, nothing changes ----

    #[test]
    fn a_bare_config_dir_defines_no_agents_and_no_default() {
        let reg = bare_registry();
        assert!(reg.agents.is_empty());
        assert_eq!(reg.defaults.agent, None);
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
    }

    #[test]
    fn the_run_request_is_untouched_without_an_agent() {
        let reg = bare_registry();
        let claude = engine(&reg, "claude");
        let mut r = req("hello");
        let before = r.clone();
        apply_agent(&claude, None, &reg, &mut r);
        assert_eq!(r.prompt, before.prompt);
        assert_eq!(r.system_prompt, None);
        assert_eq!(r.model, None);
        assert_eq!(r.permission_mode, None);
        assert_eq!(r.cwd, None);
    }

    #[test]
    fn agent_for_is_none_without_configuration() {
        let reg = bare_registry();
        assert!(agent_for(&state(None), &reg).is_none());
        // Even a stale selection resolves to nothing rather than panicking.
        assert!(agent_for(&state(Some("ghost")), &reg).is_none());
    }

    #[test]
    fn an_agent_with_an_empty_soul_still_composes_no_system_prompt() {
        // The agent exists but its soul.md was never written: the turn must
        // stay identical to a no-agent turn rather than shipping a stub.
        let dir = tmpdir();
        write(
            &dir.path().join("agents/blank/agent.toml"),
            "engine = \"claude\"\n",
        );
        let reg = load(dir.path());
        let agent = reg.agents.get("blank").expect("loaded");
        assert_eq!(compose_system_prompt(agent, &reg).unwrap(), "");

        let claude = engine(&reg, "claude");
        let mut r = req("hello");
        apply_agent(&claude, Some(agent), &reg, &mut r);
        assert_eq!(r.prompt, "hello");
        assert_eq!(r.system_prompt, None);
    }

    // ---- a valid custom agent takes effect ----

    #[test]
    fn a_custom_agent_loads_and_its_soul_becomes_the_system_prompt() {
        let dir = tmpdir();
        config_with_reviewer(dir.path());
        let reg = load(dir.path());
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);

        let agent = reg.agents.get("reviewer").expect("loaded");
        assert_eq!(agent.label, "Reviewer");
        assert_eq!(
            compose_system_prompt(agent, &reg).unwrap(),
            "Lead with the verdict."
        );
    }

    #[test]
    fn skills_are_appended_under_a_skills_heading() {
        let dir = tmpdir();
        config_with_reviewer(dir.path());
        write(
            &dir.path().join("agents/reviewer/agent.toml"),
            "engine = \"claude\"\nskills = [\"review\"]\n",
        );
        write(
            &dir.path().join("skills/review/skill.toml"),
            "description = \"Review code\"\n",
        );
        write(
            &dir.path().join("skills/review/skill.md"),
            "Quote code, don't describe it.\n",
        );

        let reg = load(dir.path());
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        let agent = reg.agents.get("reviewer").expect("loaded");
        assert_eq!(
            compose_system_prompt(agent, &reg).unwrap(),
            "Lead with the verdict.\n\n## Skills\nQuote code, don't describe it."
        );
    }

    #[test]
    fn a_skill_shared_by_two_skills_is_included_once() {
        let dir = tmpdir();
        write(
            &dir.path().join("agents/a/agent.toml"),
            "engine = \"claude\"\nskills = [\"one\", \"two\"]\n",
        );
        write(&dir.path().join("agents/a/soul.md"), "SOUL");
        for (name, uses) in [("one", "\nuses = [\"shared\"]"), ("two", "\nuses = [\"shared\"]")] {
            write(
                &dir.path().join(format!("skills/{name}/skill.toml")),
                &format!("description = \"{name}\"{uses}\n"),
            );
            write(&dir.path().join(format!("skills/{name}/skill.md")), name);
        }
        write(
            &dir.path().join("skills/shared/skill.toml"),
            "description = \"shared\"\n",
        );
        write(&dir.path().join("skills/shared/skill.md"), "SHARED");

        let reg = load(dir.path());
        let agent = reg.agents.get("a").expect("loaded");
        let system = compose_system_prompt(agent, &reg).unwrap();
        assert_eq!(system.matches("SHARED").count(), 1, "got: {system}");
        // Dependency order: the shared fragment precedes both dependents.
        assert!(system.find("SHARED") < system.find("one"), "got: {system}");
    }

    #[test]
    fn a_soul_edit_is_live_without_a_registry_reload() {
        let dir = tmpdir();
        config_with_reviewer(dir.path());
        let reg = load(dir.path());
        let agent = reg.agents.get("reviewer").expect("loaded");
        assert_eq!(
            compose_system_prompt(agent, &reg).unwrap(),
            "Lead with the verdict."
        );

        let soul = dir.path().join("agents/reviewer/soul.md");
        fs::write(&soul, "Totally new instructions.\n").unwrap();
        let f = fs::File::options().write(true).open(&soul).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .unwrap();
        drop(f);

        // Same Registry, same AgentDef, no reload.
        assert_eq!(
            compose_system_prompt(agent, &reg).unwrap(),
            "Totally new instructions."
        );
    }

    // ---- delivery onto RunRequest ----

    #[test]
    fn an_engine_with_system_prompt_args_gets_the_text_in_the_request_field() {
        let dir = tmpdir();
        config_with_reviewer(dir.path());
        let reg = load(dir.path());
        let claude = engine(&reg, "claude");
        assert!(claude.system_prompt_args.is_some(), "fixture assumption");

        let mut r = req("look at this");
        apply_agent(&claude, reg.agents.get("reviewer"), &reg, &mut r);
        assert_eq!(r.system_prompt.as_deref(), Some("Lead with the verdict."));
        // The user prompt is NOT rewritten when the engine takes a flag.
        assert_eq!(r.prompt, "look at this");
    }

    #[test]
    fn an_engine_without_system_prompt_args_gets_the_text_prepended_to_the_prompt() {
        let dir = tmpdir();
        config_with_reviewer(dir.path());
        write(
            &dir.path().join("agents/reviewer/agent.toml"),
            "engine = \"codex\"\n",
        );
        let reg = load(dir.path());
        let codex = engine(&reg, "codex");
        assert!(codex.system_prompt_args.is_none(), "fixture assumption");

        let mut r = req("look at this");
        apply_agent(&codex, reg.agents.get("reviewer"), &reg, &mut r);
        assert_eq!(r.system_prompt, None);
        assert_eq!(r.prompt, "Lead with the verdict.\n\n---\n\nlook at this");
    }

    #[test]
    fn model_permission_mode_and_cwd_are_mapped_onto_the_request() {
        let dir = tmpdir();
        write(
            &dir.path().join("agents/deep/agent.toml"),
            "engine = \"claude\"\nmodel = \"claude-opus-4\"\npermission_mode = \"bypassPermissions\"\ncwd = \"~/work/repo\"\n",
        );
        let reg = load(dir.path());
        let claude = engine(&reg, "claude");
        let mut r = req("go");
        apply_agent(&claude, reg.agents.get("deep"), &reg, &mut r);

        assert_eq!(r.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(r.permission_mode.as_deref(), Some("bypassPermissions"));
        let home = std::env::var("HOME").expect("HOME set in test env");
        assert_eq!(r.cwd, Some(PathBuf::from(home).join("work/repo")));
    }

    #[test]
    fn an_agent_prompt_template_wraps_the_user_prompt() {
        let dir = tmpdir();
        write(
            &dir.path().join("agents/wrap/agent.toml"),
            "engine = \"claude\"\nprompt_template = \"wrapper\"\n",
        );
        write(&dir.path().join("prompts/wrapper.md"), "TASK: {{prompt}}");
        let reg = load(dir.path());
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);

        let claude = engine(&reg, "claude");
        let mut r = req("ship it");
        apply_agent(&claude, reg.agents.get("wrap"), &reg, &mut r);
        assert_eq!(r.prompt, "TASK: ship it");
    }

    #[test]
    fn effort_reaches_engines_that_declare_effort_args_and_no_others() {
        let dir = tmpdir();
        write(
            &dir.path().join("agents/deep/agent.toml"),
            "engine = \"thinky\"\neffort = \"high\"\n",
        );
        write(
            &dir.path().join("engines/thinky.toml"),
            "bin = \"thinky\"\nkind = \"plain-lines\"\neffort_args = [\"--effort\", \"{{effort}}\"]\n",
        );
        let reg = load(dir.path());
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);
        let agent = reg.agents.get("deep");

        assert_eq!(agent_effort(&engine(&reg, "thinky"), agent), Some("high"));
        // claude declares no effort_args: the agent's effort is ignored, not
        // an error, so an agent stays portable between engines.
        assert_eq!(agent_effort(&engine(&reg, "claude"), agent), None);
    }

    #[test]
    fn tool_policy_unions_the_agent_with_its_skills_and_deny_wins() {
        let dir = tmpdir();
        write(
            &dir.path().join("agents/a/agent.toml"),
            "engine = \"claude\"\nskills = [\"web\"]\n[tools]\nallow = [\"Bash\"]\ndeny = [\"WebSearch\"]\n",
        );
        write(
            &dir.path().join("skills/web/skill.toml"),
            "description = \"web\"\n[tools]\nallow = [\"WebSearch\", \"WebFetch\"]\n",
        );
        let reg = load(dir.path());
        let policy = tool_policy(reg.agents.get("a"), &reg);
        // The skill's WebFetch is granted; its WebSearch loses to the agent's
        // deny.
        assert!(policy.allow.contains(&"WebFetch".to_string()));
        assert!(policy.allow.contains(&"Bash".to_string()));
        assert!(!policy.allow.contains(&"WebSearch".to_string()));
        assert!(policy.deny.contains(&"WebSearch".to_string()));
        assert_eq!(tool_policy(None, &reg), ToolPolicy::default());
    }

    // ---- an invalid agent produces the right error ----

    #[test]
    fn an_unknown_engine_drops_the_agent_and_names_the_file_key_and_alternatives() {
        let dir = tmpdir();
        write(
            &dir.path().join("agents/broken/agent.toml"),
            "engine = \"gpt5\"\n",
        );
        let reg = load(dir.path());

        assert!(!reg.agents.contains_key("broken"));
        assert_eq!(reg.errors.len(), 1, "{:?}", reg.errors);
        let err = &reg.errors[0];
        assert_eq!(err.name, "broken");
        assert_eq!(err.file, Some(dir.path().join("agents/broken/agent.toml")));
        assert!(
            err.message
                .starts_with("key `engine`: references unknown engine 'gpt5' (known: "),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn a_malformed_agent_manifest_is_skipped_not_fatal() {
        let dir = tmpdir();
        config_with_reviewer(dir.path());
        write(
            &dir.path().join("agents/broken/agent.toml"),
            "permission_mode = \"yolo\"\nengine = \"claude\"\n",
        );
        let reg = load(dir.path());

        // The good agent still loads: one bad file never takes the rest down.
        assert!(reg.agents.contains_key("reviewer"));
        assert!(!reg.agents.contains_key("broken"));
        assert_eq!(
            reg.errors[0].message,
            "key `permission_mode`: must be one of \"default\", \"bypassPermissions\" (got 'yolo')"
        );
    }

    #[test]
    fn an_unknown_skill_reference_names_the_key_and_the_known_skills() {
        let dir = tmpdir();
        config_with_reviewer(dir.path());
        write(
            &dir.path().join("agents/reviewer/agent.toml"),
            "engine = \"claude\"\nskills = [\"ghost\"]\n",
        );
        let reg = load(dir.path());
        assert!(!reg.agents.contains_key("reviewer"));
        assert!(
            reg.errors[0]
                .message
                .contains("key `skills`: references unknown skill 'ghost'"),
            "got: {}",
            reg.errors[0].message
        );
    }

    #[test]
    fn the_shipped_starter_tree_composes_base_overlay_and_skill() {
        // The example config is documentation users copy, so it has to work:
        // materialise it exactly as `bridge init` does and compose a turn.
        let dir = tmpdir();
        stackhour_core::registry::defaults::materialize(dir.path()).expect("materialize");
        let reg = load(dir.path());
        assert!(reg.errors.is_empty(), "{:?}", reg.errors);

        let agent = reg.agents.get("reviewer").expect("shipped agent");
        let system = compose_system_prompt(agent, &reg).unwrap();

        // base soul (via extends, root-first) ...
        let base = system.find("Assume the reader is walking").expect("base soul");
        // ... then reviewer's own soul ...
        let own = system.find("Lead with the verdict").expect("reviewer soul");
        // ... then its overlay ...
        let overlay = system.find("Hard cap: 200 words").expect("overlay");
        // ... then the skill bodies.
        let skills = system.find("## Skills").expect("skills heading");
        assert!(base < own && own < overlay && overlay < skills, "got:\n{system}");
    }

    // ---- selection ----

    #[test]
    fn the_conversation_selection_wins_over_the_configured_default() {
        let dir = tmpdir();
        config_with_reviewer(dir.path());
        write(
            &dir.path().join("agents/other/agent.toml"),
            "engine = \"claude\"\n",
        );
        write(
            &dir.path().join("config.json"),
            "{\"bridge\": {\"defaultAgent\": \"other\"}}",
        );
        let reg = load(dir.path());
        assert_eq!(reg.defaults.agent.as_deref(), Some("other"));

        assert_eq!(
            agent_for(&state(None), &reg).map(|a| a.name.as_str()),
            Some("other")
        );
        assert_eq!(
            agent_for(&state(Some("reviewer")), &reg).map(|a| a.name.as_str()),
            Some("reviewer")
        );
        // A selection that no longer resolves falls back to the default.
        assert_eq!(
            agent_for(&state(Some("deleted")), &reg).map(|a| a.name.as_str()),
            Some("other")
        );
    }

    #[test]
    fn precedence_is_skill_then_command_then_conversation_then_default() {
        let dir = tmpdir();
        for name in ["fromskill", "fromcmd", "fromchat", "fromdefault"] {
            write(
                &dir.path().join(format!("agents/{name}/agent.toml")),
                "engine = \"claude\"\n",
            );
        }
        write(
            &dir.path().join("config.json"),
            "{\"bridge\": {\"defaultAgent\": \"fromdefault\"}}",
        );
        let reg = load(dir.path());
        let st = state(Some("fromchat"));
        let pick = |cmd, skill| {
            resolve_agent(&st, &reg, cmd, skill)
                .map(|a| a.name.clone())
                .unwrap_or_default()
        };

        assert_eq!(pick(Some("fromcmd"), Some("fromskill")), "fromskill");
        assert_eq!(pick(Some("fromcmd"), None), "fromcmd");
        assert_eq!(pick(None, None), "fromchat");
        assert_eq!(
            resolve_agent(&state(None), &reg, None, None)
                .map(|a| a.name.clone())
                .unwrap_or_default(),
            "fromdefault"
        );
        // An unknown per-command agent falls through instead of blanking the
        // turn (the loader already dropped the command that named it).
        assert_eq!(pick(Some("ghost"), None), "fromchat");
    }

    #[test]
    fn select_agent_sets_clears_and_rejects() {
        let dir = tmpdir();
        config_with_reviewer(dir.path());
        let reg = load(dir.path());
        let mut st = state(None);

        let ok = select_agent(&mut st, &reg, "reviewer").expect("known agent");
        assert_eq!(st.agent.as_deref(), Some("reviewer"));
        assert!(ok.contains("<b>Reviewer</b>"), "got: {ok}");

        let err = select_agent(&mut st, &reg, "ghost").unwrap_err();
        assert!(err.contains("Unknown agent <b>ghost</b>"), "got: {err}");
        // A rejected selection must not clobber the working one.
        assert_eq!(st.agent.as_deref(), Some("reviewer"));

        for clearing in ["none", "OFF", "  "] {
            st.agent = Some("reviewer".into());
            assert!(select_agent(&mut st, &reg, clearing).is_ok());
            assert_eq!(st.agent, None, "'{clearing}' should clear");
        }
    }

    #[test]
    fn agent_list_text_lists_agents_and_marks_the_default() {
        let dir = tmpdir();
        config_with_reviewer(dir.path());
        write(
            &dir.path().join("agents/other/agent.toml"),
            "engine = \"codex\"\n",
        );
        write(
            &dir.path().join("config.json"),
            "{\"bridge\": {\"defaultAgent\": \"other\"}}",
        );
        let reg = load(dir.path());
        let text = agent_list_text(&reg);
        assert!(
            text.contains("• <b>reviewer</b> — Reviewer (claude)"),
            "got: {text}"
        );
        assert!(text.contains("• <b>other</b> — other (codex)"), "got: {text}");
        assert!(text.contains("Default: <b>other</b>"), "got: {text}");
    }

    #[test]
    fn agent_list_text_says_so_when_there_are_none() {
        let text = agent_list_text(&bare_registry());
        assert!(text.starts_with("No agents are configured"), "got: {text}");
    }

    #[test]
    fn agent_names_are_html_escaped_in_replies() {
        // Names come from directory names, which a user controls.
        let dir = tmpdir();
        write(
            &dir.path().join("agents/a/agent.toml"),
            "label = \"<b>x</b>\"\nengine = \"claude\"\n",
        );
        let reg = load(dir.path());
        assert!(agent_list_text(&reg).contains("&lt;b&gt;x&lt;/b&gt;"));

        let mut st = state(None);
        let err = select_agent(&mut st, &reg, "<script>").unwrap_err();
        assert!(err.contains("&lt;script&gt;"), "got: {err}");
    }

    #[test]
    fn tilde_expansion_handles_bare_home_and_non_tilde_paths() {
        let home = std::env::var("HOME").expect("HOME set in test env");
        assert_eq!(expand_tilde("~"), PathBuf::from(&home));
        assert_eq!(expand_tilde("~/x"), PathBuf::from(&home).join("x"));
        assert_eq!(expand_tilde("/abs/x"), PathBuf::from("/abs/x"));
        assert_eq!(expand_tilde("rel/x"), PathBuf::from("rel/x"));
    }
}
