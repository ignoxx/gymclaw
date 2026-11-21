//! Photos and image documents for the vision model; CSV documents for the history import.
//!
//! Originals are saved under `data/media/` (EXIF intact, so tools can read the capture date for
//! scale backfill); the model gets an upright JPEG at most `photo::MAX_EDGE` on its longest side.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::providers::photo;
use crate::telegram::Media;

/// Image kind from magic bytes.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        _ if photo::is_heif(bytes) => Some("image/heic"),
        _ => None,
    }
}

/// Save an inbound file as `data/media/<unique id>.<ext>`.
pub async fn save(data_dir: &Path, unique_id: &str, bytes: &[u8]) -> Result<Option<Media>> {
    let Some(mime) = sniff(bytes) else { return Ok(None) };
    let ext = mime.trim_start_matches("image/");
    let dir = data_dir.join("media");
    tokio::fs::create_dir_all(&dir).await?;
    let path: PathBuf = dir.join(format!("{}.{ext}", safe_name(unique_id)));
    tokio::fs::write(&path, bytes).await?;
    Ok(Some(Media { path, mime: mime.into() }))
}

/// A CSV document: another app's workout export.
pub fn is_csv(doc: &crate::telegram::Document) -> bool {
    doc.file_name.as_deref().is_some_and(|n| n.to_lowercase().ends_with(".csv"))
        || doc.mime_type.as_deref().is_some_and(|m| m == "text/csv" || m == "text/comma-separated-values")
}

/// Save an inbound CSV as `data/imports/<unique id>.csv`.
pub async fn save_import(data_dir: &Path, unique_id: &str, bytes: &[u8]) -> Result<PathBuf> {
    let dir = data_dir.join("imports");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}.csv", safe_name(unique_id)));
    tokio::fs::write(&path, bytes).await?;
    Ok(path)
}

/// Telegram's file id as a file name: only `[A-Za-z0-9_-]`.
fn safe_name(unique_id: &str) -> String {
    unique_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

/// Bytes the vision model can read: JPEG, PNG and HEIC as an upright, downscaled JPEG; WebP and GIF
/// (which `photo` doesn't decode) as is. None when a JPEG/PNG/HEIC can't be decoded.
fn viewable(media: &Media, bytes: Vec<u8>) -> Option<(String, Vec<u8>)> {
    match photo::to_jpeg(&bytes, photo::MAX_EDGE) {
        Ok(jpeg) => Some(("image/jpeg".into(), jpeg.jpeg)),
        Err(_) if matches!(media.mime.as_str(), "image/webp" | "image/gif") => Some((media.mime.clone(), bytes)),
        Err(_) => None,
    }
}

/// Attach this turn's images to the last owner message, each distinct picture once (the same
/// screenshot sent twice costs vision tokens twice and reads as two pages). Returns how many the
/// model will see.
pub async fn attach(messages: &mut [Value], images: &[Media]) -> usize {
    let Some(last) = messages.iter_mut().rev().find(|m| m["role"] == "user") else { return 0 };
    let mut parts = vec![json!({"type": "text", "text": text_of(&last["content"])})];
    let mut shown = 0;
    let mut seen = HashSet::new();
    for media in images {
        let Ok(bytes) = tokio::fs::read(&media.path).await else { continue };
        if !seen.insert(Sha256::digest(&bytes)) {
            continue;
        }
        let owned = media.clone();
        let decoded = tokio::task::spawn_blocking(move || viewable(&owned, bytes)).await.ok().flatten();
        match decoded {
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

#[cfg(test)]
mod tests {
    use super::*;

    const OFFSET_JPG: &[u8] = include_bytes!("../providers/testdata/photo/offset.jpg");

    fn media(mime: &str) -> Media {
        Media { path: PathBuf::new(), mime: mime.into() }
    }

    #[test]
    fn photos_reach_the_model_upright() {
        let (mime, jpeg) = viewable(&media("image/jpeg"), OFFSET_JPG.to_vec()).unwrap();
        let shown = photo::to_jpeg(&jpeg, photo::MAX_EDGE).unwrap();
        assert_eq!((mime.as_str(), shown.width, shown.height), ("image/jpeg", 48, 64));
        assert_eq!(viewable(&media("image/gif"), b"GIF89a".to_vec()).unwrap().0, "image/gif");
        assert!(viewable(&media("image/jpeg"), vec![0xFF, 0xD8, 0xFF, 0]).is_none());
    }

    #[cfg(feature = "heic")]
    #[test]
    fn heic_is_shown_as_jpeg() {
        let heic = include_bytes!("../providers/testdata/photo/offset.heic");
        assert_eq!(sniff(heic), Some("image/heic"));
        assert_eq!(viewable(&media("image/heic"), heic.to_vec()).unwrap().0, "image/jpeg");
    }
}
