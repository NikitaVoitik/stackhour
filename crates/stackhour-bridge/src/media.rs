//! Attachment extraction, media download, and voice transcription.
//!
//! Photo -> last/largest size; video/video_note/animation; documents only
//! when image-or-video mime. Size guards with the two exact MB messages;
//! safe_ext regex; `<epochMs>-<uuid><ext>` filenames mode 0600 in 0700
//! media/; partial file unlinked on failure. Voice/audio -> ElevenLabs
//! scribe_v2 multipart with exact error strings; the transcript status-edit
//! truncates at 3400 chars; the audio file is ALWAYS unlinked. The media
//! prompt is built from the 'media-image'/'media-video' PromptStore
//! templates (identical logic reused by worker.rs for its local rebuild).
//!
//! Reference: `/home/nikita/.claude-remote/coordinator.mjs` lines 26-33,
//! 90-152, 370-400, 415-440.
//!
//! # Seams
//!
//! Telegram and ElevenLabs are both reached through small traits/structs
//! declared here, not through a hardwired `reqwest` call:
//!
//! * [`MediaTransport`] — `getFile` plus the raw file download, implemented
//!   for [`Tg`] over its `get_file` (which carries the 5-attempt retry
//!   ladder) and `download_response` (one try, no retry — parity, since the
//!   reference does a bare `fetch` for the CDN GET).
//! * [`ElevenLabs`] — endpoint, model id and key in one value, so the
//!   endpoint can point at a local mock in tests and become a config key
//!   later without touching call sites.

use crate::config::CoordinatorCfg;
use crate::telegram::Tg;
use serde_json::Value;
use stackhour_core::registry::PromptStore;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// constants (hardcoded in the JS; gathered here so they are liftable to config)
// ---------------------------------------------------------------------------

/// `https://api.elevenlabs.io/v1/speech-to-text` — hardcoded in coordinator.mjs:140.
pub const ELEVENLABS_ENDPOINT: &str = "https://api.elevenlabs.io/v1/speech-to-text";
/// `scribe_v2` — hardcoded in coordinator.mjs:138.
pub const ELEVENLABS_MODEL: &str = "scribe_v2";
/// `7 * 24 * 60 * 60 * 1000` — the pruneMedia cutoff (coordinator.mjs:149).
pub const PRUNE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// `24 * 60 * 60 * 1000` — the pruneMedia setInterval period (coordinator.mjs:418).
pub const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// The transcript preview cut in handleVoiceMessage (coordinator.mjs:390).
pub const TRANSCRIPT_PREVIEW_CHARS: usize = 3400;
/// `mime || 'application/octet-stream'` in downloadTelegramFile's return.
pub const DEFAULT_MIME: &str = "application/octet-stream";
/// The 👀 reaction, applied to the inbound message by both media handlers.
pub const EYES: &str = "👀";

const BYTES_PER_MB: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// types
// ---------------------------------------------------------------------------

/// A recognised Telegram attachment, pre-download.
///
/// `kind` is the COLLAPSED kind the JS carries — `image` | `video` | `audio`
/// — not the raw Telegram field name. That collapsed value is what feeds the
/// `{{kind}}` placeholder in the engine prompt, the extension fallback and
/// the request/guidance branches, so it must be the stored form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// `image` | `video` | `audio`.
    pub kind: String,
    pub file_id: String,
    pub mime: Option<String>,
    pub name: Option<String>,
    pub size: Option<u64>,
}

/// A downloaded media file on disk.
///
/// `size` mirrors the JS `size || contentLength`: the declared size when
/// Telegram gave one, otherwise the Content-Length, otherwise 0. It is what
/// the `media: <kind> <n> bytes` log line reports and what rides along in the
/// mac job file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedMedia {
    pub path: PathBuf,
    pub kind: String,
    pub mime: String,
    pub name: String,
    pub size: u64,
}

/// One in-flight file download: the pieces of a `fetch()` Response that
/// downloadTelegramFile actually reads.
pub struct FileResponse {
    pub status: u16,
    /// `Number(res.headers.get('content-length') || 0)` — None renders as 0.
    pub content_length: Option<u64>,
    pub body: Box<dyn Read>,
}

/// The two Telegram operations the media path needs.
pub trait MediaTransport {
    /// `tg('getFile', { file_id })` — the unwrapped `result`, or None when
    /// the retry ladder gave up or swallowed a 400/404.
    fn get_file(&self, file_id: &str) -> Option<Value>;

    /// `fetch(FILE_API + '/' + file_path)`. One attempt, no retry, no
    /// timeout. `Err` is a transport failure, which the JS surfaces raw.
    fn fetch_file(&self, file_path: &str) -> Result<FileResponse, String>;
}

/// The real transport: `getFile` through the retry ladder, the CDN GET as a
/// single bare request (coordinator.mjs does not route it through `tg()`).
impl MediaTransport for Tg {
    fn get_file(&self, file_id: &str) -> Option<Value> {
        Tg::get_file(self, file_id)
    }

    fn fetch_file(&self, file_path: &str) -> Result<FileResponse, String> {
        let res = self
            .download_response(file_path)
            .map_err(|e| e.to_string())?;
        Ok(FileResponse {
            status: res.status().as_u16(),
            content_length: res.content_length(),
            body: Box::new(res),
        })
    }
}

/// ElevenLabs speech-to-text configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElevenLabs {
    /// "" disables transcription entirely.
    pub api_key: String,
    pub endpoint: String,
    pub model_id: String,
}

