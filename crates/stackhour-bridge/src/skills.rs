//! Skill invocation runtime.
//!
//! [`SkillDef`] is the *declaration* (see `stackhour_core::registry::skill`);
//! this module is the *execution*. It turns "the user typed `/review 4821`"
//! into a fully-bound [`InvocationPlan`]: the system-prompt fragments to hand
//! the agent pillar, the user prompt for the turn, the merged tool policy and
//! env, the agent to switch to, and the pre/post hooks with every placeholder
//! already substituted.
//!
//! A skill can be invoked from three places and all three land here:
//!
//! - a command with `kind = "skill"` (`plan_invocation`),
//! - another skill's `uses` list (handled inside composition),
//! - an agent's `skills` list (the agent pillar calls `fragments_for` to get
//!   the same bodies without binding any arguments).
//!
//! The whole module is deliberately side-effect-free EXCEPT for
//! [`run_pre_hooks`] / [`run_post_hooks`], so planning is testable without
//! spawning anything.
//!
//! DESIGN NOTE — hooks are FIXED argv. `pre`/`post` are a `Vec<String>` that
//! goes straight to `Command::new(argv[0]).args(&argv[1..])`. There is no
//! shell, so `{{arg:note}}` expanding to `; rm -rf /` is one harmless
//! argument. Substitution happens per element and never re-splits.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use stackhour_core::registry::args::bind_args;
use stackhour_core::registry::skill::{composition_order, unknown_skill_message};
use stackhour_core::registry::{Registry, ToolPolicy};

/// How long to wait between `try_wait` polls while a hook runs. Short enough
/// that a fast hook is not noticeably delayed, long enough not to spin.
const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// One constituent of a skill's system-prompt contribution.
///
/// The agent pillar owns how these are laid out under `## Skills`; this module
/// only guarantees the ORDER (dependency-first, deduped) and the content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFragment {
    pub name: String,
    pub description: String,
    /// The skill's own markdown body (never the composed one — composition is
    /// expressed by the fragment LIST, so the caller can render headings).
    pub body: String,
}

/// A hook, resolved: argv with every placeholder substituted, ready to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHooks {
    pub pre: Vec<String>,
    pub post: Vec<String>,
    pub timeout: Duration,
    /// Working directory for the hooks (the agent's cwd when it has one).
    pub cwd: Option<PathBuf>,
    /// Env applied to hooks, on top of the inherited environment.
    pub env: IndexMap<String, String>,
}

impl ResolvedHooks {
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }
}

/// Everything the coordinator needs to run one skill invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationPlan {
    /// The invoked skill (the LAST fragment; the earlier ones are its `uses`).
    pub skill: String,
    /// Dependency-first, deduped. Hand to the agent pillar for the system
    /// prompt; do NOT compose it here — `souls.rs` owns that.
    pub system_fragments: Vec<SkillFragment>,
    /// The turn's user prompt: the skill's `template` rendered with the bound
    /// arguments, or the raw argument string when no template is declared.
    pub user_prompt: String,
    /// Union of every constituent skill's tool policy (deny wins).
    pub tools: ToolPolicy,
    /// Merged env; a nearer skill overrides a further one.
    pub env: IndexMap<String, String>,
    /// The agent this skill wants to run under, if any. The nearest `agent`
    /// declaration wins (the invoked skill beats what it `uses`).
    pub agent_override: Option<String>,
    /// Bound arguments in declaration order, plus `args` (the raw string).
    pub args: IndexMap<String, String>,
    pub hooks: ResolvedHooks,
}

