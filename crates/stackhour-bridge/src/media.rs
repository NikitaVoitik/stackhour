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

use crate::config::CoordinatorCfg;
use crate::telegram::Tg;
use serde_json::Value;
use stackhour_core::registry::PromptStore;
use std::path::{Path, PathBuf};

/// A recognised Telegram attachment, pre-download.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// `photo` | `video` | `video_note` | `animation` | `document` | `voice` | `audio`.
    pub kind: String,
    pub file_id: String,
    pub mime: Option<String>,
    pub name: Option<String>,
    pub size: Option<u64>,
}

/// A downloaded media file on disk.
#[derive(Debug, Clone)]
pub struct SavedMedia {
    pub path: PathBuf,
    pub kind: String,
    pub mime: String,
    pub name: String,
}

/// Extract the (single) attachment from a Telegram message, when any.
pub fn extract_attachment(m: &Value) -> Option<Attachment> {
    let _ = m;
    todo!()
}

/// Download an attachment into media/ (0600 file in a 0700 dir); the exact
/// MB-limit messages are the Err strings.
pub fn download_media(
    tg: &Tg,
    att: &Attachment,
    media_dir: &Path,
    max_bytes: u64,
) -> Result<SavedMedia, String> {
    let _ = (tg, att, media_dir, max_bytes);
    todo!()
}

/// ElevenLabs scribe_v2 transcription (multipart); exact error strings; the
/// audio file is always unlinked.
pub fn transcribe(cfg: &CoordinatorCfg, path: &Path, mime: &str, name: &str) -> Result<String, String> {
    let _ = (cfg, path, mime, name);
    todo!()
}

/// Build the engine prompt for a saved media file from the
/// 'media-image'/'media-video' templates.
pub fn media_prompt(prompts: &PromptStore, caption: &str, media: &SavedMedia) -> String {
    let _ = (prompts, caption, media);
    todo!()
}
