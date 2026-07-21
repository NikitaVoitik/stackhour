//! The shipped prompt-template catalogue: a flat, reviewable table of
//! `(name, body, placeholders)`.
//!
//! This module is DATA. [`super::prompt`] is the mechanism (lookup, on-disk
//! override, mtime cache, substitution) and must stay free of literal user
//! facing text; every byte a user can read in Telegram lives here, so a
//! reviewer can audit the bridge's whole vocabulary in one file and a user
//! can override any single line of it with `prompts/<name>.md`.
//!
//! # Byte parity
//!
//! Every body is the string the Node bridge emits today, copied literally out
//! of `src/bridge/coordinator.mjs` / `worker.mjs`, with each interpolated
//! value replaced by a `{{placeholder}}` and every surrounding byte kept
//! exactly as it was. Rendering a built-in with the values the JS computed
//! therefore reproduces today's message exactly — that is the backward
//! compatibility gate for this pillar, and it is asserted per template in the
//! tests of [`super::prompt`].
//!
//! # Placeholders are documented, not inferred
//!
//! Each entry carries a [`Placeholder`] list: key, what it means, and whether
//! dropping it loses information. The list is authored, not parsed out of the
//! body, because it is a contract with callers (which values they must pass)
//! as well as with users (which values they may use). A `required = true`
//! placeholder that a user override omits is a silent information loss —
//! `PromptStore::missing_required` exists so `bridge doctor` can warn about
//! exactly that. `required = false` means the value is decorative or has a
//! sensible reading when absent.
//!
//! Callers may always pass MORE vars than are documented; unknown
//! placeholders in a body are left verbatim, so a user override may also use
//! any documented key of that template and no other.

/// One documented substitution slot of a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placeholder {
    /// The key, without braces: `"engine"` matches `{{engine}}`.
    pub key: &'static str,
    /// What the value means, phrased for a user editing the override.
    pub description: &'static str,
    /// Whether omitting it from an override loses information the user
    /// would otherwise see.
    pub required: bool,
}

/// A shipped template: its name, its default body, and its documented slots.
#[derive(Debug, Clone, Copy)]
pub struct PromptDefault {
    /// Template name; also the override file stem (`prompts/<name>.md`).
    pub name: &'static str,
    /// The shipped body, used whenever no readable override exists.
    pub body: &'static str,
    /// Documented placeholders, in the order they first appear in the body.
    pub placeholders: &'static [Placeholder],
}

/// Terse constructor so the table below stays scannable.
const fn ph(key: &'static str, description: &'static str, required: bool) -> Placeholder {
    Placeholder {
        key,
        description,
        required,
    }
}

const NONE: &[Placeholder] = &[];

// ---------------------------------------------------------------------------
// Placeholder groups shared by several templates.
// ---------------------------------------------------------------------------

/// The engine label ("Claude" / "Codex"), as `engineLabel()` computes it.
const P_ENGINE: Placeholder = ph("engine", "engine label, e.g. Claude or Codex", true);
/// The target label (`targets[name].label`), e.g. "Mac" / "GCP".
const P_TARGET: Placeholder = ph("target", "target label, e.g. Mac or GCP", true);

/// engine + target, the pair most status strings carry.
const P_WHERE: &[Placeholder] = &[P_ENGINE, P_TARGET];

// ---------------------------------------------------------------------------
// The catalogue.
//
// Grouped by lane, in the order a message is likely to be produced:
// composition -> status -> results -> controls -> media/voice -> errors.
// `builtin_names()` reports this order, so /help-adjacent listings are stable.
// ---------------------------------------------------------------------------