/// Resolve a skill invocation into a fully-bound plan.
///
/// `raw_args` is the untouched text the user typed after the command word.
/// With no `[[args]]` spec that string is used verbatim (the legacy path);
/// with a spec it is bound positionally and a binding failure is returned as
/// a chat-ready message naming the offending argument.
///
/// Errors are user-facing strings — they go straight into a Telegram reply —
/// and always name the skill.
pub fn plan_invocation(reg: &Registry, skill: &str, raw_args: &str) -> Result<InvocationPlan, String> {
    let def = reg
        .skills
        .get(skill)
        .ok_or_else(|| unknown_skill_message(reg, skill))?;

    // Dependency-first, deduped, cycle- and depth-guarded.
    let order = composition_order(reg, skill)?;

    // Arguments bind against the INVOKED skill's spec only. A composed skill
    // contributes prose and policy, not arity: otherwise adding `uses` to a
    // skill would silently change how the user has to type the command.
    let args = bind_args(&def.args, raw_args).map_err(|e| format!("/{skill}: {}", e.message()))?;

    let mut fragments: Vec<SkillFragment> = Vec::with_capacity(order.len());
    let mut tools = ToolPolicy::default();
    let mut env: IndexMap<String, String> = IndexMap::new();
    let mut agent_override: Option<String> = None;

    for name in &order {
        let Some(part) = reg.skills.get(name) else {
            continue;
        };
        let body = part
            .body()
            .map_err(|e| format!("skill '{name}': cannot read body: {e}"))?;
        if !body.trim().is_empty() {
            fragments.push(SkillFragment {
                name: part.name.clone(),
                description: part.description.clone(),
                body: body.trim_end().to_string(),
            });
        }
        // `order` is dependency-first, so later writes are nearer the invoked
        // skill and correctly win.
        merge_tools(&mut tools, &part.tools);
        for (k, v) in &part.env {
            env.insert(k.clone(), v.clone());
        }
        if let Some(a) = &part.agent {
            agent_override = Some(a.clone());
        }
    }

    let user_prompt = render_user_prompt(reg, skill, &args)?;
    let cwd = agent_cwd(reg, agent_override.as_deref());
    let hooks = ResolvedHooks {
        pre: substitute_argv(&def.hooks.pre, skill, &args, agent_override.as_deref(), &cwd),
        post: substitute_argv(&def.hooks.post, skill, &args, agent_override.as_deref(), &cwd),
        timeout: Duration::from_secs(def.hooks.timeout_seconds),
        cwd,
        env: env.clone(),
    };

    Ok(InvocationPlan {
        skill: skill.to_string(),
        system_fragments: fragments,
        user_prompt,
        tools,
        env,
        agent_override,
        args,
        hooks,
    })
}

/// The system-prompt fragments contributed by a skill WITHOUT binding any
/// arguments — what an agent's `skills = [...]` list needs.
///
/// Kept separate from [`plan_invocation`] because an agent listing a skill is
/// not invoking it: there are no arguments to bind, no prompt to render and no
/// hooks to run, so a skill with a required argument must not error here.
pub fn fragments_for(reg: &Registry, skill: &str) -> Result<Vec<SkillFragment>, String> {
    let order = composition_order(reg, skill)?;
    let mut out = Vec::with_capacity(order.len());
    for name in &order {
        let Some(part) = reg.skills.get(name) else {
            continue;
        };
        let body = part
            .body()
            .map_err(|e| format!("skill '{name}': cannot read body: {e}"))?;
        if body.trim().is_empty() {
            continue;
        }
        out.push(SkillFragment {
            name: part.name.clone(),
            description: part.description.clone(),
            body: body.trim_end().to_string(),
        });
    }
    Ok(out)
}

/// Run the plan's `pre` hook. A non-zero exit ABORTS the turn; the returned
/// string is chat-ready and carries the hook's stderr (trimmed) so the user
/// can see why.
pub fn run_pre_hooks(plan: &InvocationPlan) -> Result<(), String> {
    if plan.hooks.pre.is_empty() {
        return Ok(());
    }
    match run_hook(plan, &plan.hooks.pre) {
        HookOutcome::Ok => Ok(()),
        HookOutcome::Failed(msg) => Err(format!("skill '{}' pre hook: {msg}", plan.skill)),
    }
}

