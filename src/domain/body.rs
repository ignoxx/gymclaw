//! Body weight log (port of gymclaw/services/body.py). Owner: WP-train.
//!
//! Weigh-ins read from scale photos (or typed), a weekly trend and a quiet nudge. The agent reads the
//! number with its own vision; for old photos it passes the file, and the capture date comes from the
//! photo's EXIF data. Telegram strips EXIF from compressed photos, so backfills must be sent as files.
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use jiff::tz::{Offset, TimeZone};
use jiff::{SignedDuration, Timestamp, civil};
use regex::bytes::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::BodyWeight;
use crate::db::new_id;
use crate::domain::core::mutate;
use crate::domain::profile::get_profile;
use crate::domain::workout::g;
use crate::error::{Error, Result};
use crate::time::Ts;

/// Backfilled entries with only a date get a typical morning weigh-in time.
const DATE_ONLY_HOUR: i8 = 8;
/// No weigh-in for this long: the Sunday briefing asks for one. Never a separate message.
const NUDGE_AFTER_DAYS: i64 = 7;
static EXIF_STAMP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{4}):(\d{2}):(\d{2}) (\d{2}):(\d{2}):(\d{2})").unwrap());

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WeighIn {
    /// The weight on the scale, 20–400 kg.
    pub kg: f64,
    /// When: a date ("2026-03-02", logged at 08:00 local) or a time ("2026-03-02T07:10"; owner's local
    /// time unless it has an offset). Neither this nor a photo: now.
    #[serde(default)]
    pub at: Option<String>,
    /// Local path of a scale photo sent as a file; its capture date becomes the time.
    #[serde(default)]
    pub photo: Option<PathBuf>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::domain("INVALID_INPUT", message)
}

fn ascii(field: &exif::Field) -> Option<String> {
    match &field.value {
        exif::Value::Ascii(parts) => parts.first().map(|p| String::from_utf8_lossy(p).trim_matches(['\0', ' ']).to_string()),
        _ => None,
    }
}

/// Capture time from EXIF. HEIC and other containers the EXIF reader can't parse: the first EXIF-style
/// stamp in the header.
pub fn photo_taken_at(path: &Path, zone: &TimeZone) -> Result<Ts> {
    let parsed = std::fs::File::open(path).ok().and_then(|f| exif::Reader::new().read_from_container(&mut std::io::BufReader::new(f)).ok());
    let (stamp, offset) = match parsed {
        Some(data) => {
            let get = |tag| data.get_field(tag, exif::In::PRIMARY).and_then(ascii).filter(|s| !s.is_empty());
            (get(exif::Tag::DateTimeOriginal).or_else(|| get(exif::Tag::DateTime)), get(exif::Tag::OffsetTimeOriginal))
        }
        None => {
            let mut head = std::fs::read(path)?;
            head.truncate(1 << 18);
            (EXIF_STAMP.find(&head).map(|m| String::from_utf8_lossy(m.as_bytes()).into_owned()), None)
        }
    };
    let Some(stamp) = stamp else {
        crate::bail!(
            "PHOTO_DATE_UNKNOWN",
            "Photo has no capture date (Telegram strips it from compressed photos). Ask for the original sent as a file, or pass the date with --at."
        );
    };
    let taken = civil::DateTime::strptime("%Y:%m:%d %H:%M:%S", &stamp).map_err(|_| invalid(format!("Unreadable photo date: {stamp}")))?;
    let zone = offset.and_then(|o| parse_offset(&o)).map_or_else(|| zone.clone(), TimeZone::fixed);
    Ok(Ts(taken.to_zoned(zone)?.timestamp()))
}

/// "+02:00" / "-0530" → offset.
fn parse_offset(text: &str) -> Option<Offset> {
    let (sign, rest) = match text.trim().split_at_checked(1)? {
        ("+", rest) => (1, rest),
        ("-", rest) => (-1, rest),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 {
        return None;
    }
    let (hours, minutes): (i32, i32) = (digits[..2].parse().ok()?, digits[2..].parse().ok()?);
    Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).ok()
}

