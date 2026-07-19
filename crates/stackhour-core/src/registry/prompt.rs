//! `PromptStore`: named markdown templates with dumb, dependency-free
//! `{{placeholder}}` substitution. Unknown placeholders are left VERBATIM; no
//! logic, no escaping — boring on purpose.
//!
//! This file is the MECHANISM: lookup, on-disk override, mtime cache,
//! substitution, and the placeholder lint. Every literal user-facing byte
//! lives in the sibling table [`prompt_defaults::DEFAULTS`], so prompt.rs can
//! be reviewed as code and prompt_defaults.rs as copy.
//!
//! # What a template is
//!
//! Every string the bridge can send to Telegram, and every prompt it composes
//! for an engine, is a named template. A `prompts/<name>.md` file in the
//! config dir replaces the shipped body of that name; a file whose stem
//! matches NO built-in defines a brand new template, which commands
//! (`kind = "prompt"`) and skills (`template = `) may reference by name.
//! Both cases are hot: the file is re-stat'd on every render and re-read when
//! its mtime moves, so editing a prompt takes effect on the next message with
//! no reload and no restart.
//!
//! # Substitution semantics
//!
//! A sequential literal `{{key}}` -> value replace in the caller-given var
//! order (the same rule as [`super::engine::EngineDef`]'s argv templating):
//! no recursion guard, no escaping, values pasted as-is. A value that itself
//! contains a later var's `{{key}}` WILL be substituted by that later var —
//! deliberate "dumb template" semantics, documented rather than defended.
//! Callers that interpolate untrusted text into an HTML-parsed message must
//! escape it themselves before handing it over.
//!
//! # Backward compatibility
//!
//! With no config directory the store is built-ins only and every render is
//! byte-identical to the Node bridge. That is asserted template by template
//! in the tests at the bottom of this file.

use indexmap::IndexMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

#[path = "prompt_defaults.rs"]
pub mod prompt_defaults;

pub use prompt_defaults::{Placeholder, PromptDefault, DEFAULTS};

/// The names of every built-in template, in catalogue order.
pub fn builtin_names() -> &'static [&'static str] {
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| DEFAULTS.iter().map(|d| d.name).collect())
}

/// The documented placeholders of a built-in template; `&[]` for an unknown
/// name or for a purely user-defined template (whose slots only its author
/// knows).
pub fn placeholders(name: &str) -> &'static [Placeholder] {
    DEFAULTS
        .iter()
        .find(|d| d.name == name)
        .map(|d| d.placeholders)
        .unwrap_or(&[])
}