/// Run the plan's `post` hook. Failures are NOT fatal — the turn already
/// happened — so this returns the log line instead of an error, and `None`
/// when there was nothing to report.
#[must_use = "the post-hook failure should be logged"]
pub fn run_post_hooks(plan: &InvocationPlan) -> Option<String> {
    if plan.hooks.post.is_empty() {
        return None;
    }
    match run_hook(plan, &plan.hooks.post) {
        HookOutcome::Ok => None,
        HookOutcome::Failed(msg) => Some(format!("skill '{}' post hook: {msg}", plan.skill)),
    }
}

enum HookOutcome {
    Ok,
    Failed(String),
}

/// Spawn one hook, enforce the timeout, and collect stderr.
///
/// stdout is discarded (a hook is a side effect, not a producer) and stdin is
/// null so a hook that tries to prompt fails fast instead of hanging until the
/// timeout.
fn run_hook(plan: &InvocationPlan, argv: &[String]) -> HookOutcome {
    let Some((program, rest)) = argv.split_first() else {
        return HookOutcome::Ok;
    };
    let mut cmd = Command::new(program);
    cmd.args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(dir) = &plan.hooks.cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in &plan.hooks.env {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return HookOutcome::Failed(format!("cannot run `{program}`: {e}")),
    };
    // Take stderr up front so the pipe is drained by `read_to_end` below even
    // in the kill path; a hook writing more than a pipe buffer would otherwise
    // block forever on write.
    let mut stderr_pipe = child.stderr.take();

    let deadline = Instant::now() + plan.hooks.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(HOOK_POLL_INTERVAL);
            }
            Err(e) => return HookOutcome::Failed(format!("`{program}` failed to run: {e}")),
        }
    };

    let mut stderr = String::new();
    if let Some(pipe) = stderr_pipe.as_mut() {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        stderr = String::from_utf8_lossy(&buf).trim().to_string();
    }

    match status {
        None => HookOutcome::Failed(format!(
            "`{program}` timed out after {}s{}",
            plan.hooks.timeout.as_secs(),
            tail(&stderr)
        )),
        Some(s) if s.success() => HookOutcome::Ok,
        Some(s) => HookOutcome::Failed(format!(
            "`{program}` exited with {}{}",
            s.code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "a signal".to_string()),
            tail(&stderr)
        )),
    }
}

/// Append a hook's stderr to a failure message, if it said anything.
fn tail(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!("\n{stderr}")
    }
}

/// Union two tool policies: allow lists concatenate (deduped) and deny wins —
/// a tool denied anywhere in the composition stays denied, so composing a
/// skill can only ever TIGHTEN the policy, never loosen it.
fn merge_tools(into: &mut ToolPolicy, from: &ToolPolicy) {
    for a in &from.allow {
        if !into.allow.contains(a) {
            into.allow.push(a.clone());
        }
    }
    for d in &from.deny {
        if !into.deny.contains(d) {
            into.deny.push(d.clone());
        }
    }
    into.allow.retain(|a| !into.deny.contains(a));
}

/// The turn's user prompt: the skill's `template` rendered with the bound
/// args, or the raw argument string verbatim when no template is declared.
fn render_user_prompt(
    reg: &Registry,
    skill: &str,
    args: &IndexMap<String, String>,
) -> Result<String, String> {
    let def = reg
        .skills
        .get(skill)
        .ok_or_else(|| unknown_skill_message(reg, skill))?;
    let Some(template) = &def.template else {
        // No template: exactly today's behaviour — what the user typed.
        return Ok(args.get("args").cloned().unwrap_or_default());
    };
    if !reg.prompts.has(template) {
        // The loader cross-references this, so reaching here means a template
        // was deleted between load and use. Say so rather than silently
        // sending an empty prompt.
        return Err(format!(
            "skill '{skill}': prompt template '{template}' is not defined"
        ));
    }
    let vars: Vec<(&str, &str)> = args.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    Ok(reg.prompts.render(template, &vars))
}

