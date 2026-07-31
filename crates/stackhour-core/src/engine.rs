//! EngineDef: how to spawn a coding engine and parse its stream, as pure data.
//!
//! Two embedded built-ins, `claude` and `codex`, define the process contract
//! used by control nodes. `kind` selects one of the shipped stream parsers;
//! new stream protocols still require code.
//!
//! Argv templates use dumb `{{placeholder}}` substitution: `{{session_id}}`,
//! `{{model}}`, `{{permission_mode}}`, `{{system_prompt}}`. Unknown
//! placeholders are left verbatim (same rule as PromptStore).
//!
//! Argv assembly semantics are implemented by [`EngineDef::assemble_argv`]:
//!
//! 1. Start from `args` with placeholders substituted.
//! 2. The splice point is just before a trailing `-` stdin sentinel when the
//!    last base arg is exactly `-`, else the end of the base args.
//! 3. Insert at the splice point, in order: `partial_messages_flag` (only when
//!    `live_status`), the rendered `permission_args`, the rendered
//!    `system_prompt_args` (only when a system prompt is set), the rendered
//!    `model_args` (only when a model is set), then the additive
//!    `effort_args` / tool-policy args. The system-prompt-before-model order
//!    is byte-parity with coordinator.mjs, which emits
//!    `--permission-mode <m> --append-system-prompt <rules> --model <m>`.
//! 4. When a session id is present: `ResumeStyle::Flag` appends its rendered
//!    args at the very end (claude: `--resume <id>`); `ResumeStyle::Subcommand`
//!    inserts the subcommand word right after the first arg and places the raw
//!    session id before the trailing `-` sentinel (codex:
//!    `exec resume … <id> -`).
//!
//! Permission-mode branch (deliberate shipped rule, mirroring the stream-parser
//! scope boundary): `permission_args` whose template consumes
//! `{{permission_mode}}` render with the mode value (claude:
//! `--permission-mode default`). `permission_args` that do NOT reference the
//! placeholder are sandbox-style: they are emitted as-is for mode `default`
//! and replaced wholesale by `--dangerously-bypass-approvals-and-sandbox` for
//! mode `bypassPermissions` (codex: `--sandbox workspace-write` vs the bypass
//! flag). This branch cannot be expressed in a flat dumb template; encoding it
//! as a shipped rule keeps user TOML engines code-free while preserving
//! byte-parity with coordinator.mjs/worker.mjs in BOTH permission modes.

use indexmap::IndexMap;

/// The one permission mode value that changes spawn flags.
pub const BYPASS_PERMISSION_MODE: &str = "bypassPermissions";
/// Codex's sandbox-escape flag, substituted for sandbox-style
/// `permission_args` when the mode is [`BYPASS_PERMISSION_MODE`].
pub const BYPASS_SANDBOX_FLAG: &str = "--dangerously-bypass-approvals-and-sandbox";

const PLACEHOLDER_SESSION_ID: &str = "{{session_id}}";
const PLACEHOLDER_MODEL: &str = "{{model}}";
const PLACEHOLDER_PERMISSION_MODE: &str = "{{permission_mode}}";
const PLACEHOLDER_SYSTEM_PROMPT: &str = "{{system_prompt}}";
/// Substituted into `effort_args`. Engines that declare no `effort_args`
/// silently ignore an agent's `effort` field.
const PLACEHOLDER_EFFORT: &str = "{{effort}}";
/// Substituted into `allowed_tools_args` / `disallowed_tools_args`. Rendered
/// as the comma-joined tool list, which is the shape both Claude Code and
/// Codex accept.
const PLACEHOLDER_ALLOWED_TOOLS: &str = "{{allowed_tools}}";
const PLACEHOLDER_DISALLOWED_TOOLS: &str = "{{disallowed_tools}}";

/// Which shipped stream parser to use for the child's stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// `claude -p --output-format stream-json` events.
    ClaudeStreamJson,
    /// codex `--json` JSONL events.
    CodexJsonl,
    /// Accumulate plain output lines.
    PlainLines,
}