/// Every shipped template. The single source of truth for names, bodies and
/// documented placeholders.
pub const DEFAULTS: &[PromptDefault] = &[
    // ---- composition (agent turns; new in the rewrite) ----
    PromptDefault {
        // coordinator.mjs `ORWELL_RULES` — the standing style rules the JS
        // bridge hands to EVERY default-path turn (claude via
        // `--append-system-prompt`, codex prepended to the prompt). Hardcoded
        // there, a template here: this is the registry's system-prompt
        // concept, so a user can reword or empty it without touching code.
        // An empty body disables the house rules entirely.
        name: "house-rules",
        body: "Follow Orwell's writing rules in every reply:\n1. Never use a metaphor, simile, or other figure of speech you are used to seeing in print.\n2. Never use a long word where a short one will do.\n3. If it is possible to cut a word out, always cut it out.\n4. Never use the passive where the active will do.\n5. Never use a foreign phrase, a scientific word, or jargon where plain English will do.\n6. Break any of these rules sooner than say anything outright barbarous.",
        placeholders: NONE,
    },
    PromptDefault {
        // How the house rules reach an engine with no `system_prompt_args`
        // (codex): prepended to the user prompt, exactly as coordinator.mjs
        // does with its '[Standing style rules]\n' header. Applied only on a
        // FRESH attempt — a resumed thread already carries them.
        name: "house-rules-turn",
        body: "[Standing style rules]\n{{system}}\n\n{{prompt}}",
        placeholders: &[
            ph("system", "the rendered `house-rules` template", true),
            ph("prompt", "the user's text for this turn", true),
        ],
    },
    PromptDefault {
        name: "system",
        body: "{{soul}}\n\n## Skills\n{{skills}}",
        placeholders: &[
            ph("soul", "the agent's composed soul document", true),
            ph("skills", "the enabled skill bodies, already concatenated", true),
        ],
    },
    PromptDefault {
        name: "agent-turn",
        // How composed system text reaches an engine that has no
        // `system_prompt_args` (codex): prepended to the user prompt.
        body: "{{system}}\n\n---\n\n{{prompt}}",
        placeholders: &[
            ph("system", "the rendered `system` template", true),
            ph("prompt", "the user's text for this turn", true),
        ],
    },
    // ---- status line while a job runs ----
    PromptDefault {
        // coordinator.mjs `showStatus` / `dispatchMac`.
        name: "status-line",
        body: "▹ {{engine}} · {{target}} · {{activity}}",
        placeholders: &[
            P_ENGINE,
            P_TARGET,
            ph(
                "activity",
                "current activity, e.g. `working…` or a tool line",
                true,
            ),
        ],
    },
    PromptDefault {
        // The default `activity` value before any tool event arrives.
        name: "status-working",
        body: "working…",
        placeholders: NONE,
    },
    PromptDefault {
        // The `activity` value when a Mac job is queued and the Mac is asleep.
        name: "status-queued",
        body: "queued (Mac offline — runs when it wakes)",
        placeholders: NONE,
    },
    PromptDefault {
        // `activityLine()` — a Claude tool_use event.
        name: "activity-tool",
        body: "⚙️ {{name}}{{detail}}",
        placeholders: &[
            ph("name", "tool name, e.g. Bash or Read", true),
            ph(
                "detail",
                "`: ` plus the truncated command/path/pattern, or empty",
                false,
            ),
        ],
    },
    PromptDefault {
        // /where and the `where` callback (`statusText()`).
        name: "status",
        body: "Engine: {{engine}}\nTarget: {{target}}\nSession: {{session}}\nMac worker: {{worker}}\nGCP busy: {{busy}}",
        placeholders: &[
            P_ENGINE,
            P_TARGET,
            ph("session", "first 8 chars + `…`, or `none (fresh)`", true),
            ph("worker", "`online` or `offline`", true),
            ph("busy", "`yes` or `no` — whether the local lane is running", true),
        ],
    },
    PromptDefault {
        name: "session-none",
        body: "none (fresh)",
        placeholders: NONE,
    },
    // ---- final result delivery ----
    PromptDefault {
        // drainLocal / pollResults: the answer plus its provenance footer.
        // Keeping the whole composition in one template is what lets a user
        // move, reword or delete the footer.
        name: "final",
        body: "{{text}}\n\n— {{engine}} · {{target}} · {{duration}}",
        placeholders: &[
            ph("text", "the engine's answer, or an error line", true),
            P_ENGINE,
            P_TARGET,
            ph("duration", "elapsed wall time, e.g. `42s` or `3m07s`", false),
        ],
    },
    PromptDefault {
        // `(no output)` — engine exited 0 having said nothing.
        name: "no-output",
        body: "(no output)",
        placeholders: NONE,
    },
    PromptDefault {
        // deliverFinal's HTML fallback when the chunked body is empty.
        name: "empty-chunk",
        body: "…",
        placeholders: NONE,
    },
    // ---- controls ----
    PromptDefault {
        // The frozen legacy HELP blob. Telegram HTML.
        //
        // It deliberately has NO {{commands}} in the shipped body: with no
        // config at all `/help` must be byte-identical to the JS bridge. An
        // override that DOES use {{commands}} gets the generated table
        // instead — see `stackhour_bridge::commands::help_text`.
        //
        // This body is the LEGACY-ROSTER reference rendering. The store
        // regenerates the per-target lines from whatever roster it is built
        // with (`PromptStore::new_with_targets`, via [`HELP_HEAD`] +
        // [`HELP_TAIL`]); a test pins that the legacy roster reproduces this
        // exact string.
        name: "help",
        body: "<b>Claude + Codex bridge</b> (distributed)\n\n🧠 /claude — use Claude Code\n🛠 /codex — use Codex\n🖥️ /mac — run on the Mac\n☁️ /gcp — run on the GCP box\n🚀 /ship — ship a Blort task (Notion→PR)\nℹ️ /where — active engine, target &amp; session\n🆕 /new — fresh session for this engine + target\n⏹ /stop — kill/cancel the running job\n🎛 /menu — tap-button controls\n\n<i>Anything else → selected engine on the active target.</i>",
        placeholders: &[
            ph(
                "commands",
                "generated `/cmd — description` lines, one per visible command; \
                 absent from the shipped body, honoured in an override",
                false,
            ),
            ph("engine", "engine label", false),
            ph("target", "target label", false),
        ],
    },
    PromptDefault {
        // /menu
        name: "menu",
        body: "🎛 Controls — {{engine}} on {{target}}",
        placeholders: P_WHERE,
    },
    PromptDefault {
        // The startup banner (`poll()`).
        name: "online",
        body: "🤖 Claude + Codex bridge online. Active: {{engine}} on {{target}}. Mac worker: {{worker}}.",
        placeholders: &[
            P_ENGINE,
            P_TARGET,
            ph("worker", "`online` or `offline`", true),
        ],
    },
    PromptDefault {
        // switchEngine()
        name: "switch-engine",
        body: "Switched to {{engine}} on {{target}}. {{session}}",
        placeholders: &[
            P_ENGINE,
            P_TARGET,
            ph(
                "session",
                "the rendered `session-resuming` or `session-new` template",
                false,
            ),
        ],
    },
    PromptDefault {
        // switchTarget() — note the reversed order and `with`, not `on`.
        name: "switch-target",
        body: "Switched to {{target}} with {{engine}}. {{session}}",
        placeholders: &[
            P_TARGET,
            P_ENGINE,
            ph(
                "session",
                "the rendered `session-resuming` or `session-new` template",
                false,
            ),
        ],
    },
    PromptDefault {
        name: "session-resuming",
        body: "(resuming session)",
        placeholders: NONE,
    },
    PromptDefault {
        name: "session-new",
        body: "(new session)",
        placeholders: NONE,
    },
    PromptDefault {
        // Bare `/ship`, with no task: the coordinator has ALREADY switched to
        // the ship target/engine by the time this is sent (the JS mutates
        // state before it checks for a task, and there is no /unship).
        //
        // The body is the JS literal, not a rendering of {{engine}}/{{target}}:
        // it says "Claude on the Blort repo" where the labels would say
        // "Claude on 🚀 Blort". Both placeholders are offered to an override.
        name: "ship-empty",
        body: "🚀 Ship mode: Claude on the Blort repo. Send the task (text, ECM-xxxx, or a Slack link).",
        placeholders: &[
            ph("engine", "engine label", false),
            ph("target", "target label", false),
        ],
    },
    PromptDefault {
        // `/ship <task>` re-enters the prompt lane with the command word
        // still attached, so the engine sees the whole instruction.
        name: "ship-prompt",
        body: "/ship {{task}}",
        placeholders: &[ph("task", "the task text, exactly as typed", true)],
    },
    PromptDefault {
        // /new and /reset
        name: "session-reset",
        body: "🆕 Fresh {{engine}} session on {{target}}.",
        placeholders: P_WHERE,
    },
    PromptDefault {
        // /stop, local lane killed.
        name: "stop-local",
        body: "🛑 Stopped GCP job.",
        placeholders: NONE,
    },
    PromptDefault {
        // /stop, queued Mac jobs removed before the worker claimed them.
        name: "stop-cancelled",
        body: "🛑 Cancelled {{count}} queued Mac job(s).",
        placeholders: &[ph("count", "how many queued jobs were removed", true)],
    },
    PromptDefault {
        // /stop, jobs the worker already claimed — cannot be interrupted.
        name: "stop-claimed",
        body: "⚠️ {{count}} Mac job(s) already running — can't interrupt remotely yet.",
        placeholders: &[ph("count", "how many claimed jobs are still running", true)],
    },
    PromptDefault {
        // /stop with nothing to stop.
        name: "stop-idle",
        body: "Nothing running.",
        placeholders: NONE,
    },
    PromptDefault {
        // The status message of a job cancelled while queued.
        name: "cancelled",
        body: "🛑 Cancelled.",
        placeholders: NONE,
    },
    PromptDefault {
        // A `/slash` that matched nothing. `{{help}}` is the rendered `help`.
        name: "unknown-command",
        body: "Unknown command.\n\n{{help}}",
        placeholders: &[ph("help", "the rendered `help` template", true)],
    },
    // ---- callback-query toasts (answerCallbackQuery text) ----
    PromptDefault {
        name: "toast-engine",
        body: "Using {{engine}}",
        placeholders: &[ph(
            "engine",
            "long engine name, e.g. `Claude Code` or `Codex`",
            true,
        )],
    },
    PromptDefault {
        name: "toast-target",
        body: "On the {{target}}",
        placeholders: &[ph(
            "target",
            "target phrase, e.g. `Mac 🖥️` or `GCP box ☁️`",
            true,
        )],
    },
    PromptDefault {
        name: "toast-new",
        body: "Fresh session",
        placeholders: NONE,
    },
    PromptDefault {
        name: "toast-stop",
        body: "Stopping…",
        placeholders: NONE,
    },
    // ---- media + voice ----
    PromptDefault {
        // `mediaPrompt`, image branch: the full engine prompt.
        name: "media-image",
        body: "{{request}}\n\nTelegram attachment ({{kind}}, {{mime}}, {{name}}) is saved locally at: {{path}}\nUse the available image inspection tool to view it.",
        placeholders: MEDIA_PLACEHOLDERS,
    },
    PromptDefault {
        // `mediaPrompt`, video branch.
        name: "media-video",
        body: "{{request}}\n\nTelegram attachment ({{kind}}, {{mime}}, {{name}}) is saved locally at: {{path}}\nUse available tools such as ffmpeg/ffprobe to inspect representative frames and audio when useful.",
        placeholders: MEDIA_PLACEHOLDERS,
    },
    PromptDefault {
        // The `request` value when an image arrives with no caption.
        name: "media-request-image",
        body: "Please inspect this image and respond.",
        placeholders: NONE,
    },
    PromptDefault {
        // The `request` value when a video arrives with no caption.
        name: "media-request-video",
        body: "Please inspect this video and respond.",
        placeholders: NONE,
    },
    PromptDefault {
        // handleVoiceMessage: the placeholder message while ElevenLabs runs.
        name: "transcribing",
        body: "🎙️ Transcribing voice message…",
        placeholders: NONE,
    },
    PromptDefault {
        // The transcript echoed back before routing it as a prompt.
        name: "transcript",
        body: "🎙️ Transcript:\n{{transcript}}",
        placeholders: &[ph(
            "transcript",
            "the transcript, truncated to 3400 chars + `…`",
            true,
        )],
    },
    // ---- errors, in the order they can occur ----
    PromptDefault {
        // Attachment rejected on its declared size, before downloading.
        name: "error-media-size",
        body: "Attachment is too large ({{size}} MB; limit {{limit}} MB).",
        placeholders: &[
            ph("size", "the attachment size in whole MB, rounded up", true),
            ph("limit", "the configured limit in whole MB", true),
        ],
    },
    PromptDefault {
        // Attachment rejected on its Content-Length mid-download.
        name: "error-media-limit",
        body: "Attachment exceeds the {{limit}} MB limit.",
        placeholders: &[ph("limit", "the configured limit in whole MB", true)],
    },
    PromptDefault {
        // handleMediaMessage catch.
        name: "error-media",
        body: "⚠️ Could not process attachment: {{error}}",
        placeholders: &[P_ERROR],
    },
    PromptDefault {
        // handleVoiceMessage catch.
        name: "error-transcribe",
        body: "⚠️ Could not transcribe voice message: {{error}}",
        placeholders: &[P_ERROR],
    },
    PromptDefault {
        // Engine failed to spawn / errored out on the local lane.
        name: "error-run",
        body: "⚠️ Error: {{error}}",
        placeholders: &[P_ERROR],
    },
    PromptDefault {
        // Engine exited non-zero locally; the tail of stderr is included.
        name: "error-exit",
        body: "⚠️ {{engine}} exited (code {{code}}).\n{{stderr}}",
        placeholders: &[
            P_ENGINE,
            ph("code", "the process exit code", true),
            ph("stderr", "the last 500 bytes of stderr", false),
        ],
    },
    PromptDefault {
        // The worker reported an error rather than a result.
        name: "error-mac",
        body: "⚠️ Mac error: {{error}}",
        placeholders: &[P_ERROR],
    },
    PromptDefault {
        // The worker reported a non-zero exit. No stderr crosses the SSH hop.
        name: "error-exit-mac",
        body: "⚠️ {{engine}} exited on Mac (code {{code}}).",
        placeholders: &[P_ENGINE, ph("code", "the process exit code", true)],
    },
    PromptDefault {
        // drainLocal's outer catch: an unexpected coordinator-side failure,
        // edited into the status message.
        name: "error-generic",
        body: "⚠️ {{error}}",
        placeholders: &[P_ERROR],
    },
];