fn measured_at(entry: &WeighIn, now: Ts, zone: &TimeZone) -> Result<Ts> {
    if !(20.0..=400.0).contains(&entry.kg) {
        return Err(invalid("kg must be 20–400"));
    }
    let at = match (&entry.photo, &entry.at) {
        (Some(_), Some(_)) => return Err(invalid("Give a photo or a time, not both")),
        (Some(photo), None) => photo_taken_at(photo, zone)?,
        (None, Some(at)) if at.len() == 10 => {
            let date: civil::Date = at.parse().map_err(|_| invalid(format!("Not a date: {at}")))?;
            Ts(date.at(DATE_ONLY_HOUR, 0, 0, 0).to_zoned(zone.clone())?.timestamp())
        }
        (None, Some(at)) => Ts::parse(at, Some(zone))?,
        (None, None) => now,
    };
    if at > now.mins(5) || at.local(zone).year() < 2000 {
        crate::bail!("WEIGH_IN_DATE_INVALID", "Weigh-in date {} is in the future or implausible", at.local(zone).strftime("%Y-%m-%d"));
    }
    Ok(Ts(Timestamp::from_second(at.0.as_second().div_euclid(60) * 60)?))
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn signed(value: f64) -> String {
    if value < 0.0 { g(value) } else { format!("+{}", g(value)) }
}

/// Save one or more weigh-ins (a whole backfill in one call). Same time as a saved entry: skipped as a duplicate.
pub fn log(db: &Connection, entries: &[WeighIn], now: Ts, request_id: &str) -> Result<Value> {
    if entries.is_empty() {
        return Err(invalid("No weigh-ins given"));
    }
    mutate(db, "body.weight_logged", request_id, json!({"entries": entries}), now, || {
        let zone = get_profile(db)?.tz();
        let (mut logged, mut duplicates) = (vec![], vec![]);
        for entry in entries {
            let at = measured_at(entry, now, &zone)?;
            if BodyWeight::first(db, "WHERE measured_at = ?1", [at])?.is_some() {
                duplicates.push(json!({"kg": entry.kg, "measured_at": at}));
                continue;
            }
            let row = BodyWeight { id: new_id(), measured_at: at, kg: round1(entry.kg), created_at: now };
            row.insert(db)?;
            logged.push(entry_data(&row, &zone));
        }
        let summary = trend(db, now)?;
        // Echo the number back so a misread scale gets caught right away.
        let mut hint = match logged.len() {
            0 => "Already logged.".to_string(),
            1 => format!("{} kg logged", g(logged[0]["kg"].as_f64().unwrap_or_default())),
            n => format!("{n} weigh-ins logged"),
        };
        if let (Some(change), false) = (summary["change_kg"].as_f64(), logged.is_empty()) {
            hint += &format!(" · {} kg vs the week before", signed(change));
        }
        Ok(json!({"logged": logged, "duplicates": duplicates, "trend": summary, "instruction": hint}))
    })
}

/// Remove a misread or mistyped weigh-in.
pub fn delete(db: &Connection, entry_id: &str, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "body.weight_deleted", request_id, json!({"entry_id": entry_id}), now, || {
        let Some(row) = BodyWeight::get(db, entry_id)? else { crate::bail!("WEIGH_IN_NOT_FOUND", "Unknown weigh-in") };
        let data = entry_data(&row, &get_profile(db)?.tz());
        row.delete(db)?;
        Ok(json!({"deleted": data}))
    })
}

pub fn entry_data(row: &BodyWeight, zone: &TimeZone) -> Value {
    json!({"id": row.id, "kg": row.kg, "measured_at": row.measured_at, "local_date": row.measured_at.local(zone).date().to_string()})
}

/// All weigh-ins, oldest first.
pub fn entries(db: &Connection) -> Result<Vec<BodyWeight>> {
    BodyWeight::select(db, "ORDER BY measured_at", [])
}