/// The agent's `cwd`, tilde-expanded, when the skill selects an agent that
/// declares one. Hooks run there so `git`-shaped hooks act on the right repo.
fn agent_cwd(reg: &Registry, agent: Option<&str>) -> Option<PathBuf> {
    let raw = reg.agents.get(agent?)?.cwd.as_ref()?;
    Some(expand_tilde(raw))
}

fn expand_tilde(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    let Some(home) = std::env::var_os("HOME") else {
        return PathBuf::from(path);
    };
    PathBuf::from(home).join(rest.trim_start_matches('/'))
}

/// Substitute hook placeholders PER ARGV ELEMENT.
///
/// Supported: `{{arg:<name>}}`, `{{skill}}`, `{{agent}}`, `{{cwd}}`. An
/// unknown placeholder is left VERBATIM (same rule as prompt templates) so a
/// typo is visible in the failure rather than silently becoming an empty
/// argument. Substitution never splits: the result of one element is always
/// exactly one argument.
fn substitute_argv(
    argv: &[String],
    skill: &str,
    args: &IndexMap<String, String>,
    agent: Option<&str>,
    cwd: &Option<PathBuf>,
) -> Vec<String> {
    if argv.is_empty() {
        return Vec::new();
    }
    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("skill".to_string(), skill.to_string());
    vars.insert("agent".to_string(), agent.unwrap_or_default().to_string());
    vars.insert(
        "cwd".to_string(),
        cwd.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        }),
    );
    for (k, v) in args {
        vars.insert(format!("arg:{k}"), v.clone());
    }
    argv.iter().map(|e| substitute(e, &vars)).collect()
}

