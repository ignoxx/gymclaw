//! Session previews: plan lines and the exercise grid image (port of gymclaw/services/preview.py). Owner: WP-train.
//!
//! Used when the owner asks "what's on today?". One photo with every exercise's illustration, sent
//! without a caption (the plan lines go to chat history), instead of a stack of messages; rendered
//! once per plan content under `data/previews/`.
use std::path::{Path, PathBuf};

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use image::{Rgb, RgbImage, imageops::FilterType};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::db::Connection;
use crate::db::models::{PlannedSession, SetLog};
use crate::domain::illustrations::for_exercise;
use crate::domain::profile::get_profile;
use crate::domain::templates::get_template;
use crate::domain::view::{Card, CardKind};
use crate::domain::workout::g;
use crate::error::{Error, Result};
use crate::time::Ts;

const TILE: u32 = 340;
const LABEL: u32 = 70;
const GAP: u32 = 24;
const COLUMNS: u32 = 3;
const FONT_SIZE: f32 = 30.0;

/// Most recent working set of a template exercise id, for "last time" hints.
pub fn last_set(db: &Connection, exercise_id: &str) -> Result<Option<SetLog>> {
    SetLog::first(
        db,
        "WHERE set_type = 'WORKING' AND workout_exercise_id IN (SELECT id FROM workout_exercise WHERE exercise_id = ?1) ORDER BY logged_at DESC",
        [exercise_id],
    )
}

/// One line per planned exercise ('1. Barbell row · 2 sets of 8–12 · last 10 × 60 kg'), from the
/// current template; used in previews and calendar event bodies.
pub fn plan_lines(db: &Connection, session: &PlannedSession) -> Result<Vec<String>> {
    let template = get_template(db, session.workout_template_id.as_deref().unwrap_or_default())?;
    let sets = session.workout_plan_json.get("sets");
    let mut lines = vec![];
    for (index, spec) in template.exercises.iter().enumerate() {
        let reps = if spec.rep_min == spec.rep_max { spec.rep_min.to_string() } else { format!("{}–{}", spec.rep_min, spec.rep_max) };
        let count = sets.and_then(|s| s.get(&spec.id)).and_then(Value::as_i64).unwrap_or(spec.working_sets);
        let last = last_set(db, &spec.id)?.map(|l| format!(" · last {} × {} kg", l.reps, g(l.weight))).unwrap_or_default();
        lines.push(format!("{}. {} · {count} sets of {reps}{last}", index + 1, spec.name));
    }
    Ok(lines)
}