/// How a previous session is resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeStyle {
    /// Args appended to the base argv (claude: `--resume {{session_id}}`).
    /// An empty args list means the engine does not support resumption.
    Flag { args: Vec<String> },
    /// A subcommand inserted after the first arg, with the session id placed
    /// before the trailing `-` stdin sentinel (codex: `resume`).
    Subcommand { insert: String },
}

/// How the prompt reaches the child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDelivery {
    Stdin,
    LastArg,
}

/// A coding-engine process definition.
#[derive(Debug, Clone)]
pub struct EngineDef {
    pub name: String,
    pub label: String,
    pub emoji: String,
    /// Binary path or bare name resolved on PATH at spawn. For the built-ins
    /// this is the bare name; node configuration can override it at spawn.
    pub bin: String,
    pub kind: StreamKind,
    /// Base argv (after the binary), `{{placeholder}}` templated.
    pub args: Vec<String>,
    pub resume: ResumeStyle,
    /// e.g. `["--model", "{{model}}"]`; None = engine takes no model flag.
    pub model_args: Option<Vec<String>>,
    /// e.g. `["--permission-mode", "{{permission_mode}}"]`. Templates that do
    /// not reference `{{permission_mode}}` are sandbox-style: replaced by
    /// [`BYPASS_SANDBOX_FLAG`] under mode [`BYPASS_PERMISSION_MODE`].
    pub permission_args: Option<Vec<String>>,
    /// e.g. `["--append-system-prompt", "{{system_prompt}}"]`; None = the
    /// composed soul text is prepended to the user prompt instead.
    pub system_prompt_args: Option<Vec<String>>,
    pub prompt_delivery: PromptDelivery,
    /// Extra env for the child, merged over the target's extraPath.
    pub env: IndexMap<String, String>,
    /// Applied only when live status updates are requested.
    pub partial_messages_flag: Option<String>,
    /// e.g. `["--reasoning-effort", "{{effort}}"]`. Spliced only when the
    /// agent sets `effort`; `None` means this engine has no notion of effort
    /// and an agent's `effort` is ignored rather than an error (engines are
    /// swappable, so a soft ignore is the right failure mode).
    pub effort_args: Option<Vec<String>>,
    /// e.g. `["--allowedTools", "{{allowed_tools}}"]`. Spliced only when the
    /// effective [`ToolPolicy`] has a non-empty `allow` list.
    pub allowed_tools_args: Option<Vec<String>>,
    /// e.g. `["--disallowedTools", "{{disallowed_tools}}"]`.
    ///
    /// Unlike `effort_args`, a `deny` list that an engine cannot express is
    /// NOT safe to ignore silently — see [`EngineDef::unenforceable_policy`],
    /// which callers use to refuse the run instead.
    pub disallowed_tools_args: Option<Vec<String>>,
}

/// Spawn-time template variables for [`EngineDef::assemble_argv`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ArgvVars<'a> {
    pub session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    /// None renders as `default` (JS: `tgt.permissionMode || 'default'`).
    pub permission_mode: Option<&'a str>,
    pub system_prompt: Option<&'a str>,
    /// Agent reasoning effort ("low" | "medium" | "high"); `None` = the
    /// engine's own default and `effort_args` is not spliced at all.
    pub effort: Option<&'a str>,
    /// Tools to allow; empty = do not splice `allowed_tools_args` at all.
    pub allow_tools: &'a [String],
    /// Tools to deny; empty = do not splice `disallowed_tools_args` at all.
    pub deny_tools: &'a [String],
    /// true on the coordinator's local lane (enables partial_messages_flag).
    pub live_status: bool,
}

fn subst(template: &str, vars: &ArgvVars<'_>) -> String {
    template
        .replace(PLACEHOLDER_SESSION_ID, vars.session_id.unwrap_or(""))
        .replace(PLACEHOLDER_MODEL, vars.model.unwrap_or(""))
        .replace(
            PLACEHOLDER_PERMISSION_MODE,
            vars.permission_mode.unwrap_or("default"),
        )
        .replace(PLACEHOLDER_SYSTEM_PROMPT, vars.system_prompt.unwrap_or(""))
        .replace(PLACEHOLDER_EFFORT, vars.effort.unwrap_or(""))
        .replace(PLACEHOLDER_ALLOWED_TOOLS, &vars.allow_tools.join(","))
        .replace(PLACEHOLDER_DISALLOWED_TOOLS, &vars.deny_tools.join(","))
}