/// Named prompt templates: built-ins + optional on-disk overrides.
#[derive(Debug)]
pub struct PromptStore {
    /// Built-in template bodies by name, in catalogue order.
    builtins: IndexMap<String, &'static str>,
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
        let mut map: IndexMap<String, &'static str> = IndexMap::new();
        for d in DEFAULTS {
            map.insert(d.name.to_string(), d.body);
        }
        PromptStore {
            builtins: map,
            dir,
            cache: Mutex::new(IndexMap::new()),
        }
    }

    /// Render template `name` with `{{var}}` substitution.
    ///
    /// An unknown template renders as `""` — the miss is the caller's to
    /// notice, and [`has`](Self::has) is the existence check that cross
    /// reference validation uses, so an unknown name never reaches here in
    /// validated config. Unknown placeholders are left verbatim.
    pub fn render(&self, name: &str, vars: &[(&str, &str)]) -> String {
        self.try_render(name, vars).unwrap_or_default()
    }

    /// [`render`](Self::render), distinguishing "template missing" (`None`)
    /// from "template rendered to nothing" (`Some("")`).
    pub fn try_render(&self, name: &str, vars: &[(&str, &str)]) -> Option<String> {
        let mut out = self.body(name)?;
        for (key, value) in vars {
            let needle = format!("{{{{{key}}}}}");
            out = out.replace(&needle, value);
        }
        Some(out)
    }

    /// Whether a template of this name exists — built-in, override of a
    /// built-in, or a purely user-defined `prompts/<name>.md`. This is the
    /// predicate the loader cross-references command `template` and skill
    /// `template` against.
    pub fn has(&self, name: &str) -> bool {
        if self.builtins.contains_key(name) {
            return true;
        }
        match (&self.dir, safe_name(name)) {
            (Some(dir), true) => dir.join(format!("{name}.md")).is_file(),
            _ => false,
        }
    }

    /// The template body currently in force: the on-disk override
    /// (mtime-cached) when present and readable, else the shipped body, else
    /// `None`.
    pub fn body(&self, name: &str) -> Option<String> {
        if let Some(text) = self.override_body(name) {
            return Some(text);
        }
        self.builtins.get(name).map(|s| s.to_string())
    }

    /// Whether this name is currently served by a file rather than the
    /// shipped body. Used by `bridge doctor` to report what a user overrode.
    pub fn is_overridden(&self, name: &str) -> bool {
        self.override_body(name).is_some()
    }

    /// Required placeholders that the body currently in force does NOT
    /// contain — i.e. information the user's override silently drops.
    ///
    /// Empty for a built-in with no override (the shipped bodies are checked
    /// against their own documentation by a test), empty for a purely
    /// user-defined template (nothing is documented, so nothing is required),
    /// and empty for an unknown name.
    pub fn missing_required(&self, name: &str) -> Vec<&'static str> {
        let Some(body) = self.body(name) else {
            return Vec::new();
        };
        placeholders(name)
            .iter()
            .filter(|p| p.required && !body.contains(&format!("{{{{{}}}}}", p.key)))
            .map(|p| p.key)
            .collect()
    }

    /// Every override that drops a required placeholder, as
    /// `(name, missing keys)`, in catalogue order. One call gives
    /// `bridge doctor` its whole prompt report.
    pub fn lint(&self) -> Vec<(&'static str, Vec<&'static str>)> {
        DEFAULTS
            .iter()
            .filter_map(|d| {
                let missing = self.missing_required(d.name);
                (!missing.is_empty()).then_some((d.name, missing))
            })
            .collect()
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

    // ---- catalogue invariants ----

    #[test]
    fn builtin_names_all_exist_and_render_nonempty() {
        let store = PromptStore::new(None);
        for name in builtin_names() {
            assert!(store.has(name), "missing built-in {name}");
            assert!(!store.render(name, &[]).is_empty(), "empty built-in {name}");
        }
        assert_eq!(builtin_names().len(), DEFAULTS.len());
    }

    #[test]
    fn builtin_names_are_in_catalogue_order() {
        let names: Vec<&str> = DEFAULTS.iter().map(|d| d.name).collect();
        assert_eq!(builtin_names(), names.as_slice());
        // And the store iterates in the same order, so anything listing
        // templates is stable across runs.
        let store = PromptStore::new(None);
        let keys: Vec<&str> = store.builtins.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, names);
    }

    #[test]
    fn shipped_bodies_never_drop_their_own_required_placeholders() {
        let store = PromptStore::new(None);
        assert_eq!(store.lint(), Vec::new());
    }

    #[test]
    fn placeholders_are_exposed_for_builtins_only() {
        assert!(placeholders("status").iter().any(|p| p.key == "session"));
        assert!(placeholders("stop-idle").is_empty());
        assert!(placeholders("no-such-template").is_empty());
    }

    // ---- byte parity with the JS bridge, template by template ----

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
    fn media_caption_fallbacks_match_js() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render("media-request-image", &[]),
            "Please inspect this image and respond."
        );
        assert_eq!(
            store.render("media-request-video", &[]),
            "Please inspect this video and respond."
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

    /// `{{commands}}` is documented but deliberately absent from the shipped
    /// body: passing it must not perturb the legacy bytes.
    #[test]
    fn help_ignores_the_commands_var_unless_an_override_uses_it() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render("help", &[("commands", "/x — y")]),
            store.render("help", &[])
        );
        assert!(placeholders("help").iter().any(|p| p.key == "commands"));
        assert!(placeholders("help")
            .iter()
            .all(|p| !p.required || p.key != "commands"));
    }

    #[test]
    fn unknown_command_wraps_the_help_body() {
        let store = PromptStore::new(None);
        let help = store.render("help", &[]);
        assert_eq!(
            store.render("unknown-command", &[("help", &help)]),
            format!("Unknown command.\n\n{help}")
        );
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
        assert_eq!(store.render("session-none", &[]), "none (fresh)");
    }

    #[test]
    fn status_line_matches_show_status() {
        let store = PromptStore::new(None);
        let activity = store.render("status-working", &[]);
        assert_eq!(
            store.render(
                "status-line",
                &[("engine", "Claude"), ("target", "GCP"), ("activity", &activity)]
            ),
            "▹ Claude · GCP · working…"
        );
        assert_eq!(
            store.render(
                "status-line",
                &[
                    ("engine", "Claude"),
                    ("target", "Mac"),
                    ("activity", &store.render("status-queued", &[])),
                ]
            ),
            "▹ Claude · Mac · queued (Mac offline — runs when it wakes)"
        );
    }

    #[test]
    fn activity_tool_matches_activity_line() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render("activity-tool", &[("name", "Bash"), ("detail", ": ls -la")]),
            "⚙️ Bash: ls -la"
        );
        assert_eq!(
            store.render("activity-tool", &[("name", "Plan"), ("detail", "")]),
            "⚙️ Plan"
        );
    }

    #[test]
    fn final_footer_matches_deliver_final() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render(
                "final",
                &[
                    ("text", "all done"),
                    ("engine", "Codex"),
                    ("target", "Mac"),
                    ("duration", "3m07s"),
                ]
            ),
            "all done\n\n— Codex · Mac · 3m07s"
        );
        assert_eq!(store.render("no-output", &[]), "(no output)");
        assert_eq!(store.render("empty-chunk", &[]), "…");
    }

    #[test]
    fn switch_replies_match_coordinator() {
        let store = PromptStore::new(None);
        let resuming = store.render("session-resuming", &[]);
        let fresh = store.render("session-new", &[]);
        assert_eq!(resuming, "(resuming session)");
        assert_eq!(fresh, "(new session)");
        assert_eq!(
            store.render(
                "switch-engine",
                &[("engine", "Codex"), ("target", "Mac"), ("session", &resuming)]
            ),
            "Switched to Codex on Mac. (resuming session)"
        );
        // switchTarget names the TARGET first and says "with", not "on".
        assert_eq!(
            store.render(
                "switch-target",
                &[("target", "Mac"), ("engine", "Claude"), ("session", &fresh)]
            ),
            "Switched to Mac with Claude. (new session)"
        );
        assert_eq!(
            store.render("session-reset", &[("engine", "Claude"), ("target", "GCP")]),
            "🆕 Fresh Claude session on GCP."
        );
        assert_eq!(
            store.render("menu", &[("engine", "Claude"), ("target", "GCP")]),
            "🎛 Controls — Claude on GCP"
        );
    }

    #[test]
    fn stop_replies_match_coordinator() {
        let store = PromptStore::new(None);
        assert_eq!(store.render("stop-local", &[]), "🛑 Stopped GCP job.");
        assert_eq!(
            store.render("stop-cancelled", &[("count", "2")]),
            "🛑 Cancelled 2 queued Mac job(s)."
        );
        assert_eq!(
            store.render("stop-claimed", &[("count", "1")]),
            "⚠️ 1 Mac job(s) already running — can't interrupt remotely yet."
        );
        assert_eq!(store.render("stop-idle", &[]), "Nothing running.");
        assert_eq!(store.render("cancelled", &[]), "🛑 Cancelled.");
    }

    #[test]
    fn callback_toasts_match_coordinator() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render("toast-engine", &[("engine", "Claude Code")]),
            "Using Claude Code"
        );
        assert_eq!(
            store.render("toast-target", &[("target", "Mac 🖥️")]),
            "On the Mac 🖥️"
        );
        assert_eq!(
            store.render("toast-target", &[("target", "GCP box ☁️")]),
            "On the GCP box ☁️"
        );
        assert_eq!(store.render("toast-new", &[]), "Fresh session");
        assert_eq!(store.render("toast-stop", &[]), "Stopping…");
    }

    #[test]
    fn voice_flow_strings_match_coordinator() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render("transcribing", &[]),
            "🎙️ Transcribing voice message…"
        );
        assert_eq!(
            store.render("transcript", &[("transcript", "hello there")]),
            "🎙️ Transcript:\nhello there"
        );
        assert_eq!(
            store.render("error-transcribe", &[("error", "HTTP 500")]),
            "⚠️ Could not transcribe voice message: HTTP 500"
        );
    }

    #[test]
    fn error_reports_match_coordinator_and_worker() {
        let store = PromptStore::new(None);
        assert_eq!(
            store.render("error-run", &[("error", "spawn ENOENT")]),
            "⚠️ Error: spawn ENOENT"
        );
        assert_eq!(
            store.render(
                "error-exit",
                &[("engine", "Claude"), ("code", "1"), ("stderr", "boom")]
            ),
            "⚠️ Claude exited (code 1).\nboom"
        );
        assert_eq!(
            store.render("error-mac", &[("error", "scp exited 1")]),
            "⚠️ Mac error: scp exited 1"
        );
        assert_eq!(
            store.render("error-exit-mac", &[("engine", "Codex"), ("code", "2")]),
            "⚠️ Codex exited on Mac (code 2)."
        );
        assert_eq!(
            store.render("error-generic", &[("error", "disk full")]),
            "⚠️ disk full"
        );
        assert_eq!(
            store.render("error-media", &[("error", "too big")]),
            "⚠️ Could not process attachment: too big"
        );
        assert_eq!(
            store.render("error-media-size", &[("size", "700"), ("limit", "512")]),
            "Attachment is too large (700 MB; limit 512 MB)."
        );
        assert_eq!(
            store.render("error-media-limit", &[("limit", "512")]),
            "Attachment exceeds the 512 MB limit."
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

    #[test]
    fn try_render_separates_missing_from_empty() {
        let dir = tmpdir();
        fs::write(dir.path().join("blank.md"), "").unwrap();
        let store = store_over(dir.path());
        assert_eq!(store.try_render("nope", &[]), None);
        assert_eq!(store.try_render("blank", &[]), Some(String::new()));
        assert_eq!(store.body("nope"), None);
    }

    // ---- overrides + mtime cache ----

    #[test]
    fn override_file_wins_over_builtin() {
        let dir = tmpdir();
        fs::write(dir.path().join("help.md"), "custom help {{who}}").unwrap();
        let store = store_over(dir.path());
        assert_eq!(store.render("help", &[("who", "nikita")]), "custom help nikita");
        assert!(store.has("help"));
        assert!(store.is_overridden("help"));
        assert!(!store.is_overridden("status"));
    }

    #[test]
    fn override_only_template_exists_and_renders() {
        let dir = tmpdir();
        fs::write(dir.path().join("deploy.md"), "ship {{args}} now").unwrap();
        let store = store_over(dir.path());
        assert!(store.has("deploy"));
        assert_eq!(store.render("deploy", &[("args", "v2")]), "ship v2 now");
        // Nothing is documented for a user-defined template, so nothing can
        // be reported as missing.
        assert!(placeholders("deploy").is_empty());
        assert!(store.missing_required("deploy").is_empty());
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

    // ---- the placeholder lint ----

    #[test]
    fn an_override_that_drops_a_required_placeholder_is_reported() {
        let dir = tmpdir();
        // A user rewrites /where and forgets {{session}} — the single
        // highest-value guard rail, because the loss is otherwise silent.
        fs::write(
            dir.path().join("status.md"),
            "Engine: {{engine}}\nTarget: {{target}}\nMac worker: {{worker}}\nGCP busy: {{busy}}",
        )
        .unwrap();
        let store = store_over(dir.path());
        assert_eq!(store.missing_required("status"), vec!["session"]);
        assert_eq!(store.lint(), vec![("status", vec!["session"])]);
    }

    #[test]
    fn an_override_keeping_every_required_placeholder_is_clean() {
        let dir = tmpdir();
        fs::write(
            dir.path().join("status.md"),
            "{{engine}}/{{target}} s={{session}} w={{worker}} b={{busy}}",
        )
        .unwrap();
        let store = store_over(dir.path());
        assert!(store.missing_required("status").is_empty());
        assert!(store.lint().is_empty());
    }

    #[test]
    fn optional_placeholders_are_never_reported() {
        let dir = tmpdir();
        // `duration` is optional; dropping it is a legitimate taste change.
        fs::write(
            dir.path().join("final.md"),
            "{{text}}\n\n({{engine}} on {{target}})",
        )
        .unwrap();
        let store = store_over(dir.path());
        assert!(store.lint().is_empty());
    }

    #[test]
    fn lint_reports_every_broken_override_in_catalogue_order() {
        let dir = tmpdir();
        fs::write(dir.path().join("status.md"), "no vars here").unwrap();
        fs::write(dir.path().join("final.md"), "no vars here either").unwrap();
        let store = store_over(dir.path());
        let names: Vec<&str> = store.lint().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["status", "final"]);
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