impl ElevenLabs {
    /// Pull the key off the coordinator config; endpoint and model take the
    /// hardcoded JS values.
    pub fn from_cfg(cfg: &CoordinatorCfg) -> Self {
        ElevenLabs {
            api_key: cfg.eleven_labs_api_key.clone(),
            endpoint: ELEVENLABS_ENDPOINT.to_string(),
            model_id: ELEVENLABS_MODEL.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// extraction
// ---------------------------------------------------------------------------

/// Non-empty string field, or None. JS `a || fallback` treats "" as absent.
fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn file_id(v: &Value) -> Option<String> {
    s(v, "file_id")
}

fn file_size(v: &Value) -> Option<u64> {
    v.get("file_size").and_then(Value::as_u64)
}

/// `attachmentFromMessage` (coordinator.mjs:110-123).
///
/// The order is load-bearing: Telegram sends `animation` messages with a
/// `document` field too, so animation must win.
pub fn extract_attachment(m: &Value) -> Option<Attachment> {
    // (1) photo -> the LAST (largest) size.
    if let Some(sizes) = m.get("photo").and_then(Value::as_array) {
        if let Some(p) = sizes.last() {
            return Some(Attachment {
                kind: "image".into(),
                file_id: file_id(p)?,
                mime: Some("image/jpeg".into()),
                name: Some("telegram-photo.jpg".into()),
                size: file_size(p),
            });
        }
    }
    // (2) video, (3) video_note, (4) animation. video_note hardcodes both its
    // mime and its name rather than reading the message.
    for (field, mime_default, name_default, read_fields) in [
        ("video", "video/mp4", "telegram-video.mp4", true),
        ("video_note", "video/mp4", "telegram-video-note.mp4", false),
        ("animation", "video/mp4", "telegram-animation.mp4", true),
    ] {
        if let Some(v) = m.get(field).filter(|v| v.is_object()) {
            return Some(Attachment {
                kind: "video".into(),
                file_id: file_id(v)?,
                mime: Some(
                    read_fields
                        .then(|| s(v, "mime_type"))
                        .flatten()
                        .unwrap_or_else(|| mime_default.to_string()),
                ),
                name: Some(
                    read_fields
                        .then(|| s(v, "file_name"))
                        .flatten()
                        .unwrap_or_else(|| name_default.to_string()),
                ),
                size: file_size(v),
            });
        }
    }
    // (5) document, only when its mime is image/* or video/*.
    if let Some(d) = m.get("document").filter(|v| v.is_object()) {
        let mime = s(d, "mime_type").unwrap_or_default();
        let kind = if mime.starts_with("image/") {
            "image"
        } else if mime.starts_with("video/") {
            "video"
        } else {
            return None;
        };
        return Some(Attachment {
            kind: kind.into(),
            file_id: file_id(d)?,
            mime: Some(mime),
            // NOTE: no extension in this fallback. Parity.
            name: Some(s(d, "file_name").unwrap_or_else(|| format!("telegram-{kind}"))),
            size: file_size(d),
        });
    }
    None
}

/// `voiceFromMessage` (coordinator.mjs:124-128). `voice` beats `audio`.
pub fn extract_voice(m: &Value) -> Option<Attachment> {
    let (a, is_voice) = match m.get("voice").filter(|v| v.is_object()) {
        Some(v) => (v, true),
        None => (m.get("audio").filter(|v| v.is_object())?, false),
    };
    Some(Attachment {
        kind: "audio".into(),
        file_id: file_id(a)?,
        mime: Some(
            s(a, "mime_type")
                .unwrap_or_else(|| if is_voice { "audio/ogg" } else { "audio/mpeg" }.to_string()),
        ),
        // The audio fallback name has NO extension; that name is what goes to
        // ElevenLabs as the multipart filename, so format detection rests on
        // the mime alone.
        name: Some(s(a, "file_name").unwrap_or_else(|| {
            if is_voice {
                "telegram-voice.ogg"
            } else {
                "telegram-audio"
            }
            .to_string()
        })),
        size: file_size(a),
    })
}

// ---------------------------------------------------------------------------
// download
// ---------------------------------------------------------------------------

/// `safeExt` (coordinator.mjs:91-94).
///
/// Node `extname` semantics: the last dot segment (`a.tar.gz` -> `.gz`), and
/// "" when there is no dot or the name is dot-led with no other dot. The
/// result is accepted only when it matches `^\.[a-z0-9]{1,10}$`.
pub fn safe_ext(file_path: &str, fallback: &str) -> String {
    let base = file_path.rsplit('/').next().unwrap_or("");
    let ext = match base.rfind('.') {
        // A leading dot with no other dot is not an extension.
        Some(0) | None => String::new(),
        Some(i) => base[i..].to_lowercase(),
    };
    let ok = (2..=11).contains(&ext.len())
        && ext.starts_with('.')
        && ext[1..]
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if ok {
        ext
    } else {
        fallback.to_string()
    }
}

/// The extension fallback by collapsed kind (coordinator.mjs:100).
fn ext_fallback(kind: &str) -> &'static str {
    match kind {
        "image" => ".jpg",
        "audio" => ".ogg",
        _ => ".mp4",
    }
}

fn ceil_mb(bytes: u64) -> u64 {
    bytes.div_ceil(BYTES_PER_MB)
}

fn floor_mb(bytes: u64) -> u64 {
    bytes / BYTES_PER_MB
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `downloadTelegramFile` (coordinator.mjs:95-109).
///
/// The two size messages come from the `error-media-size` /
/// `error-media-limit` templates; the three transport messages are still
/// literal here because the JS has no template for them.
///
/// # The 20 MB trap
///
/// Telegram's own getFile refuses files over 20 MB with a 400, which the
/// retry ladder swallows into `None` — so an oversized video produces the
/// GENERIC "did not return a downloadable file path" message, and the two
/// nicely-worded MB errors are nearly unreachable. Faithful, unhelpful.
pub fn download_media(
    tg: &dyn MediaTransport,
    att: &Attachment,
    media_dir: &Path,
    max_bytes: u64,
    prompts: &PromptStore,
) -> Result<SavedMedia, String> {
    let meta = tg.get_file(&att.file_id);
    let file_path = meta
        .as_ref()
        .and_then(|m| s(m, "file_path"))
        .ok_or("Telegram did not return a downloadable file path.")?;

    // `Number(info.size || meta.file_size || 0)` — 0 is falsy in JS.
    let size = att
        .size
        .filter(|n| *n != 0)
        .or_else(|| meta.as_ref().and_then(file_size))
        .unwrap_or(0);
    if size > max_bytes {
        return Err(prompts.render(
            "error-media-size",
            &[
                ("size", &ceil_mb(size).to_string()),
                ("limit", &floor_mb(max_bytes).to_string()),
            ],
        ));
    }

    let ext = safe_ext(&file_path, ext_fallback(&att.kind));
    let dest = media_dir.join(format!("{}-{}{ext}", epoch_ms(), uuid::Uuid::new_v4()));

    let mut res = tg.fetch_file(&file_path)?;
    if !(200..300).contains(&res.status) {
        return Err(format!("Telegram download failed (HTTP {}).", res.status));
    }
    let content_length = res.content_length.unwrap_or(0);
    if content_length > max_bytes {
        return Err(prompts.render(
            "error-media-limit",
            &[("limit", &floor_mb(max_bytes).to_string())],
        ));
    }

    // Stream to a 0600 file; a failed write unlinks the partial file and
    // surfaces the raw IO error, exactly as the JS rethrows.
    if let Err(e) = stream_to_file(&mut res.body, &dest) {
        let _ = std::fs::remove_file(&dest);
        return Err(e);
    }

    Ok(SavedMedia {
        path: dest,
        kind: att.kind.clone(),
        mime: att.mime.clone().unwrap_or_else(|| DEFAULT_MIME.to_string()),
        name: att.name.clone().unwrap_or_else(|| format!("telegram{ext}")),
        size: if size != 0 { size } else { content_length },
    })
}

fn stream_to_file(body: &mut dyn Read, dest: &Path) -> Result<(), String> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut fh = opts.open(dest).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        fh.write_all(&buf[..n]).map_err(|e| e.to_string())?;
    }
    fh.flush().map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// transcription
// ---------------------------------------------------------------------------

/// `transcribeAudio` (coordinator.mjs:136-147), taking the endpoint/model
/// from [`ElevenLabs`] so a test can point it at a local mock.
///
/// One attempt, no timeout, no retry — parity.
pub fn transcribe_with(
    el: &ElevenLabs,
    path: &Path,
    mime: &str,
    name: &str,
) -> Result<String, String> {
    if el.api_key.is_empty() {
        return Err("ElevenLabs API key is not configured.".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let part = reqwest::blocking::multipart::Part::bytes(bytes)
        .file_name(name.to_string())
        .mime_str(mime)
        .map_err(|e| e.to_string())?;
    // Field order matches the JS FormData: model_id, then file.
    let form = reqwest::blocking::multipart::Form::new()
        .text("model_id", el.model_id.clone())
        .part("file", part);

    let res = reqwest::blocking::Client::new()
        .post(&el.endpoint)
        .header("xi-api-key", &el.api_key)
        .multipart(form)
        .send()
        .map_err(|e| e.to_string())?;

    let status = res.status().as_u16();
    // `await res.json().catch(() => ({}))` — an unparseable body is {}.
    let data: Value = res
        .json()
        .unwrap_or_else(|_| Value::Object(Default::default()));
    if !(200..300).contains(&status) {
        let detail = data
            .get("detail")
            .and_then(|d| d.get("message"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        return Err(format!(
            "ElevenLabs transcription failed (HTTP {status}{detail})."
        ));
    }
    let text = data.get("text").and_then(Value::as_str).unwrap_or("").trim();
    if text.is_empty() {
        return Err("ElevenLabs returned an empty transcript.".into());
    }
    Ok(text.to_string())
}

/// Convenience wrapper over [`transcribe_with`] for a coordinator config.
pub fn transcribe(
    cfg: &CoordinatorCfg,
    path: &Path,
    mime: &str,
    name: &str,
) -> Result<String, String> {
    transcribe_with(&ElevenLabs::from_cfg(cfg), path, mime, name)
}

/// `transcript.length > 3400 ? transcript.slice(0, 3400) + '…' : transcript`.
///
/// JS `.length` and `.slice` are UTF-16 code units, so the cut is counted in
/// UTF-16 units here too. A cut landing inside a surrogate pair is nudged
/// back one unit rather than producing a lone surrogate (unrepresentable in
/// Rust); that costs one code unit on a boundary only non-BMP text can reach.
pub fn transcript_preview(transcript: &str) -> String {
    let units: usize = transcript.chars().map(char::len_utf16).sum();
    if units <= TRANSCRIPT_PREVIEW_CHARS {
        return transcript.to_string();
    }
    let mut used = 0usize;
    let mut out = String::new();
    for c in transcript.chars() {
        let w = c.len_utf16();
        if used + w > TRANSCRIPT_PREVIEW_CHARS {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// The chat operations the two media handlers perform, as a seam so the
/// ordering can be tested without a Telegram server.
pub trait MediaChat {
    /// `react(message_id, '👀')` — fire-and-forget.
    fn react_eyes(&self, message_id: i64);
    /// `sendMessage(text)` with NO parse_mode and NO keyboard. The message id
    /// when Telegram accepted it; `None` when the retry ladder gave up.
    fn send_plain(&self, text: &str) -> Option<i64>;
    /// `editMessage(message_id, text)` with NO parse_mode.
    fn edit_plain(&self, message_id: i64, text: &str);
}

fn message_id_of(v: &Value) -> Option<i64> {
    v.get("message_id").and_then(Value::as_i64)
}

impl MediaChat for Tg {
    fn react_eyes(&self, message_id: i64) {
        Tg::react_eyes(self, message_id);
    }
    fn send_plain(&self, text: &str) -> Option<i64> {
        self.send(text).as_ref().and_then(message_id_of)
    }
    fn edit_plain(&self, message_id: i64, text: &str) {
        self.edit(message_id, text, None);
    }
}

/// Everything the handlers need that is not the transport.
pub struct MediaCtx<'a> {
    pub prompts: &'a PromptStore,
    pub media_dir: &'a Path,
    pub max_bytes: u64,
    pub eleven: &'a ElevenLabs,
    /// One coordinator.log line.
    pub log: &'a dyn Fn(&str),
}

/// `handleMediaMessage` (coordinator.mjs:375-382).
///
/// Reacts 👀 first, downloads, logs, and hands back the caption + saved file
/// for the caller to route with `msgId = null` — deliberately null, so
/// `routePrompt` does NOT react a second time. On failure it sends the
/// `error-media` message (plain, no keyboard) and returns `None`.
///
/// The file is NOT deleted afterwards: the engine is still reading the path
/// it was handed in the prompt, so only the 7-day prune removes it.
pub fn handle_media_message<T: MediaTransport + MediaChat + ?Sized>(
    tg: &T,
    ctx: &MediaCtx,
    message_id: i64,
    caption: &str,
    att: &Attachment,
) -> Option<(String, SavedMedia)> {
    tg.react_eyes(message_id);
    match download_media(tg, att, ctx.media_dir, ctx.max_bytes, ctx.prompts) {
        Ok(media) => {
            (ctx.log)(&format!("media: {} {} bytes", media.kind, media.size));
            Some((caption.to_string(), media))
        }
        Err(e) => {
            (ctx.log)(&format!("media err {e}"));
            tg.send_plain(&ctx.prompts.render("error-media", &[("error", &e)]));
            None
        }
    }
}

/// `handleVoiceMessage` (coordinator.mjs:383-400).
///
/// Reacts 👀, posts the "Transcribing…" placeholder, downloads, transcribes,
/// echoes the (3400-char) preview by EDITING the placeholder — or by sending
/// a fresh message when the placeholder never made it — and returns the FULL
/// untruncated transcript for the caller to route as plain text with no media
/// object. The audio file is unlinked on every path, success or failure.
pub fn handle_voice_message<T: MediaTransport + MediaChat + ?Sized>(
    tg: &T,
    ctx: &MediaCtx,
    message_id: i64,
    voice: &Attachment,
) -> Option<String> {
    tg.react_eyes(message_id);
    let status = tg.send_plain(&ctx.prompts.render("transcribing", &[]));

    let mut saved: Option<SavedMedia> = None;
    let outcome = download_media(tg, voice, ctx.media_dir, ctx.max_bytes, ctx.prompts).and_then(
        |media| {
            let r = transcribe_with(ctx.eleven, &media.path, &media.mime, &media.name);
            saved = Some(media);
            r
        },
    );

    let result = match outcome {
        Ok(transcript) => {
            let text = ctx.prompts.render(
                "transcript",
                &[("transcript", &transcript_preview(&transcript))],
            );
            emit(tg, status, &text);
            let size = saved.as_ref().map(|m| m.size).unwrap_or(0);
            let chars: usize = transcript.chars().map(char::len_utf16).sum();
            (ctx.log)(&format!("voice: transcribed {size} bytes to {chars} chars"));
            Some(transcript)
        }
        Err(e) => {
            (ctx.log)(&format!("voice err {e}"));
            emit(
                tg,
                status,
                &ctx.prompts.render("error-transcribe", &[("error", &e)]),
            );
            None
        }
    };

    // The JS `finally`: voice audio never persists on disk.
    if let Some(m) = saved {
        let _ = std::fs::remove_file(&m.path);
    }
    result
}

/// `if (status?.message_id) await editMessage(...) else await sendMessage(...)`.
fn emit<T: MediaChat + ?Sized>(tg: &T, status: Option<i64>, text: &str) {
    match status {
        Some(id) => tg.edit_plain(id, text),
        None => {
            tg.send_plain(text);
        }
    }
}

// ---------------------------------------------------------------------------
// prompt
// ---------------------------------------------------------------------------

/// `mediaPrompt(text, media)` (coordinator.mjs:129-135).
pub fn media_prompt(prompts: &PromptStore, caption: &str, media: &SavedMedia) -> String {
    media_prompt_at(prompts, caption, media, &media.path.to_string_lossy())
}

/// `mediaPrompt(text, media, localPath)` — the third argument exists so the
/// mac worker can substitute the path the file actually landed at on the Mac.
///
/// The two branches test DIFFERENT kinds and that asymmetry is real: the
/// request fallback asks "is it an image?", the guidance asks "is it a
/// video?". So an unrecognised kind gets the VIDEO request text with the
/// IMAGE guidance.
pub fn media_prompt_at(
    prompts: &PromptStore,
    caption: &str,
    media: &SavedMedia,
    local_path: &str,
) -> String {
    let caption = caption.trim();
    let request = if !caption.is_empty() {
        caption.to_string()
    } else if media.kind == "image" {
        prompts.render("media-request-image", &[])
    } else {
        prompts.render("media-request-video", &[])
    };
    let template = if media.kind == "video" {
        "media-video"
    } else {
        "media-image"
    };
    prompts.render(
        template,
        &[
            ("request", request.as_str()),
            ("kind", media.kind.as_str()),
            ("mime", media.mime.as_str()),
            ("name", media.name.as_str()),
            ("path", local_path),
        ],
    )
}

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

/// `pruneMedia` (coordinator.mjs:148-152).
///
/// Flat sweep, no extension filter, no recursion; every entry is wrapped so
/// one bad file never aborts the pass, and an unreadable directory is a
/// silent no-op. Non-voice attachments are removed ONLY here — the engine may
/// still be reading the path handed to it in the prompt, so nothing deletes
/// them after a run.
pub fn prune_media(dir: &Path, max_age: Duration) {
    let cutoff = match SystemTime::now().checked_sub(max_age) {
        Some(t) => t,
        None => return,
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|m| m < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> PromptStore {
        PromptStore::new(None)
    }

    // ---- extraction -------------------------------------------------------

    #[test]
    fn a_photo_takes_the_largest_size_with_the_hardcoded_jpeg_identity() {
        let m = json!({ "photo": [
            { "file_id": "small", "file_size": 100 },
            { "file_id": "big", "file_size": 9000 },
        ]});
        let a = extract_attachment(&m).unwrap();
        assert_eq!(a.file_id, "big");
        assert_eq!(a.kind, "image");
        assert_eq!(a.mime.as_deref(), Some("image/jpeg"));
        assert_eq!(a.name.as_deref(), Some("telegram-photo.jpg"));
        assert_eq!(a.size, Some(9000));
    }

    #[test]
    fn video_video_note_and_animation_all_collapse_to_the_video_kind() {
        let cases = [
            (
                json!({ "video": { "file_id": "v" } }),
                "video/mp4",
                "telegram-video.mp4",
            ),
            (
                json!({ "video_note": { "file_id": "v" } }),
                "video/mp4",
                "telegram-video-note.mp4",
            ),
            (
                json!({ "animation": { "file_id": "v" } }),
                "video/mp4",
                "telegram-animation.mp4",
            ),
        ];
        for (m, mime, name) in cases {
            let a = extract_attachment(&m).unwrap();
            assert_eq!(a.kind, "video");
            assert_eq!(a.mime.as_deref(), Some(mime));
            assert_eq!(a.name.as_deref(), Some(name), "{m}");
        }
    }

    /// video_note has no mime_type/file_name in the Bot API, and the JS
    /// hardcodes both rather than reading them.
    #[test]
    fn a_video_note_ignores_any_mime_or_name_telegram_supplies() {
        let m = json!({ "video_note": { "file_id": "v", "mime_type": "video/quicktime", "file_name": "clip.mov" } });
        let a = extract_attachment(&m).unwrap();
        assert_eq!(a.mime.as_deref(), Some("video/mp4"));
        assert_eq!(a.name.as_deref(), Some("telegram-video-note.mp4"));
    }

    /// Telegram attaches a `document` to animation messages, so animation
    /// must be checked first or every GIF would take the document branch.
    #[test]
    fn animation_wins_over_the_document_telegram_sends_alongside_it() {
        let m = json!({
            "animation": { "file_id": "anim", "mime_type": "video/mp4" },
            "document":  { "file_id": "doc",  "mime_type": "video/mp4" },
        });
        assert_eq!(extract_attachment(&m).unwrap().file_id, "anim");
    }

    #[test]
    fn documents_are_taken_only_for_image_or_video_mimes() {
        let img = json!({ "document": { "file_id": "d", "mime_type": "image/png" } });
        let a = extract_attachment(&img).unwrap();
        assert_eq!(a.kind, "image");
        assert_eq!(a.mime.as_deref(), Some("image/png"));
        // The fallback name carries NO extension. Parity.
        assert_eq!(a.name.as_deref(), Some("telegram-image"));

        let vid = json!({ "document": { "file_id": "d", "mime_type": "video/webm" } });
        assert_eq!(
            extract_attachment(&vid).unwrap().name.as_deref(),
            Some("telegram-video")
        );

        for bad in ["application/pdf", "text/plain", ""] {
            let m = json!({ "document": { "file_id": "d", "mime_type": bad } });
            assert!(extract_attachment(&m).is_none(), "{bad} must not be taken");
        }
        assert!(extract_attachment(&json!({ "document": { "file_id": "d" } })).is_none());
    }

    /// Stickers, PDFs, locations, contacts and polls all yield nothing and
    /// the coordinator answers with silence. That silence is the contract.
    #[test]
    fn unknown_message_types_yield_no_attachment_and_no_voice() {
        for m in [
            json!({ "sticker": { "file_id": "s" } }),
            json!({ "location": { "latitude": 1.0 } }),
            json!({ "poll": { "id": "p" } }),
            json!({ "text": "hello" }),
        ] {
            assert!(extract_attachment(&m).is_none(), "{m}");
            assert!(extract_voice(&m).is_none(), "{m}");
        }
    }

    #[test]
    fn voice_beats_audio_and_each_has_its_own_mime_and_name_fallback() {
        let m = json!({
            "voice": { "file_id": "voice", "file_size": 12 },
            "audio": { "file_id": "audio" },
        });
        let a = extract_voice(&m).unwrap();
        assert_eq!(a.file_id, "voice");
        assert_eq!(a.kind, "audio");
        assert_eq!(a.mime.as_deref(), Some("audio/ogg"));
        assert_eq!(a.name.as_deref(), Some("telegram-voice.ogg"));
        assert_eq!(a.size, Some(12));

        let m = json!({ "audio": { "file_id": "audio" } });
        let a = extract_voice(&m).unwrap();
        assert_eq!(a.mime.as_deref(), Some("audio/mpeg"));
        // No extension: ElevenLabs then detects format from the mime alone.
        assert_eq!(a.name.as_deref(), Some("telegram-audio"));

        let m =
            json!({ "audio": { "file_id": "a", "mime_type": "audio/flac", "file_name": "song.flac" } });
        let a = extract_voice(&m).unwrap();
        assert_eq!(a.mime.as_deref(), Some("audio/flac"));
        assert_eq!(a.name.as_deref(), Some("song.flac"));
    }

    /// Voice is computed before the attachment and wins.
    #[test]
    fn a_voice_note_is_not_also_an_attachment() {
        let m = json!({ "voice": { "file_id": "v" } });
        assert!(extract_voice(&m).is_some());
        assert!(extract_attachment(&m).is_none());
    }

    // ---- safe_ext ---------------------------------------------------------

    #[test]
    fn safe_ext_mirrors_node_extname_plus_the_regex_gate() {
        assert_eq!(safe_ext("photos/file_1.JPG", ".mp4"), ".jpg");
        assert_eq!(safe_ext("a.tar.gz", ".mp4"), ".gz");
        assert_eq!(safe_ext("voice/file_9.oga", ".ogg"), ".oga");
        // No dot at all.
        assert_eq!(safe_ext("noext", ".jpg"), ".jpg");
        assert_eq!(safe_ext("", ".jpg"), ".jpg");
        // Dot-led with no other dot is not an extension.
        assert_eq!(safe_ext(".bashrc", ".mp4"), ".mp4");
        // Longer than 10, or non-alphanumeric: rejected.
        assert_eq!(safe_ext("f.abcdefghijk", ".mp4"), ".mp4");
        assert_eq!(safe_ext("f.tar-gz", ".mp4"), ".mp4");
        assert_eq!(safe_ext("f.", ".mp4"), ".mp4");
        // Exactly 10 is still fine.
        assert_eq!(safe_ext("f.abcdefghij", ".mp4"), ".abcdefghij");
    }

    #[test]
    fn the_extension_fallback_follows_the_collapsed_kind() {
        assert_eq!(ext_fallback("image"), ".jpg");
        assert_eq!(ext_fallback("audio"), ".ogg");
        assert_eq!(ext_fallback("video"), ".mp4");
        assert_eq!(ext_fallback("anything-else"), ".mp4");
    }

    // ---- media prompt -----------------------------------------------------

    fn saved(kind: &str, mime: &str, name: &str) -> SavedMedia {
        SavedMedia {
            path: PathBuf::from("/run/media/1-x.jpg"),
            kind: kind.into(),
            mime: mime.into(),
            name: name.into(),
            size: 4,
        }
    }

    #[test]
    fn the_image_prompt_is_byte_identical_to_the_js() {
        let got = media_prompt(
            &store(),
            "",
            &saved("image", "image/jpeg", "telegram-photo.jpg"),
        );
        assert_eq!(
            got,
            "Please inspect this image and respond.\n\nTelegram attachment (image, image/jpeg, telegram-photo.jpg) is saved locally at: /run/media/1-x.jpg\nUse the available image inspection tool to view it."
        );
    }

    #[test]
    fn the_video_prompt_is_byte_identical_to_the_js() {
        let got = media_prompt(
            &store(),
            "  what is this?  ",
            &saved("video", "video/mp4", "clip.mp4"),
        );
        assert_eq!(
            got,
            "what is this?\n\nTelegram attachment (video, video/mp4, clip.mp4) is saved locally at: /run/media/1-x.jpg\nUse available tools such as ffmpeg/ffprobe to inspect representative frames and audio when useful."
        );
    }

    /// The request fallback tests for 'image' and the guidance tests for
    /// 'video', so an unrecognised kind gets the VIDEO request with the
    /// IMAGE guidance. Preserved deliberately.
    #[test]
    fn an_unrecognised_kind_gets_the_video_request_and_the_image_guidance() {
        let got = media_prompt(&store(), "", &saved("audio", "audio/ogg", "telegram-voice.ogg"));
        assert!(
            got.starts_with("Please inspect this video and respond."),
            "{got}"
        );
        assert!(
            got.ends_with("Use the available image inspection tool to view it."),
            "{got}"
        );
    }

    /// The mac worker rebuilds the prompt with the path the file landed at
    /// on the Mac; everything else must be identical.
    #[test]
    fn the_worker_can_substitute_a_local_path() {
        let media = saved("image", "image/jpeg", "p.jpg");
        let got = media_prompt_at(&store(), "look", &media, "/Users/n/media/p.jpg");
        assert!(got.contains("saved locally at: /Users/n/media/p.jpg"));
        assert!(!got.contains("/run/media"));
    }

    // ---- download ---------------------------------------------------------

    struct FakeTg {
        meta: Option<Value>,
        status: u16,
        content_length: Option<u64>,
        body: Vec<u8>,
        fail: Option<String>,
    }

    impl FakeTg {
        fn ok(file_path: &str, body: &[u8]) -> Self {
            FakeTg {
                meta: Some(json!({ "file_path": file_path, "file_size": body.len() })),
                status: 200,
                content_length: Some(body.len() as u64),
                body: body.to_vec(),
                fail: None,
            }
        }
    }

    impl MediaTransport for FakeTg {
        fn get_file(&self, _file_id: &str) -> Option<Value> {
            self.meta.clone()
        }
        fn fetch_file(&self, _file_path: &str) -> Result<FileResponse, String> {
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            Ok(FileResponse {
                status: self.status,
                content_length: self.content_length,
                body: Box::new(std::io::Cursor::new(self.body.clone())),
            })
        }
    }

    fn att(kind: &str) -> Attachment {
        Attachment {
            kind: kind.into(),
            file_id: "fid".into(),
            mime: Some("image/jpeg".into()),
            name: Some("telegram-photo.jpg".into()),
            size: None,
        }
    }

    #[test]
    fn a_download_writes_a_0600_file_named_epoch_dash_uuid_dot_ext() {
        let dir = tempfile::tempdir().unwrap();
        let tg = FakeTg::ok("photos/file_7.jpg", b"binary-bytes");
        let m =
            download_media(&tg, &att("image"), dir.path(), 512 * 1024 * 1024, &store()).unwrap();

        assert_eq!(std::fs::read(&m.path).unwrap(), b"binary-bytes");
        assert_eq!(m.size, 12, "declared size falls back to the getFile size");
        assert_eq!(m.kind, "image");
        assert_eq!(m.mime, "image/jpeg");

        let name = m.path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with(".jpg"), "{name}");
        let stem = name.trim_end_matches(".jpg");
        let (ms, uuid) = stem.split_once('-').unwrap();
        assert!(
            ms.chars().all(|c| c.is_ascii_digit()) && ms.len() >= 13,
            "{ms}"
        );
        assert_eq!(uuid.len(), 36, "{uuid}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&m.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "attachments are private");
        }
    }

    /// The dominant real-world failure: getFile 400s on anything over 20 MB,
    /// the retry ladder swallows it to None, and the user sees the GENERIC
    /// message rather than either MB error.
    #[test]
    fn a_missing_file_path_yields_the_generic_message_not_a_size_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut tg = FakeTg::ok("x.jpg", b"");
        tg.meta = None;
        let e = download_media(&tg, &att("image"), dir.path(), 1, &store()).unwrap_err();
        assert_eq!(e, "Telegram did not return a downloadable file path.");

        tg.meta = Some(json!({ "file_size": 10 }));
        let e = download_media(&tg, &att("image"), dir.path(), 1, &store()).unwrap_err();
        assert_eq!(e, "Telegram did not return a downloadable file path.");
    }

    /// Two different strings for the same condition — ceil on the actual,
    /// floor on the limit, and a different sentence mid-download.
    #[test]
    fn the_two_size_errors_are_distinct_and_round_in_opposite_directions() {
        let dir = tempfile::tempdir().unwrap();
        let limit = 512 * 1024 * 1024 + 1; // floors to 512
        let mut a = att("image");
        a.size = Some(700 * 1024 * 1024 - 1); // ceils to 700

        let tg = FakeTg::ok("x.jpg", b"");
        let e = download_media(&tg, &a, dir.path(), limit, &store()).unwrap_err();
        assert_eq!(e, "Attachment is too large (700 MB; limit 512 MB).");

        // Pre-check passes, Content-Length does not.
        let mut tg = FakeTg::ok("x.jpg", b"");
        tg.content_length = Some(limit + 1);
        let e = download_media(&tg, &att("image"), dir.path(), limit, &store()).unwrap_err();
        assert_eq!(e, "Attachment exceeds the 512 MB limit.");
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "no file written"
        );
    }

    /// `Number(null || 0)` is 0, so a missing Content-Length passes the check
    /// and nothing counts bytes during the stream. Advisory only — flagged,
    /// not hardened, because hardening would change behaviour.
    #[test]
    fn a_missing_content_length_passes_the_check_unbounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut tg = FakeTg::ok("x.jpg", &[7u8; 4096]);
        tg.content_length = None;
        tg.meta = Some(json!({ "file_path": "x.jpg" }));
        let m = download_media(&tg, &att("image"), dir.path(), 1, &store()).unwrap();
        assert_eq!(std::fs::metadata(&m.path).unwrap().len(), 4096);
        assert_eq!(m.size, 0, "neither a declared size nor a content-length");
    }

    #[test]
    fn a_non_ok_download_reports_the_status_and_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut tg = FakeTg::ok("x.jpg", b"");
        tg.status = 502;
        let e = download_media(&tg, &att("image"), dir.path(), u64::MAX, &store()).unwrap_err();
        assert_eq!(e, "Telegram download failed (HTTP 502).");
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn a_transport_failure_surfaces_raw() {
        let dir = tempfile::tempdir().unwrap();
        let mut tg = FakeTg::ok("x.jpg", b"");
        tg.fail = Some("dns error".into());
        let e = download_media(&tg, &att("image"), dir.path(), u64::MAX, &store()).unwrap_err();
        assert_eq!(e, "dns error");
    }

    /// A stream that dies mid-body must unlink the partial file and surface
    /// the raw IO error, not a friendly string.
    #[test]
    fn a_broken_stream_unlinks_the_partial_file_and_rethrows() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf[..4].copy_from_slice(b"half");
                Err(std::io::Error::other("terminated"))
            }
        }
        struct T;
        impl MediaTransport for T {
            fn get_file(&self, _: &str) -> Option<Value> {
                Some(json!({ "file_path": "x.jpg" }))
            }
            fn fetch_file(&self, _: &str) -> Result<FileResponse, String> {
                Ok(FileResponse {
                    status: 200,
                    content_length: Some(4),
                    body: Box::new(Broken),
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let e = download_media(&T, &att("image"), dir.path(), u64::MAX, &store()).unwrap_err();
        assert_eq!(e, "terminated");
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "partial file removed"
        );
    }

    #[test]
    fn the_saved_name_and_mime_fall_back_when_the_attachment_carried_none() {
        let dir = tempfile::tempdir().unwrap();
        let tg = FakeTg::ok("videos/file_2.mp4", b"v");
        let a = Attachment {
            kind: "video".into(),
            file_id: "f".into(),
            mime: None,
            name: None,
            size: None,
        };
        let m = download_media(&tg, &a, dir.path(), u64::MAX, &store()).unwrap();
        assert_eq!(m.mime, "application/octet-stream");
        assert_eq!(m.name, "telegram.mp4");
    }

    /// Two back-to-back attachments must not collide on a filename even
    /// within the same millisecond — the uuid carries that.
    #[test]
    fn concurrent_downloads_get_distinct_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let tg = FakeTg::ok("x.jpg", b"a");
        let one = download_media(&tg, &att("image"), dir.path(), u64::MAX, &store()).unwrap();
        let two = download_media(&tg, &att("image"), dir.path(), u64::MAX, &store()).unwrap();
        assert_ne!(one.path, two.path);
    }

    // ---- handlers ---------------------------------------------------------

    /// Records every chat call in order so the tests can assert the sequence,
    /// not just the end state.
    #[derive(Default)]
    struct Chat {
        calls: std::sync::Mutex<Vec<String>>,
        /// None makes sendMessage fail, as the retry ladder does after 5 tries.
        send_id: Option<i64>,
    }

    impl Chat {
        fn log(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl MediaChat for Chat {
        fn react_eyes(&self, message_id: i64) {
            self.calls.lock().unwrap().push(format!("react {message_id}"));
        }
        fn send_plain(&self, text: &str) -> Option<i64> {
            self.calls.lock().unwrap().push(format!("send {text}"));
            self.send_id
        }
        fn edit_plain(&self, message_id: i64, text: &str) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("edit {message_id} {text}"));
        }
    }

    /// Chat + transport in one value, so a handler can take a single `tg`.
    struct Wired {
        chat: Chat,
        tg: FakeTg,
    }

    impl MediaChat for Wired {
        fn react_eyes(&self, id: i64) {
            self.chat.react_eyes(id)
        }
        fn send_plain(&self, t: &str) -> Option<i64> {
            self.chat.send_plain(t)
        }
        fn edit_plain(&self, id: i64, t: &str) {
            self.chat.edit_plain(id, t)
        }
    }

    impl MediaTransport for Wired {
        fn get_file(&self, id: &str) -> Option<Value> {
            self.tg.get_file(id)
        }
        fn fetch_file(&self, p: &str) -> Result<FileResponse, String> {
            self.tg.fetch_file(p)
        }
    }

    struct Harness {
        dir: tempfile::TempDir,
        prompts: PromptStore,
        eleven: ElevenLabs,
        logs: std::sync::Mutex<Vec<String>>,
    }

    impl Harness {
        fn new() -> Self {
            Harness {
                dir: tempfile::tempdir().unwrap(),
                prompts: store(),
                eleven: ElevenLabs {
                    api_key: String::new(), // disabled unless a test sets it
                    endpoint: "http://127.0.0.1:1/never".into(),
                    model_id: ELEVENLABS_MODEL.into(),
                },
                logs: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn ctx(&self) -> MediaCtx<'_> {
            MediaCtx {
                prompts: &self.prompts,
                media_dir: self.dir.path(),
                max_bytes: u64::MAX,
                eleven: &self.eleven,
                log: &|m| self.logs.lock().unwrap().push(m.to_string()),
            }
        }
    }

    #[test]
    fn a_media_message_reacts_first_then_hands_back_the_caption_and_the_file() {
        let h = Harness::new();
        let w = Wired {
            chat: Chat { send_id: Some(9), ..Default::default() },
            tg: FakeTg::ok("photos/f.jpg", b"jpegdata"),
        };
        let (caption, media) =
            handle_media_message(&w, &h.ctx(), 77, "what is this", &att("image")).unwrap();

        assert_eq!(caption, "what is this");
        assert!(media.path.exists(), "media files are NOT cleaned up after a run");
        // The only chat call is the reaction: no status message on this lane.
        assert_eq!(w.chat.log(), vec!["react 77"]);
        assert_eq!(h.logs.lock().unwrap().clone(), vec!["media: image 8 bytes"]);
    }

    #[test]
    fn a_failed_media_download_reports_plainly_and_routes_nothing() {
        let h = Harness::new();
        let mut tg = FakeTg::ok("x.jpg", b"");
        tg.meta = None;
        let w = Wired { chat: Chat { send_id: Some(9), ..Default::default() }, tg };

        assert!(handle_media_message(&w, &h.ctx(), 5, "cap", &att("image")).is_none());
        assert_eq!(
            w.chat.log(),
            vec![
                "react 5",
                "send ⚠️ Could not process attachment: Telegram did not return a downloadable file path.",
            ]
        );
        assert_eq!(
            h.logs.lock().unwrap().clone(),
            vec!["media err Telegram did not return a downloadable file path."]
        );
    }

    /// A five-photo album arrives as five independent updates, so the caption
    /// rides on the first one and the rest carry ''. Preserved.
    #[test]
    fn an_album_is_n_independent_reactions_and_n_independent_files() {
        let h = Harness::new();
        let mut paths = vec![];
        for (i, caption) in ["the album caption", "", "", "", ""].iter().enumerate() {
            let w = Wired {
                chat: Chat { send_id: Some(1), ..Default::default() },
                tg: FakeTg::ok("photos/f.jpg", b"x"),
            };
            let (cap, media) =
                handle_media_message(&w, &h.ctx(), 100 + i as i64, caption, &att("image")).unwrap();
            assert_eq!(&cap, caption);
            assert_eq!(w.chat.log(), vec![format!("react {}", 100 + i)]);
            paths.push(media.path);
        }
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), 5, "five separate files on disk");
    }

    #[test]
    fn a_voice_message_edits_the_placeholder_and_routes_the_full_transcript() {
        let mut h = Harness::new();
        h.eleven.api_key = "fake".into();
        let long = "word ".repeat(1000); // > 3400 UTF-16 units
        let body = serde_json::to_vec(&json!({ "text": &long })).unwrap();
        h.eleven.endpoint = mock_json(200, body);

        let w = Wired {
            chat: Chat { send_id: Some(42), ..Default::default() },
            tg: FakeTg::ok("voice/f.oga", b"oggdata"),
        };
        let transcript = handle_voice_message(&w, &h.ctx(), 7, &voice_att()).unwrap();

        assert_eq!(transcript, long, "routing gets the UNTRUNCATED transcript");
        let calls = w.chat.log();
        assert_eq!(calls[0], "react 7");
        assert_eq!(calls[1], "send 🎙️ Transcribing voice message…");
        assert!(calls[2].starts_with("edit 42 🎙️ Transcript:\nword "), "{}", calls[2]);
        // The echoed preview IS truncated.
        assert!(calls[2].ends_with('…'));
        assert_eq!(calls.len(), 3);

        assert_eq!(
            h.logs.lock().unwrap().clone(),
            vec![format!("voice: transcribed 7 bytes to {} chars", long.len())]
        );
        assert_no_files_left(&h);
    }

    /// sendMessage returns null once the retry ladder gives up, so the
    /// transcript must fall back to a fresh message. Both branches are real.
    #[test]
    fn a_voice_transcript_falls_back_to_a_new_message_when_the_placeholder_failed() {
        let mut h = Harness::new();
        h.eleven.api_key = "fake".into();
        h.eleven.endpoint = mock_json(200, serde_json::to_vec(&json!({ "text": " hi " })).unwrap());

        let w = Wired {
            chat: Chat { send_id: None, ..Default::default() }, // placeholder never landed
            tg: FakeTg::ok("voice/f.oga", b"ogg"),
        };
        let transcript = handle_voice_message(&w, &h.ctx(), 7, &voice_att()).unwrap();
        assert_eq!(transcript, "hi", "ElevenLabs text is trimmed");
        assert_eq!(
            w.chat.log(),
            vec![
                "react 7",
                "send 🎙️ Transcribing voice message…",
                "send 🎙️ Transcript:\nhi",
            ]
        );
        assert_no_files_left(&h);
    }

    /// The `finally` deletes the audio on the failure path too, and no key
    /// configured is the exact string a user with transcription off sees.
    #[test]
    fn a_transcription_failure_still_unlinks_the_audio_and_edits_the_placeholder() {
        let h = Harness::new(); // api_key is ""
        let w = Wired {
            chat: Chat { send_id: Some(3), ..Default::default() },
            tg: FakeTg::ok("voice/f.oga", b"ogg"),
        };
        assert!(handle_voice_message(&w, &h.ctx(), 7, &voice_att()).is_none());
        assert_eq!(
            w.chat.log(),
            vec![
                "react 7",
                "send 🎙️ Transcribing voice message…",
                "edit 3 ⚠️ Could not transcribe voice message: ElevenLabs API key is not configured.",
            ]
        );
        assert_eq!(
            h.logs.lock().unwrap().clone(),
            vec!["voice err ElevenLabs API key is not configured."]
        );
        assert_no_files_left(&h);
    }

    /// A download that never happened leaves nothing to unlink and still
    /// reports through the placeholder.
    #[test]
    fn a_voice_download_failure_reports_without_touching_disk() {
        let h = Harness::new();
        let mut tg = FakeTg::ok("x.oga", b"");
        tg.status = 500;
        let w = Wired { chat: Chat { send_id: Some(3), ..Default::default() }, tg };
        assert!(handle_voice_message(&w, &h.ctx(), 7, &voice_att()).is_none());
        assert_eq!(
            w.chat.log()[2],
            "edit 3 ⚠️ Could not transcribe voice message: Telegram download failed (HTTP 500)."
        );
        assert_no_files_left(&h);
    }

    fn voice_att() -> Attachment {
        Attachment {
            kind: "audio".into(),
            file_id: "fid".into(),
            mime: Some("audio/ogg".into()),
            name: Some("telegram-voice.ogg".into()),
            size: None,
        }
    }

    fn assert_no_files_left(h: &Harness) {
        assert!(
            std::fs::read_dir(h.dir.path()).unwrap().next().is_none(),
            "voice audio must never persist"
        );
    }

    /// A one-shot local HTTP server that answers the next request with a
    /// canned body. NEVER the real ElevenLabs (or Telegram) endpoint.
    fn mock_json(status: u16, body: Vec<u8>) -> String {
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                // Drain enough of the request that the client is not blocked
                // writing while we reply.
                let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
                let mut sink = [0u8; 8192];
                while let Ok(n) = sock.read(&mut sink) {
                    if n < sink.len() {
                        break;
                    }
                }
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(&body);
                let _ = sock.flush();
            }
        });
        format!("http://{addr}/v1/speech-to-text")
    }

