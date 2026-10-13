//! Photos and image documents for the vision model.
//!
//! Originals are saved under `data/media/` (EXIF intact, so tools can read the capture date for
//! scale backfill); the model gets a JPEG/PNG data URL.
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{Value, json};

use crate::error::Result;
use crate::telegram::Media;

/// Image kind from magic bytes.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [_, _, _, _, b'f', b't', b'y', b'p', rest @ ..] if rest.starts_with(b"heic") || rest.starts_with(b"heix") || rest.starts_with(b"mif1") || rest.starts_with(b"msf1") => Some("image/heic"),
        _ => None,
    }
}

/// Save an inbound file as `data/media/<unique id>.<ext>`.
pub async fn save(data_dir: &Path, unique_id: &str, bytes: &[u8]) -> Result<Option<Media>> {
    let Some(mime) = sniff(bytes) else { return Ok(None) };
    let ext = mime.trim_start_matches("image/");
    let dir = data_dir.join("media");
    tokio::fs::create_dir_all(&dir).await?;
    let safe: String = unique_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    let path: PathBuf = dir.join(format!("{safe}.{ext}"));
    tokio::fs::write(&path, bytes).await?;
    Ok(Some(Media { path, mime: mime.into() }))
}

/// Bytes the vision model can read: JPEG/PNG/WebP/GIF as is; HEIC is decoded to JPEG.
///
/// HEIC decoding belongs to `crate::providers::photo::to_jpeg` (WP-providers); until it lands, HEIC
/// files are described to the model instead of shown.
fn viewable(media: &Media, bytes: Vec<u8>) -> Option<(String, Vec<u8>)> {
    if media.mime == "image/heic" {
        return None;
    }
    Some((media.mime.clone(), bytes))
}

/// Attach this turn's images to the last owner message. Returns how many the model will see.
pub async fn attach(messages: &mut [Value], images: &[Media]) -> usize {
    let Some(last) = messages.iter_mut().rev().find(|m| m["role"] == "user") else { return 0 };
    let mut parts = vec![json!({"type": "text", "text": text_of(&last["content"])})];
    let mut shown = 0;
    for media in images {
        let Ok(bytes) = tokio::fs::read(&media.path).await else { continue };
        match viewable(media, bytes) {
            Some((mime, bytes)) => {
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                parts.push(json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{data}")}}));
                shown += 1;
            }
            None => parts.push(json!({"type": "text", "text": format!("(Image {} could not be decoded for viewing.)", media.path.display())})),
        }
    }
    if parts.len() > 1 {
        last["content"] = Value::Array(parts);
    }
    shown
}

/// Text of a message's content (string or parts).
pub fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}
