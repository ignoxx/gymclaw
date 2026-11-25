//! Week helpers over planned sessions: the local week of a timestamp, a week's sessions and lineup,
//! workout names, and fitting a workout into its slot (`allocation`).
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{PlannedSession, SessionStatus};
use crate::domain::compression::compress_template;
use crate::domain::profile::get_profile;
use crate::domain::scheduling::session_data;
use crate::domain::templates::{Template, get_template};
use crate::domain::workout::historical_set_duration;
use crate::error::{Error, Result};
use crate::time::Ts;

/// Monday of the local week containing `stamp`.
pub fn week_of(stamp: Ts, zone: &TimeZone) -> Date {
    let day = stamp.local(zone).date();
    day.checked_sub(jiff::Span::new().days(day.weekday().to_monday_zero_offset() as i64)).expect("date in range")
}

/// Fit the session's workout into its slot: `workout_plan_json` becomes the compressed plan (or an
/// `error` entry if it can't fit), keeping the `crowd_*` keys. Caller saves the session.
pub fn allocation(db: &Connection, session: &mut PlannedSession) -> Result<()> {
    let Some(template_id) = session.workout_template_id.clone() else { return Ok(()) };
    let mut template: Template = match session.plan("template") {
        Some(snapshot) if !snapshot.is_null() => {
            serde_json::from_value(snapshot.clone()).map_err(|e| Error::domain("INVALID_TEMPLATE", e.to_string()))?
        }
        _ => get_template(db, &template_id)?,
    };
    template.set_duration_seconds = historical_set_duration(db, template.set_duration_seconds as f64)?.ceil() as i64;
    let budget = (session.planned_end_at - session.planned_start_at).as_secs();
    let crowd = session.crowd_keys(true);
    session.workout_plan_json.0 = match compress_template(&template, &get_profile(db)?, budget) {
        Ok(plan) => plan,
        Err(error @ Error::Domain { .. }) => {
            json!({"template": template, "budget_seconds": budget, "error": error.code()})
        }
        Err(other) => return Err(other),
    };
    session.merge_plan(crowd);
    Ok(())
}

pub fn workout_name(session: &PlannedSession) -> String {
    if let Some(name) = session.plan("template").and_then(|t| t.get("name")).and_then(Value::as_str) {
        return name.to_string();
    }
    title(session.workout_template_id.as_deref().unwrap_or("Workout"))
}

/// Capitalize each alphabetic run: "leg day" → "Leg Day".
fn title(text: &str) -> String {
    let mut out = String::new();
    let mut previous_alpha = false;
    for c in text.chars() {
        if previous_alpha {
            out.extend(c.to_lowercase())
        } else {
            out.extend(c.to_uppercase())
        }
        previous_alpha = c.is_alphabetic();
    }
    out
}

fn active_sessions(db: &Connection) -> Result<Vec<PlannedSession>> {
    PlannedSession::select(
        db,
        "WHERE status IN ('TENTATIVE', 'COMMITTED', 'STARTED', 'COMPLETED') ORDER BY planned_start_at",
        [],
    )
}

/// The week's planned workouts for owner messages, e.g. "This week: Mon 12:15 Legs · Wed 10:30 Push."
/// Build it after rotation so names match the calendar.
pub fn week_lineup(db: &Connection, week_start: Date, now: Ts) -> Result<String> {
    let tz = get_profile(db)?.tz();
    let sessions: Vec<String> = active_sessions(db)?
        .iter()
        .filter(|s| week_of(s.planned_start_at, &tz) == week_start)
        .map(|s| format!("{} {}", s.planned_start_at.local(&tz).strftime("%a %H:%M"), workout_name(s)))
        .collect();
    let label = if week_start == week_of(now, &tz) {
        "This week".to_string()
    } else {
        format!("Week of {} {}", week_start.strftime("%b"), week_start.day())
    };
    let list = if sessions.is_empty() { "no workouts".to_string() } else { sessions.join(" · ") };
    Ok(format!("{label}: {list}."))
}

/// Every session of the week starting `week_start` (any status).
pub fn get_week(db: &Connection, week_start: Date) -> Result<Value> {
    crate::domain::planning::require_monday(week_start)?;
    let tz = get_profile(db)?.tz();
    let sessions: Vec<Value> = PlannedSession::select(db, "ORDER BY planned_start_at", [])?
        .iter()
        .filter(|s| week_of(s.planned_start_at, &tz) == week_start)
        .map(session_data)
        .collect();
    Ok(json!({"week_start": week_start.to_string(), "sessions": sessions}))
}

pub fn is_closed(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Cancelled | SessionStatus::Skipped | SessionStatus::Missed | SessionStatus::Completed
    )
}