/// The built-in `claude` engine (argv byte-identical to coordinator.mjs/worker.mjs).
///
/// Coordinator (live_status = true):
/// `-p --output-format stream-json --verbose --include-partial-messages
///  --permission-mode <mode> [--model <m>] [--resume <sid>]`
/// Worker (live_status = false): same minus `--include-partial-messages`.
pub fn builtin_claude() -> EngineDef {
    EngineDef {
        name: "claude".to_string(),
        label: "Claude".to_string(),
        emoji: "🧠".to_string(),
        bin: "claude".to_string(),
        kind: StreamKind::ClaudeStreamJson,
        args: vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ],
        resume: ResumeStyle::Flag {
            args: vec!["--resume".to_string(), PLACEHOLDER_SESSION_ID.to_string()],
        },
        model_args: Some(vec!["--model".to_string(), PLACEHOLDER_MODEL.to_string()]),
        permission_args: Some(vec![
            "--permission-mode".to_string(),
            PLACEHOLDER_PERMISSION_MODE.to_string(),
        ]),
        system_prompt_args: Some(vec![
            "--append-system-prompt".to_string(),
            PLACEHOLDER_SYSTEM_PROMPT.to_string(),
        ]),
        prompt_delivery: PromptDelivery::Stdin,
        env: IndexMap::new(),
        partial_messages_flag: Some("--include-partial-messages".to_string()),
        // Claude Code exposes no reasoning-effort flag today.
        effort_args: None,
        // Claude Code's own tool gating. Splicing these is a no-op unless an
        // agent or skill actually declares a `[tools]` policy, so argv stays
        // byte-identical to coordinator.mjs/worker.mjs for every existing
        // config.
        allowed_tools_args: Some(vec![
            "--allowedTools".to_string(),
            PLACEHOLDER_ALLOWED_TOOLS.to_string(),
        ]),
        disallowed_tools_args: Some(vec![
            "--disallowedTools".to_string(),
            PLACEHOLDER_DISALLOWED_TOOLS.to_string(),
        ]),
    }
}

/// The built-in `codex` engine (argv byte-identical to coordinator.mjs/worker.mjs).
///
/// Fresh: `exec --json --skip-git-repo-check <sandbox-or-bypass> [--model <m>] -`
/// Resume: `exec resume --json --skip-git-repo-check <sandbox-or-bypass>
///          [--model <m>] <sid> -`
pub fn builtin_codex() -> EngineDef {
    EngineDef {
        name: "codex".to_string(),
        label: "Codex".to_string(),
        emoji: "🛠".to_string(),
        bin: "codex".to_string(),
        kind: StreamKind::CodexJsonl,
        args: vec![
            "exec".to_string(),
            "--json".to_string(),
            "--skip-git-repo-check".to_string(),
            "-".to_string(),
        ],
        resume: ResumeStyle::Subcommand {
            insert: "resume".to_string(),
        },
        model_args: Some(vec!["--model".to_string(), PLACEHOLDER_MODEL.to_string()]),
        // Sandbox-style (no {{permission_mode}} reference): emitted as-is for
        // mode "default", replaced by BYPASS_SANDBOX_FLAG for
        // "bypassPermissions" — exactly the coordinator.mjs/worker.mjs branch.
        permission_args: Some(vec!["--sandbox".to_string(), "workspace-write".to_string()]),
        // Codex has no system-prompt flag: souls are prepended to the prompt.
        system_prompt_args: None,
        prompt_delivery: PromptDelivery::Stdin,
        env: IndexMap::new(),
        partial_messages_flag: None,
        // Codex exposes reasoning effort as a config override. This remains a
        // structured argv pair; no shell parses the value.
        effort_args: Some(vec![
            "-c".to_string(),
            "model_reasoning_effort=\"{{effort}}\"".to_string(),
        ]),
        // `codex exec` has no per-tool allow/deny flags. Leaving these None
        // means a `[tools]` policy on a codex agent is UNENFORCEABLE, which
        // `unenforceable_policy` turns into a refusal rather than a silent
        // grant.
        allowed_tools_args: None,
        disallowed_tools_args: None,
    }
}