/// Latest weigh-in, and the 7-day average versus the 7 days before it. Daily weight swings by 1–2 kg,
/// so weeks are compared, not single readings. With weekly logging it's simply last vs previous.
pub fn trend(db: &Connection, now: Ts) -> Result<Value> {
    let rows: Vec<BodyWeight> = entries(db)?.into_iter().filter(|r| r.measured_at <= now).collect();
    let Some(latest) = rows.last() else { return Ok(json!({"latest": null, "change_kg": null, "days_since": null})) };
    let week_ago = latest.measured_at - SignedDuration::from_hours(24 * 7);
    let two_weeks_ago = latest.measured_at - SignedDuration::from_hours(24 * 14);
    let week: Vec<f64> = rows.iter().filter(|r| week_ago < r.measured_at).map(|r| r.kg).collect();
    let before: Vec<f64> = rows.iter().filter(|r| two_weeks_ago < r.measured_at && r.measured_at <= week_ago).map(|r| r.kg).collect();
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let change = (!before.is_empty()).then(|| round1(mean(&week) - mean(&before)));
    let days_since = (now - latest.measured_at).as_secs().div_euclid(86400);
    Ok(json!({"latest": entry_data(latest, &get_profile(db)?.tz()), "change_kg": change, "days_since": days_since}))
}

/// Weekly briefing line about weight trend, or None without data.
pub fn briefing_line(db: &Connection, now: Ts) -> Result<Option<String>> {
    let summary = trend(db, now)?;
    let Some(kg) = summary["latest"]["kg"].as_f64() else {
        return Ok(Some("⚖️ Snap a photo of your scale anytime and I'll track your weight.".into()));
    };
    let days = summary["days_since"].as_i64().unwrap_or_default();
    if days >= NUDGE_AFTER_DAYS {
        return Ok(Some(format!("⚖️ Last weigh-in {days} days ago. Send a scale photo when you get a chance.")));
    }
    let change = summary["change_kg"].as_f64().map(|c| format!(" · {} kg vs the week before", signed(c))).unwrap_or_default();
    Ok(Some(format!("⚖️ {} kg{change}", g(kg))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workout::tests::db;

    fn now() -> Ts {
        Ts::parse("2026-10-11T17:00:00+00:00", None).unwrap()
    }

    fn weigh(kg: f64, at: Option<&str>, photo: Option<PathBuf>) -> WeighIn {
        WeighIn { kg, at: at.map(String::from), photo }
    }

    #[test]
    fn backfill_uses_photo_dates_and_reimport_is_a_noop() {
        let db = db();
        // HEIC from an iPhone: no parsable container here, the EXIF stamp is still in the header.
        let heic = std::env::temp_dir().join(format!("{}.heic", new_id()));
        std::fs::write(&heic, b"\x00\x00\x00\x18ftypheic....Exif\x00\x002026:10:05 07:40:12\x00").unwrap();
        let entries = [weigh(83.0, Some("2026-09-28T07:05:00+02:00"), None), weigh(82.4, None, Some(heic)), weigh(83.4, Some("2026-09-27"), None)];
        let out = log(&db, &entries, now(), "tg-1").unwrap();
        let times: Vec<&str> = out["data"]["logged"].as_array().unwrap().iter().map(|e| e["measured_at"].as_str().unwrap()).collect();
        assert_eq!(times, ["2026-09-28T05:05:00+00:00", "2026-10-05T05:40:00+00:00", "2026-09-27T06:00:00+00:00"]);
        // 7 days up to the Oct 5 weigh-in: 82.4. The 7 days before: (83.0 + 83.4) / 2.
        assert_eq!(out["user_message_hint"], "3 weigh-ins logged · -0.8 kg vs the week before");
        let again = log(&db, &entries, now(), "tg-2").unwrap();
        assert_eq!((again["data"]["logged"].as_array().unwrap().len(), again["data"]["duplicates"].as_array().unwrap().len()), (0, 3));
    }

    #[test]
    fn briefing_line_shows_trend_then_nudges_when_stale() {
        let db = db();
        log(&db, &[weigh(83.1, Some("2026-10-03T17:00:00+00:00"), None), weigh(82.5, None, None)], now(), "r").unwrap();
        assert_eq!(briefing_line(&db, now()).unwrap().unwrap(), "⚖️ 82.5 kg · -0.6 kg vs the week before");
        let later = now() + SignedDuration::from_hours(24 * 9);
        assert_eq!(briefing_line(&db, later).unwrap().unwrap(), "⚖️ Last weigh-in 9 days ago. Send a scale photo when you get a chance.");
    }
}