/// Replace `{{key}}` occurrences in one string; unknown keys survive intact.
///
/// Hand-rolled rather than regex because the substituted VALUES must never be
/// rescanned — a value containing `{{skill}}` is data, not a placeholder.
fn substitute(input: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"{{") {
            if let Some(end) = input[i + 2..].find("}}") {
                let key = &input[i + 2..i + 2 + end];
                let trimmed = key.trim();
                match vars.get(trimmed) {
                    Some(value) => {
                        out.push_str(value);
                        i += 2 + end + 2;
                        continue;
                    }
                    None => {
                        // Unknown: emit the whole `{{...}}` verbatim.
                        out.push_str(&input[i..i + 2 + end + 2]);
                        i += 2 + end + 2;
                        continue;
                    }
                }
            }
        }
        // Push one whole UTF-8 character, never a byte, so multi-byte input
        // survives.
        let ch = input[i..].chars().next().expect("in-bounds char");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A config dir built from `(relative path, contents)` pairs, loaded
    /// through the REAL loader — these are integration tests of the schema.
    fn registry(files: &[(&str, &str)]) -> (TempDir, Registry) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (rel, body) in files {
            let path = dir.path().join(rel);
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(&path, body).expect("write");
        }
        let reg = stackhour_core::registry::load(dir.path());
        (dir, reg)
    }

    fn skill_files() -> Vec<(&'static str, &'static str)> {
        vec![
            ("skills/review/skill.toml", "description = \"Review a PR\"\n"),
            ("skills/review/skill.md", "Read before you write.\n"),
        ]
    }

    // ---- DEFAULTS-ONLY ----

    #[test]
    fn an_empty_config_dir_has_no_skills_and_says_so_clearly() {
        let (_d, reg) = registry(&[]);
        assert!(reg.skills.is_empty());
        assert_eq!(
            plan_invocation(&reg, "review", "").unwrap_err(),
            "unknown skill 'review' (no skills are defined)"
        );
    }

    #[test]
    fn a_minimal_skill_behaves_exactly_as_before() {
        let (_d, reg) = registry(&skill_files());
        let plan = plan_invocation(&reg, "review", "  look at PR 4821  ").expect("plan");
        // No template -> the raw argument string, trimmed, verbatim.
        assert_eq!(plan.user_prompt, "look at PR 4821");
        assert_eq!(plan.agent_override, None);
        assert_eq!(plan.tools, ToolPolicy::default());
        assert!(plan.env.is_empty());
        assert!(plan.hooks.is_empty());
        assert_eq!(plan.system_fragments.len(), 1);
        assert_eq!(plan.system_fragments[0].name, "review");
        assert_eq!(plan.system_fragments[0].body, "Read before you write.");
    }

    // ---- A VALID CUSTOM SKILL TAKES EFFECT ----

    #[test]
    fn a_full_skill_binds_args_renders_its_template_and_selects_its_agent() {
        let (_d, reg) = registry(&[
            (
                "engines/ollama.toml",
                "label = \"Ollama\"\nbin = \"ollama\"\nkind = \"plain-lines\"\n",
            ),
            (
                "agents/reviewer/agent.toml",
                "engine = \"ollama\"\nlabel = \"Reviewer\"\n",
            ),
            ("agents/reviewer/soul.md", "You review.\n"),
            (
                "prompts/review-prompt.md",
                "Review PR {{pr}} focusing on {{focus}}.",
            ),
            (
                "skills/review/skill.toml",
                r#"
description = "Review a PR"
agent = "reviewer"
template = "review-prompt"

[[args]]
name = "pr"
required = true

[[args]]
name = "focus"
default = "correctness"
rest = true

[tools]
allow = ["Bash"]

[env]
REVIEW_MODE = "strict"
"#,
            ),
            ("skills/review/skill.md", "Read before you write.\n"),
        ]);
        assert!(reg.errors.is_empty(), "unexpected: {:?}", reg.errors);

        let plan = plan_invocation(&reg, "review", "4821 the error paths").expect("plan");
        assert_eq!(plan.args["pr"], "4821");
        assert_eq!(plan.args["focus"], "the error paths");
        assert_eq!(plan.user_prompt, "Review PR 4821 focusing on the error paths.");
        assert_eq!(plan.agent_override.as_deref(), Some("reviewer"));
        assert_eq!(plan.tools.allow, vec!["Bash"]);
        assert_eq!(plan.env["REVIEW_MODE"], "strict");

        // An omitted optional argument falls back to its default.
        let plan = plan_invocation(&reg, "review", "99").expect("plan");
        assert_eq!(plan.user_prompt, "Review PR 99 focusing on correctness.");
    }

    #[test]
    fn composition_orders_fragments_dependency_first_and_tightens_policy() {
        let (_d, reg) = registry(&[
            (
                "skills/base/skill.toml",
                "description = \"base\"\n[tools]\nallow = [\"Bash\", \"Edit\"]\n[env]\nA = \"1\"\n",
            ),
            ("skills/base/skill.md", "BASE"),
            (
                "skills/top/skill.toml",
                "description = \"top\"\nuses = [\"base\"]\n[tools]\ndeny = [\"Edit\"]\n[env]\nA = \"2\"\nB = \"3\"\n",
            ),
            ("skills/top/skill.md", "TOP"),
        ]);
        assert!(reg.errors.is_empty(), "unexpected: {:?}", reg.errors);

        let plan = plan_invocation(&reg, "top", "").expect("plan");
        let names: Vec<&str> = plan.system_fragments.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["base", "top"]);
        // deny wins over an inherited allow: composing can only tighten.
        assert_eq!(plan.tools.allow, vec!["Bash"]);
        assert_eq!(plan.tools.deny, vec!["Edit"]);
        // The nearer skill's env wins.
        assert_eq!(plan.env["A"], "2");
        assert_eq!(plan.env["B"], "3");
    }

    #[test]
    fn an_agent_listing_a_skill_gets_fragments_without_binding_args() {
        let (_d, reg) = registry(&[
            (
                "skills/review/skill.toml",
                "description = \"d\"\n[[args]]\nname = \"pr\"\nrequired = true\n",
            ),
            ("skills/review/skill.md", "BODY"),
        ]);
        // Invoking without the required arg fails...
        assert!(plan_invocation(&reg, "review", "").is_err());
        // ...but merely listing it on an agent must not.
        let frags = fragments_for(&reg, "review").expect("fragments");
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].body, "BODY");
    }

    // ---- INVALID INPUT PRODUCES THE RIGHT ERROR ----

    #[test]
    fn a_missing_required_argument_names_the_skill_and_the_argument() {
        let (_d, reg) = registry(&[(
            "skills/review/skill.toml",
            "description = \"d\"\n[[args]]\nname = \"pr\"\nrequired = true\ndescription = \"pull request number\"\n",
        )]);
        assert_eq!(
            plan_invocation(&reg, "review", "").unwrap_err(),
            "/review: key `pr`: missing required argument 'pr' (pull request number)"
        );
    }

    #[test]
    fn an_out_of_range_choice_is_rejected_at_bind_time() {
        let (_d, reg) = registry(&[(
            "skills/deploy/skill.toml",
            "description = \"d\"\n[[args]]\nname = \"env\"\nchoices = [\"staging\", \"prod\"]\n",
        )]);
        let e = plan_invocation(&reg, "deploy", "wat").unwrap_err();
        assert!(e.starts_with("/deploy: key `env`:"), "got: {e}");
        assert!(e.contains("expected one of staging, prod"), "got: {e}");
    }

    #[test]
    fn an_unknown_skill_lists_the_known_ones() {
        let (_d, reg) = registry(&skill_files());
        assert_eq!(
            plan_invocation(&reg, "reveiw", "").unwrap_err(),
            "unknown skill 'reveiw' (known: review)"
        );
    }

    #[test]
    fn a_uses_cycle_is_dropped_by_the_loader_and_reported() {
        let (_d, reg) = registry(&[
            ("skills/a/skill.toml", "description = \"a\"\nuses = [\"b\"]\n"),
            ("skills/b/skill.toml", "description = \"b\"\nuses = [\"a\"]\n"),
        ]);
        // Both nodes on the cycle are dropped, so neither is invocable and the
        // registry carries an error naming the cycle.
        assert!(reg.skills.is_empty(), "got: {:?}", reg.skills.keys());
        assert!(
            reg.errors.iter().any(|e| e.message.contains("cycle")),
            "got: {:?}",
            reg.errors
        );
    }

    #[test]
    fn a_skill_dir_missing_its_manifest_is_reported_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("skills/halfdone")).expect("mkdir");
        let reg = stackhour_core::registry::load(dir.path());
        assert!(reg.skills.is_empty());
        assert_eq!(reg.errors.len(), 1);
        assert_eq!(reg.errors[0].message, "missing skill.toml");
    }

    // ---- HOOKS ----

    #[test]
    fn hook_argv_placeholders_substitute_per_element_and_never_split() {
        let (_d, reg) = registry(&[(
            "skills/review/skill.toml",
            r#"
description = "d"
[[args]]
name = "note"
rest = true
[hooks]
pre = ["/bin/echo", "{{skill}}", "{{arg:note}}", "{{arg:missing}}"]
"#,
        )]);
        let plan = plan_invocation(&reg, "review", "; rm -rf / #").expect("plan");
        assert_eq!(
            plan.hooks.pre,
            vec![
                "/bin/echo".to_string(),
                "review".to_string(),
                // One element, shell metacharacters and all.
                "; rm -rf / #".to_string(),
                // Unknown placeholder survives verbatim.
                "{{arg:missing}}".to_string(),
            ]
        );
    }

    #[test]
    fn a_successful_pre_hook_lets_the_turn_proceed() {
        let (_d, reg) = registry(&[(
            "skills/s/skill.toml",
            "description = \"d\"\n[hooks]\npre = [\"/usr/bin/true\"]\npost = [\"/usr/bin/true\"]\n",
        )]);
        let plan = plan_invocation(&reg, "s", "").expect("plan");
        assert_eq!(run_pre_hooks(&plan), Ok(()));
        assert_eq!(run_post_hooks(&plan), None);
    }

    #[test]
    fn a_failing_pre_hook_aborts_and_surfaces_its_stderr() {
        let (_d, reg) = registry(&[(
            "skills/s/skill.toml",
            "description = \"d\"\n[hooks]\npre = [\"/bin/sh\", \"-c\", \"echo nope 1>&2; exit 3\"]\n",
        )]);
        let plan = plan_invocation(&reg, "s", "").expect("plan");
        let e = run_pre_hooks(&plan).unwrap_err();
        assert!(e.starts_with("skill 's' pre hook:"), "got: {e}");
        assert!(e.contains("exited with 3"), "got: {e}");
        assert!(e.contains("nope"), "got: {e}");
    }

    #[test]
    fn a_failing_post_hook_is_reported_but_not_fatal() {
        let (_d, reg) = registry(&[(
            "skills/s/skill.toml",
            "description = \"d\"\n[hooks]\npost = [\"/usr/bin/false\"]\n",
        )]);
        let plan = plan_invocation(&reg, "s", "").expect("plan");
        let msg = run_post_hooks(&plan).expect("a log line");
        assert!(msg.contains("post hook"), "got: {msg}");
    }

    #[test]
    fn a_missing_hook_binary_is_a_clear_error_not_a_panic() {
        let (_d, reg) = registry(&[(
            "skills/s/skill.toml",
            "description = \"d\"\n[hooks]\npre = [\"/nonexistent/definitely-not-here\"]\n",
        )]);
        let plan = plan_invocation(&reg, "s", "").expect("plan");
        let e = run_pre_hooks(&plan).unwrap_err();
        assert!(e.contains("cannot run"), "got: {e}");
    }

    #[test]
    fn a_hung_hook_is_killed_at_the_timeout() {
        let (_d, reg) = registry(&[(
            "skills/s/skill.toml",
            "description = \"d\"\n[hooks]\npre = [\"/bin/sleep\", \"30\"]\ntimeout_seconds = 1\n",
        )]);
        let plan = plan_invocation(&reg, "s", "").expect("plan");
        let started = Instant::now();
        let e = run_pre_hooks(&plan).unwrap_err();
        assert!(e.contains("timed out after 1s"), "got: {e}");
        assert!(started.elapsed() < Duration::from_secs(10), "did not kill");
    }

    #[test]
    fn hooks_see_the_skills_env() {
        let (_d, reg) = registry(&[(
            "skills/s/skill.toml",
            "description = \"d\"\n[env]\nSTACKHOUR_TEST_HOOK = \"yes\"\n[hooks]\npre = [\"/bin/sh\", \"-c\", \"test \\\"$STACKHOUR_TEST_HOOK\\\" = yes\"]\n",
        )]);
        let plan = plan_invocation(&reg, "s", "").expect("plan");
        assert_eq!(run_pre_hooks(&plan), Ok(()));
    }

    // ---- substitution unit tests ----

    #[test]
    fn substitution_does_not_rescan_substituted_values() {
        let mut vars = HashMap::new();
        vars.insert("a".to_string(), "{{b}}".to_string());
        vars.insert("b".to_string(), "BOOM".to_string());
        assert_eq!(substitute("{{a}}", &vars), "{{b}}");
    }

    #[test]
    fn substitution_survives_unterminated_braces_and_unicode() {
        let vars = HashMap::new();
        assert_eq!(substitute("{{unclosed", &vars), "{{unclosed");
        assert_eq!(substitute("héllo — {{x}}", &vars), "héllo — {{x}}");
    }
}