impl EngineDef {
    /// Render the permission args for a mode (None = `default`).
    ///
    /// Templates referencing `{{permission_mode}}` get the mode substituted;
    /// sandbox-style templates (no placeholder) are replaced wholesale by
    /// [`BYPASS_SANDBOX_FLAG`] when the mode is [`BYPASS_PERMISSION_MODE`].
    pub fn permission_argv(&self, permission_mode: Option<&str>) -> Vec<String> {
        let Some(template) = &self.permission_args else {
            return Vec::new();
        };
        let mode = permission_mode.unwrap_or("default");
        let takes_mode_flag = template.iter().any(|a| a.contains(PLACEHOLDER_PERMISSION_MODE));
        if mode == BYPASS_PERMISSION_MODE && !takes_mode_flag {
            return vec![BYPASS_SANDBOX_FLAG.to_string()];
        }
        let vars = ArgvVars {
            permission_mode: Some(mode),
            ..ArgvVars::default()
        };
        template.iter().map(|a| subst(a, &vars)).collect()
    }

    /// Assemble the full argv (after the binary) for one spawn.
    pub fn assemble_argv(&self, vars: &ArgvVars<'_>) -> Vec<String> {
        let mut out: Vec<String> = self.args.iter().map(|a| subst(a, vars)).collect();

        // Splice point: just before a trailing '-' stdin sentinel.
        let mut splice = match out.last().map(String::as_str) {
            Some("-") => out.len() - 1,
            _ => out.len(),
        };
        let mut splice_in = |out: &mut Vec<String>, items: Vec<String>| {
            for item in items {
                out.insert(splice, item);
                splice += 1;
            }
        };

        if vars.live_status {
            if let Some(flag) = &self.partial_messages_flag {
                splice_in(&mut out, vec![flag.clone()]);
            }
        }
        splice_in(&mut out, self.permission_argv(vars.permission_mode));
        // BEFORE the model args: coordinator.mjs emits
        // `--permission-mode <m> --append-system-prompt <rules> --model <m>`,
        // and this splice order is the byte-parity surface.
        if vars.system_prompt.is_some() {
            if let Some(template) = &self.system_prompt_args {
                splice_in(&mut out, template.iter().map(|a| subst(a, vars)).collect());
            }
        }
        if vars.model.is_some() {
            if let Some(template) = &self.model_args {
                splice_in(&mut out, template.iter().map(|a| subst(a, vars)).collect());
            }
        }
        if vars.effort.is_some() {
            if let Some(template) = &self.effort_args {
                splice_in(&mut out, template.iter().map(|a| subst(a, vars)).collect());
            }
        }
        if !vars.allow_tools.is_empty() {
            if let Some(template) = &self.allowed_tools_args {
                splice_in(&mut out, template.iter().map(|a| subst(a, vars)).collect());
            }
        }
        if !vars.deny_tools.is_empty() {
            if let Some(template) = &self.disallowed_tools_args {
                splice_in(&mut out, template.iter().map(|a| subst(a, vars)).collect());
            }
        }

        if let Some(sid) = vars.session_id {
            match &self.resume {
                ResumeStyle::Flag { args } => {
                    out.extend(args.iter().map(|a| subst(a, vars)));
                }
                ResumeStyle::Subcommand { insert } => {
                    let sub_at = 1.min(out.len());
                    out.insert(sub_at, insert.clone());
                    let sid_at = match out.last().map(String::as_str) {
                        Some("-") => out.len() - 1,
                        _ => out.len(),
                    };
                    out.insert(sid_at, sid.to_string());
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(def: &EngineDef, vars: &ArgvVars<'_>) -> Vec<String> {
        def.assemble_argv(vars)
    }

    // ---- claude byte-parity (coordinator.mjs spawnLocal) ----

    #[test]
    fn claude_coordinator_fresh_no_model() {
        let def = builtin_claude();
        let vars = ArgvVars {
            live_status: true,
            ..ArgvVars::default()
        };
        assert_eq!(
            argv(&def, &vars),
            vec![
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-mode",
                "default",
            ]
        );
    }

    #[test]
    fn claude_coordinator_resume_with_model_and_mode() {
        let def = builtin_claude();
        let vars = ArgvVars {
            session_id: Some("sess-123"),
            model: Some("opus"),
            permission_mode: Some("bypassPermissions"),
            live_status: true,
            ..ArgvVars::default()
        };
        assert_eq!(
            argv(&def, &vars),
            vec![
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-mode",
                "bypassPermissions",
                "--model",
                "opus",
                "--resume",
                "sess-123",
            ]
        );
    }

    // ---- claude byte-parity (worker.mjs runClaude — no partial messages) ----

    #[test]
    fn claude_worker_omits_include_partial_messages() {
        let def = builtin_claude();
        let vars = ArgvVars {
            session_id: Some("abc"),
            live_status: false,
            ..ArgvVars::default()
        };
        assert_eq!(
            argv(&def, &vars),
            vec![
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "default",
                "--resume",
                "abc",
            ]
        );
    }

    // ---- codex byte-parity (both coordinator and worker share the argv) ----

    #[test]
    fn codex_fresh_default_mode() {
        let def = builtin_codex();
        let vars = ArgvVars::default();
        assert_eq!(
            argv(&def, &vars),
            vec![
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                "workspace-write",
                "-",
            ]
        );
    }

    #[test]
    fn codex_fresh_bypass_with_model() {
        let def = builtin_codex();
        let vars = ArgvVars {
            model: Some("gpt-5-codex"),
            permission_mode: Some("bypassPermissions"),
            ..ArgvVars::default()
        };
        assert_eq!(
            argv(&def, &vars),
            vec![
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "--model",
                "gpt-5-codex",
                "-",
            ]
        );
    }

    #[test]
    fn codex_resume_inserts_subcommand_and_session_before_sentinel() {
        let def = builtin_codex();
        let vars = ArgvVars {
            session_id: Some("thread-9"),
            ..ArgvVars::default()
        };
        assert_eq!(
            argv(&def, &vars),
            vec![
                "exec",
                "resume",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                "workspace-write",
                "thread-9",
                "-",
            ]
        );
    }

    #[test]
    fn codex_resume_bypass_with_model() {
        let def = builtin_codex();
        let vars = ArgvVars {
            session_id: Some("t1"),
            model: Some("gpt-5"),
            permission_mode: Some("bypassPermissions"),
            ..ArgvVars::default()
        };
        assert_eq!(
            argv(&def, &vars),
            vec![
                "exec",
                "resume",
                "--json",
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "--model",
                "gpt-5",
                "t1",
                "-",
            ]
        );
    }

    #[test]
    fn codex_live_status_has_no_partial_flag() {
        let def = builtin_codex();
        let fresh = argv(&def, &ArgvVars::default());
        let live = argv(
            &def,
            &ArgvVars {
                live_status: true,
                ..ArgvVars::default()
            },
        );
        assert_eq!(fresh, live);
    }

    // ---- builtin metadata ----

    #[test]
    fn builtin_metadata() {
        let c = builtin_claude();
        assert_eq!(
            (c.name.as_str(), c.label.as_str(), c.emoji.as_str()),
            ("claude", "Claude", "🧠")
        );
        assert_eq!(c.kind, StreamKind::ClaudeStreamJson);
        assert_eq!(c.prompt_delivery, PromptDelivery::Stdin);
        assert!(c.env.is_empty());

        let x = builtin_codex();
        assert_eq!(
            (x.name.as_str(), x.label.as_str(), x.emoji.as_str()),
            ("codex", "Codex", "🛠")
        );
        assert_eq!(x.kind, StreamKind::CodexJsonl);
        assert_eq!(x.bin, "codex");
        assert_eq!(x.partial_messages_flag, None);
        assert_eq!(x.system_prompt_args, None);
    }

    // ---- permission_argv shipped rule ----

    #[test]
    fn permission_argv_mode_flag_template_keeps_mode_value() {
        let def = builtin_claude();
        assert_eq!(
            def.permission_argv(Some("bypassPermissions")),
            vec!["--permission-mode", "bypassPermissions"]
        );
        assert_eq!(def.permission_argv(None), vec!["--permission-mode", "default"]);
    }

    #[test]
    fn permission_argv_sandbox_template_swaps_to_bypass_flag() {
        let def = builtin_codex();
        assert_eq!(
            def.permission_argv(Some("bypassPermissions")),
            vec![BYPASS_SANDBOX_FLAG]
        );
        assert_eq!(
            def.permission_argv(Some("default")),
            vec!["--sandbox", "workspace-write"]
        );
        assert_eq!(def.permission_argv(None), vec!["--sandbox", "workspace-write"]);
    }

    #[test]
    fn permission_argv_none_is_empty() {
        let mut def = builtin_codex();
        def.permission_args = None;
        assert!(def.permission_argv(Some("bypassPermissions")).is_empty());
    }

    // ---- substitution ----

    #[test]
    fn unknown_placeholders_left_verbatim() {
        let vars = ArgvVars {
            model: Some("m1"),
            ..ArgvVars::default()
        };
        assert_eq!(subst("{{model}}/{{mystery}}", &vars), "m1/{{mystery}}");
        assert_eq!(subst("{{session_id}}", &vars), "");
        assert_eq!(subst("{{permission_mode}}", &vars), "default");
    }

    #[test]
    fn system_prompt_args_spliced_only_when_present() {
        let def = builtin_claude();
        let vars = ArgvVars {
            system_prompt: Some("be brief"),
            ..ArgvVars::default()
        };
        assert_eq!(
            argv(&def, &vars),
            vec![
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "default",
                "--append-system-prompt",
                "be brief",
            ]
        );
        // Absent system prompt -> no flag at all.
        let plain = argv(&def, &ArgvVars::default());
        assert!(!plain.iter().any(|a| a == "--append-system-prompt"));
    }

    // ---- effort_args (agents' `effort`, spliced only when the engine opts in) ----

    fn effort_engine() -> EngineDef {
        let mut def = builtin_codex();
        def.args = vec!["run".to_string(), "-".to_string()];
        def.permission_args = None;
        def.effort_args = Some(vec!["--reasoning-effort".to_string(), "{{effort}}".to_string()]);
        def
    }

    #[test]
    fn effort_args_are_spliced_when_the_agent_sets_effort() {
        let def = effort_engine();
        let vars = ArgvVars {
            effort: Some("high"),
            ..ArgvVars::default()
        };
        assert_eq!(
            def.assemble_argv(&vars),
            vec!["run", "--reasoning-effort", "high", "-"]
        );
    }

    #[test]
    fn effort_args_are_omitted_when_the_agent_sets_no_effort() {
        let def = effort_engine();
        assert_eq!(def.assemble_argv(&ArgvVars::default()), vec!["run", "-"]);
    }

    #[test]
    fn effort_on_claude_is_silently_ignored() {
        let def = builtin_claude();
        let with = ArgvVars {
            effort: Some("high"),
            live_status: true,
            ..ArgvVars::default()
        };
        let without = ArgvVars {
            live_status: true,
            ..ArgvVars::default()
        };
        assert_eq!(def.assemble_argv(&with), def.assemble_argv(&without));
    }

    #[test]
    fn codex_reasoning_effort_is_a_structured_config_override() {
        let argv = builtin_codex().assemble_argv(&ArgvVars {
            effort: Some("high"),
            ..ArgvVars::default()
        });
        assert!(argv
            .windows(2)
            .any(|pair| { pair == ["-c".to_string(), "model_reasoning_effort=\"high\"".to_string(),] }));
    }
}
