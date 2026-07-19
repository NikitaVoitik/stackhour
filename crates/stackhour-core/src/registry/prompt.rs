//! PromptStore (NEW): named markdown templates with dumb, dependency-free
//! `{{placeholder}}` substitution. Unknown placeholders are left VERBATIM; no
//! logic, no escaping — boring on purpose.
//!
//! Built-in defaults are the exact strings currently hardcoded in
//! coordinator.mjs / worker.mjs (system composition, media-image, media-video
//! guidance, help text, online banner, transcribing notice, status text
//! skeleton). A `prompts/<name>.md` file overrides the built-in of the same
//! name, re-read via an mtime cache.
//!
//! Substitution is a sequential literal `{{key}}` -> value replace in the
//! caller-given var order (the same rule as `EngineDef`'s argv templating):
//! no recursion guard, no escaping, values pasted as-is. A value that itself
//! contains a later var's `{{key}}` WILL be substituted by that later var —
//! deliberate "dumb template" semantics, documented rather than defended.

use indexmap::IndexMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Built-in template bodies.
//
// Every dynamic value the JS interpolated becomes a {{placeholder}}; every
// literal byte around them is kept byte-identical to coordinator.mjs /
// worker.mjs so a built-in render with the right vars reproduces today's
// Telegram strings exactly.
// ---------------------------------------------------------------------------

/// 'system': the composed agent system prompt (souls.rs
/// `compose_system_prompt`): soul text + skill bodies under a Skills heading.
const TPL_SYSTEM: &str = "{{soul}}\n\n## Skills\n{{skills}}";

/// 'agent-turn': how the composed system text reaches engines WITHOUT
/// `system_prompt_args` (e.g. codex) — prepended to the user prompt with a
/// separator. New in the rewrite (JS had no souls), so no legacy bytes to
/// match; the separator is a plain markdown rule.
const TPL_AGENT_TURN: &str = "{{system}}\n\n---\n\n{{prompt}}";

/// 'media-image': the full engine prompt for an image attachment
/// (coordinator.mjs / worker.mjs `mediaPrompt`, kind != 'video' guidance
/// line). `{{request}}` is the trimmed caption or the hardcoded
/// "Please inspect this image and respond." fallback — the fallback stays in
/// the caller because it depends on the caption, not the template.
const TPL_MEDIA_IMAGE: &str = "{{request}}\n\nTelegram attachment ({{kind}}, {{mime}}, {{name}}) is saved locally at: {{path}}\nUse the available image inspection tool to view it.";

/// 'media-video': ditto for video attachments (`mediaPrompt`, kind ==
/// 'video' guidance line).
const TPL_MEDIA_VIDEO: &str = "{{request}}\n\nTelegram attachment ({{kind}}, {{mime}}, {{name}}) is saved locally at: {{path}}\nUse available tools such as ffmpeg/ffprobe to inspect representative frames and audio when useful.";

/// 'help': the HELP constant from coordinator.mjs (HTML, lines joined by
/// '\n'). No placeholders.
const TPL_HELP: &str = "<b>Claude + Codex bridge</b> (distributed)\n\n\u{1f9e0} /claude — use Claude Code\n\u{1f6e0} /codex — use Codex\n\u{1f5a5}\u{fe0f} /mac — run on the Mac\n\u{2601}\u{fe0f} /gcp — run on the GCP box\n\u{2139}\u{fe0f} /where — active engine, target &amp; session\n\u{1f195} /new — fresh session for this engine + target\n\u{23f9} /stop — kill/cancel the running job\n\u{1f39b} /menu — tap-button controls\n\n<i>Anything else → selected engine on the active target.</i>";

/// 'online': the startup banner (coordinator.mjs `poll()`). Vars: engine =
/// engine label ("Claude"/"Codex"), target = target label, worker =
/// "online"/"offline".
const TPL_ONLINE: &str =
    "\u{1f916} Claude + Codex bridge online. Active: {{engine}} on {{target}}. Mac worker: {{worker}}.";

/// 'status': the /where reply skeleton (coordinator.mjs `statusText()`).
/// Vars: engine label, target label, session ("<first8>…" or
/// "none (fresh)"), worker ("online"/"offline"), busy ("yes"/"no").
const TPL_STATUS: &str = "Engine: {{engine}}\nTarget: {{target}}\nSession: {{session}}\nMac worker: {{worker}}\nGCP busy: {{busy}}";