/// The `{{error}}` slot every error template shares.
const P_ERROR: Placeholder = ph("error", "the failure message, unescaped", true);

/// The `help` body above the generated per-target lines. Byte-identical to
/// the corresponding slice of the frozen legacy blob in [`DEFAULTS`].
pub const HELP_HEAD: &str =
    "<b>Claude + Codex bridge</b> (distributed)\n\n🧠 /claude — use Claude Code\n🛠 /codex — use Codex\n";

/// The `help` body below the generated per-target lines. Byte-identical to
/// the corresponding slice of the frozen legacy blob in [`DEFAULTS`].
pub const HELP_TAIL: &str = "🚀 /ship — ship a Blort task (Notion→PR)\nℹ️ /where — active engine, target &amp; session\n🆕 /new — fresh session for this engine + target\n⏹ /stop — kill/cancel the running job\n🎛 /menu — tap-button controls\n\n<i>Anything else → selected engine on the active target.</i>";

/// The four slots `mediaPrompt` fills, shared by both media templates.
const MEDIA_PLACEHOLDERS: &[Placeholder] = &[
    ph(
        "request",
        "the caption, or the rendered `media-request-image` / `media-request-video`",
        true,
    ),
    ph("kind", "`image` or `video`", false),
    ph("mime", "the attachment MIME type", false),
    ph("name", "the attachment file name", false),
    ph(
        "path",
        "absolute path of the downloaded file on the engine's box",
        true,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn names_are_unique_and_file_safe() {
        let mut seen = HashSet::new();
        for d in DEFAULTS {
            assert!(seen.insert(d.name), "duplicate template name {}", d.name);
            assert!(!d.name.is_empty());
            assert!(
                d.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{} is not a safe prompts/<name>.md stem",
                d.name
            );
        }
    }

    #[test]
    fn bodies_are_never_empty() {
        for d in DEFAULTS {
            assert!(!d.body.is_empty(), "{} has an empty body", d.name);
        }
    }

    /// The documentation must describe the body, and only the body: every
    /// documented key must occur in it, and every `{{key}}` in it must be
    /// documented. This is what makes `placeholders()` trustworthy enough for
    /// `bridge doctor` to lint overrides against.
    #[test]
    fn documented_placeholders_match_the_body_exactly() {
        for d in DEFAULTS {
            for p in d.placeholders {
                if p.required {
                    assert!(
                        d.body.contains(&format!("{{{{{}}}}}", p.key)),
                        "{}: documents required {{{{{}}}}} which the body does not use",
                        d.name,
                        p.key
                    );
                }
            }
            for key in scan_keys(d.body) {
                assert!(
                    d.placeholders.iter().any(|p| p.key == key),
                    "{}: body uses undocumented {{{{{key}}}}}",
                    d.name
                );
            }
        }
    }

    #[test]
    fn placeholder_docs_are_written() {
        for d in DEFAULTS {
            for p in d.placeholders {
                assert!(
                    p.description.len() > 5,
                    "{}: {} has no useful description",
                    d.name,
                    p.key
                );
            }
        }
    }

    /// Extract every `{{key}}` occurring in a body.
    fn scan_keys(body: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(open) = rest.find("{{") {
            rest = &rest[open + 2..];
            let Some(close) = rest.find("}}") else { break };
            let key = &rest[..close];
            if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                out.push(key.to_string());
            }
            rest = &rest[close + 2..];
        }
        out
    }
}