    // ---- transcription over the mock ---------------------------------------

    #[test]
    fn transcribe_posts_multipart_and_returns_the_trimmed_text() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.ogg");
        std::fs::write(&f, b"audio").unwrap();
        let el = ElevenLabs {
            api_key: "fake-key".into(),
            endpoint: mock_json(200, serde_json::to_vec(&json!({ "text": "  hello  " })).unwrap()),
            model_id: ELEVENLABS_MODEL.into(),
        };
        assert_eq!(
            transcribe_with(&el, &f, "audio/ogg", "telegram-voice.ogg").unwrap(),
            "hello"
        );
    }

    #[test]
    fn an_elevenlabs_error_carries_the_status_and_the_detail_message() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.ogg");
        std::fs::write(&f, b"audio").unwrap();

        let with_detail = ElevenLabs {
            api_key: "k".into(),
            endpoint: mock_json(
                422,
                serde_json::to_vec(&json!({ "detail": { "message": "bad audio" } })).unwrap(),
            ),
            model_id: ELEVENLABS_MODEL.into(),
        };
        assert_eq!(
            transcribe_with(&with_detail, &f, "audio/ogg", "v.ogg").unwrap_err(),
            "ElevenLabs transcription failed (HTTP 422: bad audio)."
        );

        // No parseable detail -> no colon clause.
        let bare = ElevenLabs {
            endpoint: mock_json(500, b"<html>oops</html>".to_vec()),
            ..with_detail.clone()
        };
        assert_eq!(
            transcribe_with(&bare, &f, "audio/ogg", "v.ogg").unwrap_err(),
            "ElevenLabs transcription failed (HTTP 500)."
        );
    }

    #[test]
    fn an_empty_or_missing_transcript_is_an_error_not_an_empty_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.ogg");
        std::fs::write(&f, b"audio").unwrap();
        for body in [json!({ "text": "   " }), json!({}), json!({ "text": 7 })] {
            let el = ElevenLabs {
                api_key: "k".into(),
                endpoint: mock_json(200, serde_json::to_vec(&body).unwrap()),
                model_id: ELEVENLABS_MODEL.into(),
            };
            assert_eq!(
                transcribe_with(&el, &f, "audio/ogg", "v.ogg").unwrap_err(),
                "ElevenLabs returned an empty transcript.",
                "{body}"
            );
        }
    }

    #[test]
    fn no_api_key_short_circuits_before_any_request() {
        let el = ElevenLabs {
            api_key: String::new(),
            endpoint: "http://127.0.0.1:1/nope".into(),
            model_id: ELEVENLABS_MODEL.into(),
        };
        assert_eq!(
            transcribe_with(&el, Path::new("/nonexistent"), "audio/ogg", "v.ogg").unwrap_err(),
            "ElevenLabs API key is not configured."
        );
    }

    // ---- transcript preview ----------------------------------------------

    #[test]
    fn the_transcript_preview_truncates_at_3400_and_appends_an_ellipsis() {
        assert_eq!(transcript_preview("hello"), "hello");

        let exact = "a".repeat(TRANSCRIPT_PREVIEW_CHARS);
        assert_eq!(transcript_preview(&exact), exact, "the boundary is > not >=");

        let long = "a".repeat(TRANSCRIPT_PREVIEW_CHARS + 1);
        let cut = transcript_preview(&long);
        assert_eq!(cut.chars().count(), TRANSCRIPT_PREVIEW_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    /// JS counts UTF-16 code units, so a string of astral emoji hits the
    /// limit at half as many characters.
    #[test]
    fn the_preview_counts_utf16_code_units_like_js_does() {
        let emoji = "😀".repeat(TRANSCRIPT_PREVIEW_CHARS); // 2 units each
        let cut = transcript_preview(&emoji);
        let kept = cut.trim_end_matches('…').chars().count();
        assert_eq!(kept, TRANSCRIPT_PREVIEW_CHARS / 2);
    }

    // ---- prune ------------------------------------------------------------

    #[test]
    fn prune_deletes_only_files_older_than_the_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.jpg");
        let fresh = dir.path().join("fresh.jpg");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&fresh, b"x").unwrap();
        let stale = SystemTime::now() - PRUNE_MAX_AGE - Duration::from_secs(60);
        set_mtime(&old, stale);

        prune_media(dir.path(), PRUNE_MAX_AGE);
        assert!(!old.exists(), "the 7-day-old file is gone");
        assert!(fresh.exists(), "a fresh attachment survives");
    }

    /// No extension filter, no recursion, and a subdirectory that cannot be
    /// unlinked must not abort the sweep.
    #[test]
    fn prune_is_flat_untyped_and_survives_a_bad_entry() {
        let dir = tempfile::tempdir().unwrap();
        let stale = SystemTime::now() - PRUNE_MAX_AGE - Duration::from_secs(60);

        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let inner = sub.join("keep.jpg");
        std::fs::write(&inner, b"x").unwrap();
        set_mtime(&inner, stale);
        set_mtime(&sub, stale);

        let odd = dir.path().join("no-extension-at-all");
        std::fs::write(&odd, b"x").unwrap();
        set_mtime(&odd, stale);

        prune_media(dir.path(), PRUNE_MAX_AGE);
        assert!(!odd.exists(), "extension is irrelevant");
        assert!(sub.is_dir(), "a directory cannot be unlinked; swallowed");
        assert!(inner.exists(), "no recursion");
    }

    #[test]
    fn prune_on_a_missing_directory_is_a_silent_no_op() {
        prune_media(Path::new("/nonexistent-media-dir-xyz"), PRUNE_MAX_AGE);
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        let secs = when
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let times = [
            libc::timespec {
                tv_sec: secs,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: secs,
                tv_nsec: 0,
            },
        ];
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(rc, 0, "utimensat on {path:?}");
    }
}