/// 'transcribing': the voice-flow status message (coordinator.mjs
/// `handleVoiceMessage`). No placeholders.
const TPL_TRANSCRIBING: &str = "\u{1f399}\u{fe0f} Transcribing voice message…";

/// (name, body) for every built-in, in [`builtin_names`] order.
fn builtins() -> [(&'static str, &'static str); 8] {
    [
        ("system", TPL_SYSTEM),
        ("agent-turn", TPL_AGENT_TURN),
        ("media-image", TPL_MEDIA_IMAGE),
        ("media-video", TPL_MEDIA_VIDEO),
        ("help", TPL_HELP),
        ("online", TPL_ONLINE),
        ("status", TPL_STATUS),
        ("transcribing", TPL_TRANSCRIBING),
    ]
}

/// The names of every built-in template.
pub fn builtin_names() -> &'static [&'static str] {
    &[
        "system",
        "agent-turn",
        "media-image",
        "media-video",
        "help",
        "online",
        "status",
        "transcribing",
    ]
}

/// Named prompt templates: built-ins + optional on-disk overrides.
#[derive(Debug)]
pub struct PromptStore {
    /// Built-in template bodies by name.
    builtins: IndexMap<String, String>,
    /// `prompts/` directory, when the registry root exists.
    dir: Option<PathBuf>,
    /// mtime cache of on-disk overrides.
    cache: Mutex<IndexMap<String, (SystemTime, String)>>,
}

/// A template name that is safe to map to `prompts/<name>.md`: non-empty, no
/// path separators, not dot-prefixed (editor artifacts / traversal).
fn safe_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains('/') && !name.contains('\\')
}

impl PromptStore {
    /// Build a store over the given prompts dir (None = built-ins only).
    pub fn new(dir: Option<PathBuf>) -> Self {
        let mut map: IndexMap<String, String> = IndexMap::new();
        for (name, body) in builtins() {
            map.insert(name.to_string(), body.to_string());
        }
        PromptStore {
            builtins: map,
            dir,
            cache: Mutex::new(IndexMap::new()),
        }
    }

    /// Render template `name` with `{{var}}` substitution; unknown template
    /// -> "" (the miss is the caller's to notice — `has` is the existence
    /// check); unknown placeholders left verbatim.
    pub fn render(&self, name: &str, vars: &[(&str, &str)]) -> String {
        let Some(body) = self.body(name) else {
            return String::new();
        };
        let mut out = body;
        for (key, value) in vars {
            let needle = format!("{{{{{key}}}}}");
            out = out.replace(&needle, value);
        }
        out
    }

    /// Whether a template of this name exists (built-in or override).
    pub fn has(&self, name: &str) -> bool {
        if self.builtins.contains_key(name) {
            return true;
        }
        match (&self.dir, safe_name(name)) {
            (Some(dir), true) => dir.join(format!("{name}.md")).is_file(),
            _ => false,
        }
    }

    /// The current template body: on-disk override (mtime-cached) when
    /// present and readable, else the built-in, else None.
    fn body(&self, name: &str) -> Option<String> {
        if let Some(text) = self.override_body(name) {
            return Some(text);
        }
        self.builtins.get(name).cloned()
    }

    /// Read `prompts/<name>.md` through the mtime cache. Any failure (missing
    /// dir/file, unreadable, unstattable) -> None, falling back to the
    /// built-in; a vanished file also evicts its cache entry.
    fn override_body(&self, name: &str) -> Option<String> {
        let dir = self.dir.as_ref()?;
        if !safe_name(name) {
            return None;
        }
        let path = dir.join(format!("{name}.md"));
        // Poisoned mutex: another thread panicked mid-insert; the map itself
        // is still structurally valid, so keep serving (never panic here).
        let mut cache = match self.cache.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mtime = match std::fs::metadata(&path).and_then(|md| md.modified()) {
            Ok(t) => t,
            Err(_) => {
                cache.shift_remove(name);
                return None;
            }
        };
        if let Some((cached_mtime, text)) = cache.get(name) {
            if *cached_mtime == mtime {
                return Some(text.clone());
            }
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                cache.insert(name.to_string(), (mtime, text.clone()));
                Some(text)
            }
            // Stat succeeded but read failed (perms, raced deletion): drop
            // any stale cache entry and fall back to the built-in.
            Err(_) => {
                cache.shift_remove(name);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn store_over(dir: &Path) -> PromptStore {
        PromptStore::new(Some(dir.to_path_buf()))
    }

    // ---- built-ins ----

    #[test]
    fn builtin_names_all_exist_and_render_nonempty() {
        let store = PromptStore::new(None);
        for name in builtin_names() {
            assert!(store.has(name), "missing built-in {name}");
            assert!(!store.render(name, &[]).is_empty(), "empty built-in {name}");
        }
        assert_eq!(builtin_names().len(), builtins().len());
    }

    #[test]
    fn system_template_is_the_documented_composition() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render("system", &[("soul", "You are terse."), ("skills", "- deploy")]),
            "You are terse.\n\n## Skills\n- deploy"
        );
    }