/// Card for a planned session (default: the next one), using the current template.
pub fn session_preview(db: &Connection, now: Ts, planned_session_id: Option<&str>, data_dir: &Path) -> Result<Card> {
    let session = match planned_session_id {
        Some(id) => PlannedSession::get(db, id)?,
        None => PlannedSession::first(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') AND planned_end_at > ?1 ORDER BY planned_start_at", [now])?,
    };
    let Some(session) = session.filter(|s| s.workout_template_id.is_some()) else {
        crate::bail!("NO_PLANNED_SESSION", "No upcoming session with a workout plan");
    };
    let template = get_template(db, session.workout_template_id.as_deref().unwrap_or_default())?;
    let start = session.planned_start_at.local(&get_profile(db)?.tz());
    let crowd = session.workout_plan_json.pointer("/crowd_forecast/feel").and_then(Value::as_str).filter(|f| !f.is_empty());
    let mut lines = vec![format!("**{}** · {}{}", template.name, start.strftime("%a %H:%M"), crowd.map(|c| format!(" · {c}")).unwrap_or_default())];
    lines.extend(plan_lines(db, &session)?);
    let items: Vec<(String, String)> = template.exercises.iter().filter_map(|s| for_exercise(&s.name, s.guide_id.as_deref()).map(|p| (s.name.clone(), p.telegram_png))).collect();
    let photo = if items.is_empty() { None } else { Some(render_grid(&items, &data_dir.join("previews"))?) };
    let kind = if photo.is_some() { CardKind::Preview } else { CardKind::Other };
    Ok(Card { text: lines.join("\n"), photo, buttons: vec![], rest_until: None, kind })
}

/// Python `json.dumps` escaping (ASCII only), so keys and fingerprints match the old app.
pub(crate) fn ensure_ascii(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// DM Sans, embedded so previews never depend on the deploy layout.
fn font() -> Result<FontVec> {
    FontVec::try_from_vec(include_bytes!("../../web/fonts/DM-Sans.ttf").to_vec()).map_err(|e| Error::domain("ASSET_MISSING", format!("Bad font: {e}")))
}

/// items: (label, png path). Returns a cached PNG path under `root`; same content renders once.
pub fn render_grid(items: &[(String, String)], root: &Path) -> Result<String> {
    let dumped = items.iter().map(|(l, p)| format!("[{}, {}]", serde_json::to_string(l).unwrap_or_default(), serde_json::to_string(p).unwrap_or_default())).collect::<Vec<_>>().join(", ");
    let key = hex::encode(Sha256::digest(ensure_ascii(&format!("[{dumped}]")).as_bytes()));
    let path: PathBuf = root.join(&key[..16]).join("preview.png");
    if path.exists() {
        return Ok(path.to_string_lossy().into());
    }
    let rows = (items.len() as u32).div_ceil(COLUMNS);
    let width = COLUMNS * TILE + (COLUMNS + 1) * GAP;
    let height = rows * (TILE + LABEL) + (rows + 1) * GAP;
    let mut canvas = RgbImage::new(width, height);
    let font = font()?;
    // PIL sizes fonts by em; ab_glyph's scale is ascent - descent.
    let scale = PxScale::from(FONT_SIZE * font.height_unscaled() / font.units_per_em().unwrap_or(1000.0));
    let scaled = font.as_scaled(scale);
    let text_width = |text: &str| text.chars().map(|c| scaled.h_advance(scaled.glyph_id(c))).sum::<f32>();
    for (index, (label, png)) in items.iter().enumerate() {
        let x = GAP + (index as u32 % COLUMNS) * (TILE + GAP);
        let y = GAP + (index as u32 / COLUMNS) * (TILE + LABEL + GAP);
        let art = image::open(png).map_err(|e| Error::domain("ASSET_MISSING", format!("{png}: {e}")))?.to_rgb8();
        // Telegram padding isn't needed inside the grid.
        let margin = art.width() / 12;
        let cropped = image::imageops::crop_imm(&art, margin, margin, art.width() - 2 * margin, art.height() - 2 * margin).to_image();
        image::imageops::replace(&mut canvas, &image::imageops::resize(&cropped, TILE, TILE, FilterType::CatmullRom), x as i64, y as i64);
        let mut text: Vec<char> = format!("{}. {label}", index + 1).chars().collect();
        while text_width(&text.iter().collect::<String>()) > TILE as f32 && text.len() > 4 {
            text.truncate(text.len() - 2);
            text.push('…');
        }
        let text: String = text.into_iter().collect();
        // Anchor "ma": centered horizontally, ascender at y + TILE + 18.
        let mut pen = x as f32 + TILE as f32 / 2.0 - text_width(&text) / 2.0;
        let baseline = (y + TILE + 18) as f32 + scaled.ascent();
        for c in text.chars() {
            let glyph = scaled.scaled_glyph(c);
            let advance = scaled.h_advance(glyph.id);
            if let Some(outline) = font.outline_glyph(ab_glyph::Glyph { position: point(pen, baseline), ..glyph }) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, coverage| {
                    let (px, py) = (bounds.min.x as i64 + gx as i64, bounds.min.y as i64 + gy as i64);
                    if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < height {
                        let pixel = canvas.get_pixel_mut(px as u32, py as u32);
                        *pixel = Rgb(pixel.0.map(|v| (v as f32 * (1.0 - coverage) + 255.0 * coverage).round() as u8));
                    }
                });
            }
            pen += advance;
        }
    }
    std::fs::create_dir_all(path.parent().expect("preview dir"))?;
    canvas.save(&path).map_err(|e| Error::domain("IO_ERROR", format!("{}: {e}", path.display())))?;
    Ok(path.to_string_lossy().into())
}