    #[test]
    fn agent_turn_prepends_system_with_separator() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render("agent-turn", &[("system", "SYS"), ("prompt", "do it")]),
            "SYS\n\n---\n\ndo it"
        );
    }

    #[test]
    fn media_image_matches_js_media_prompt() {
        let store = PromptStore::new(None);
        let out = store.render(
            "media-image",
            &[
                ("request", "Please inspect this image and respond."),
                ("kind", "image"),
                ("mime", "image/jpeg"),
                ("name", "telegram-photo.jpg"),
                ("path", "/run/media/1-x.jpg"),
            ],
        );
        assert_eq!(
            out,
            "Please inspect this image and respond.\n\nTelegram attachment (image, image/jpeg, telegram-photo.jpg) is saved locally at: /run/media/1-x.jpg\nUse the available image inspection tool to view it."
        );
    }

    #[test]
    fn media_video_matches_js_media_prompt() {
        let store = PromptStore::new(None);
        let out = store.render(
            "media-video",
            &[
                ("request", "caption text"),
                ("kind", "video"),
                ("mime", "video/mp4"),
                ("name", "telegram-video.mp4"),
                ("path", "/m/v.mp4"),
            ],
        );
        assert_eq!(
            out,
            "caption text\n\nTelegram attachment (video, video/mp4, telegram-video.mp4) is saved locally at: /m/v.mp4\nUse available tools such as ffmpeg/ffprobe to inspect representative frames and audio when useful."
        );
    }

    #[test]
    fn help_matches_coordinator_help_constant() {
        // The exact string from coordinator.mjs: HTML lines joined by '\n'.
        let expected = [
            "<b>Claude + Codex bridge</b> (distributed)",
            "",
            "🧠 /claude — use Claude Code",
            "🛠 /codex — use Codex",
            "🖥️ /mac — run on the Mac",
            "☁️ /gcp — run on the GCP box",
            "ℹ️ /where — active engine, target &amp; session",
            "🆕 /new — fresh session for this engine + target",
            "⏹ /stop — kill/cancel the running job",
            "🎛 /menu — tap-button controls",
            "",
            "<i>Anything else → selected engine on the active target.</i>",
        ]
        .join("\n");
        assert_eq!(PromptStore::new(None).render("help", &[]), expected);
    }

    #[test]
    fn online_banner_matches_coordinator() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render(
                "online",
                &[("engine", "Claude"), ("target", "Linux"), ("worker", "offline")]
            ),
            "🤖 Claude + Codex bridge online. Active: Claude on Linux. Mac worker: offline."
        );
    }

    #[test]
    fn status_skeleton_matches_status_text() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render(
                "status",
                &[
                    ("engine", "Codex"),
                    ("target", "Mac"),
                    ("session", "0198ab12…"),
                    ("worker", "online"),
                    ("busy", "no"),
                ]
            ),
            "Engine: Codex\nTarget: Mac\nSession: 0198ab12…\nMac worker: online\nGCP busy: no"
        );
    }

    #[test]
    fn transcribing_notice_matches_coordinator() {
        assert_eq!(
            PromptStore::new(None).render("transcribing", &[]),
            "🎙️ Transcribing voice message…"
        );
    }

    // ---- substitution semantics ----

    #[test]
    fn unknown_placeholders_left_verbatim() {
        let store = PromptStore::new(None);
        let out = store.render("online", &[("engine", "Claude")]);
        assert_eq!(
            out,
            "🤖 Claude + Codex bridge online. Active: Claude on {{target}}. Mac worker: {{worker}}."
        );
    }

    #[test]
    fn no_escaping_values_pasted_as_is() {
        let store = PromptStore::new(None);
        let out = store.render("system", &[("soul", "<b>&{{x}}"), ("skills", "")]);
        assert_eq!(out, "<b>&{{x}}\n\n## Skills\n");
    }

    #[test]
    fn substitution_is_sequential_in_var_order() {
        // Dumb-template documented behaviour: a value containing a LATER
        // var's placeholder gets substituted by that later var.
        let store = PromptStore::new(None);
        let out = store.render("system", &[("soul", "{{skills}}!"), ("skills", "K")]);
        assert_eq!(out, "K!\n\n## Skills\nK");
    }

    #[test]
    fn unknown_template_renders_empty() {
        let store = PromptStore::new(None);
        assert_eq!(store.render("nope", &[("a", "b")]), "");
        assert!(!store.has("nope"));
    }

    // ---- overrides + mtime cache ----

    #[test]
    fn override_file_wins_over_builtin() {
        let dir = tmpdir();
        fs::write(dir.path().join("help.md"), "custom help {{who}}").unwrap();
        let store = store_over(dir.path());
        assert_eq!(store.render("help", &[("who", "nikita")]), "custom help nikita");
        assert!(store.has("help"));
    }

    #[test]
    fn override_only_template_exists_and_renders() {
        let dir = tmpdir();
        fs::write(dir.path().join("deploy.md"), "ship {{args}} now").unwrap();
        let store = store_over(dir.path());
        assert!(store.has("deploy"));
        assert_eq!(store.render("deploy", &[("args", "v2")]), "ship v2 now");
    }

    #[test]
    fn missing_dir_or_file_falls_back_to_builtin() {
        let dir = tmpdir();
        let store = store_over(&dir.path().join("prompts")); // dir absent
        assert_eq!(
            store.render("transcribing", &[]),
            "🎙️ Transcribing voice message…"
        );
        assert!(store.has("transcribing"));
        assert!(!store.has("deploy"));
    }

    #[test]
    fn deleted_override_reverts_to_builtin_and_evicts_cache() {
        let dir = tmpdir();
        let path = dir.path().join("transcribing.md");
        fs::write(&path, "OVERRIDDEN").unwrap();
        let store = store_over(dir.path());
        assert_eq!(store.render("transcribing", &[]), "OVERRIDDEN");
        fs::remove_file(&path).unwrap();
        assert_eq!(
            store.render("transcribing", &[]),
            "🎙️ Transcribing voice message…"
        );
        assert!(store.cache.lock().unwrap().get("transcribing").is_none());
    }

    #[test]
    fn matching_mtime_serves_cached_text() {
        let dir = tmpdir();
        let path = dir.path().join("status.md");
        fs::write(&path, "DISK").unwrap();
        let store = store_over(dir.path());
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        store
            .cache
            .lock()
            .unwrap()
            .insert("status".to_string(), (mtime, "CACHED".to_string()));
        // Same mtime -> the cache wins, the file is not re-read.
        assert_eq!(store.render("status", &[]), "CACHED");
    }

    #[test]
    fn changed_mtime_triggers_reread() {
        let dir = tmpdir();
        let path = dir.path().join("status.md");
        fs::write(&path, "FRESH").unwrap();
        let store = store_over(dir.path());
        let stale = UNIX_EPOCH + Duration::from_secs(1);
        store
            .cache
            .lock()
            .unwrap()
            .insert("status".to_string(), (stale, "STALE".to_string()));
        assert_eq!(store.render("status", &[]), "FRESH");
        // And the cache now holds the fresh copy under the real mtime.
        let real = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            store.cache.lock().unwrap().get("status"),
            Some(&(real, "FRESH".to_string()))
        );
    }

    // ---- name hygiene ----

    #[test]
    fn unsafe_names_never_touch_disk() {
        let dir = tmpdir();
        // A file OUTSIDE the prompts dir that a traversal name would reach.
        fs::write(dir.path().join("evil.md"), "EVIL").unwrap();
        let prompts = dir.path().join("prompts");
        fs::create_dir(&prompts).unwrap();
        let store = store_over(&prompts);
        assert!(!store.has("../evil"));
        assert_eq!(store.render("../evil", &[]), "");
        assert!(!store.has(".hidden"));
        assert!(!store.has(""));
        assert!(!store.has("a/b"));
        assert!(!store.has("a\\b"));
    }
}
