//! Plan a whole week against persisted profile/calendar state (port of scheduling.py). Owner: lead.
use jiff::civil::Date;
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{AgentEvent, PlannedSession};
use crate::domain::calendar_busy::busy_intervals;
use crate::domain::compression::compress_template;
use crate::domain::core::emit;
use crate::domain::crowd::{CrowdModel, planning_signals};
use crate::domain::planning::{SlotSignal, Inputs, Interval, PlannerConfig, plan_week};
use crate::domain::profile::get_profile;
use crate::domain::templates::get_template;
use crate::domain::workout::historical_set_duration;
use crate::error::Result;
use crate::time::Ts;

/// A planned session as tools and the dashboard show it.
pub fn session_data(row: &PlannedSession) -> Value {
    json!({
        "id": row.id, "week_id": row.week_id, "status": row.status,
        "start": row.planned_start_at, "end": row.planned_end_at, "prep_start": row.prep_start_at,
        "leave_home": row.leave_home_at, "expected_finish": row.expected_finish_at,
        "crowd": row.crowd_prediction, "confidence": row.crowd_confidence, "user_locked": row.user_locked,
        "workout_template_id": row.workout_template_id, "workout_plan": row.workout_plan_json,
    })
}

pub fn interval(session: &PlannedSession) -> Interval {
    Interval { start: session.planned_start_at, end: session.planned_end_at }
}

/// Owner-deleted sessions: the planner never puts a session back there.
pub fn deleted_intervals(db: &Connection) -> Result<Vec<Interval>> {
    Ok(PlannedSession::select(db, "WHERE status = 'CANCELLED' ORDER BY rowid", [])?.iter().filter(|s| s.deleted_by_user()).map(interval).collect())
}

/// Window the planner checks for blockers around a week: Sunday before to Tuesday after, local.
pub fn busy_window(week_start: Date, profile: &crate::domain::profile::Profile) -> Result<(Ts, Ts)> {
    let start = Ts(week_start.checked_sub(jiff::Span::new().days(1))?.to_zoned(profile.tz())?.timestamp());
    let end = Ts(week_start.checked_add(jiff::Span::new().days(8))?.to_zoned(profile.tz())?.timestamp());
    Ok((start, end))
}

/// Fill a week with new sessions around the existing ones. Idempotent per `request_id`. `fixture_signals`
/// replace the crowd model's signals (demos and tests); their crowd values are marked demo.
pub fn schedule_week(db: &Connection, week_start: Date, now: Ts, request_id: Option<&str>, template_id: Option<&str>, fixture_signals: &[SlotSignal]) -> Result<Value> {
    let mut request = json!({"week_start": week_start.to_string(), "fixture": {"busy": [], "unavailable": [], "signals": fixture_signals,
        "config": {"step_minutes": 15, "adherence_weight": 1.0, "preferred_time_weight": 0.4, "uncertainty_weight": 0.5, "shortened_duration_penalty": 1.0}}});
    if let Some(id) = template_id {
        request["template_id"] = json!(id);
    }
    if let Some(key) = request_id
        && let Some(previous) = AgentEvent::first(db, "WHERE correlation_id = ?1", [key])?
    {
        if previous.payload_json.get("request") != Some(&request) {
            crate::bail!("IDEMPOTENCY_CONFLICT", "Request ID already used with different planning input");
        }
        return Ok(previous.payload_json["result"].clone());
    }
    let profile = get_profile(db)?;
    let tz = profile.tz();
    let active = PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED', 'STARTED', 'COMPLETED') ORDER BY rowid", [])?;
    let existing: Vec<Interval> = active.iter().map(interval).collect();
    let (start, end) = busy_window(week_start, &profile)?;
    let busy = busy_intervals(db, start, end)?;
    let deleted = deleted_intervals(db)?;
    let config = PlannerConfig::default();
    let signals = if fixture_signals.is_empty() { planning_signals(db, week_start, now, config.step_minutes)? } else { fixture_signals.to_vec() };
    let model = if signals.is_empty() || !fixture_signals.is_empty() { None } else { Some(CrowdModel::new(db, now)?) };
    let plan = plan_week(&profile, week_start, &Inputs { busy: &busy, unavailable: &deleted, existing: &existing, signals: &signals }, now, &config)?;
    let template = match template_id {
        Some(id) => {
            let mut t = get_template(db, id)?;
            t.set_duration_seconds = historical_set_duration(db, t.set_duration_seconds as f64)?.ceil() as i64;
            Some(t)
        }
        None => None,
    };
    let mut created = Vec::new();
    for slot in &plan.sessions {
        let mut row = PlannedSession::new(week_start.to_string(), template_id.map(String::from), false);
        if let Some(t) = &template {
            row.workout_plan_json.0 = compress_template(t, &profile, (slot.end - slot.start).as_secs())?;
        }
        row.planned_start_at = slot.start;
        row.planned_end_at = slot.end;
        row.prep_start_at = slot.prep_start;
        row.leave_home_at = slot.leave_home;
        row.expected_finish_at = slot.end;
        row.crowd_prediction = slot.crowd;
        row.crowd_confidence = Some(slot.confidence);
        if slot.crowd.is_some() {
            let demo = !fixture_signals.is_empty() || model.as_ref().is_some_and(|m| m.predict(slot.start).demo);
            row.merge_plan(json!({"crowd_source": if demo { "demo_fixture" } else { "local_crowd_model" }, "crowd_score_kind": "personal_perceived_crowd_proxy"}));
        }
        row.insert(db)?;
        created.push(row);
    }
    let week_key = (week_start.iso_week_date().year(), week_start.iso_week_date().week());
    let mut week_rows: Vec<&PlannedSession> = active
        .iter()
        .filter(|s| {
            let d = s.planned_start_at.local(&tz).date().iso_week_date();
            (d.year(), d.week()) == week_key
        })
        .chain(&created)
        .collect();
    week_rows.sort_by_key(|s| s.planned_start_at);
    let result = json!({
        "week_start": week_start.to_string(),
        "sessions": week_rows.iter().map(|s| session_data(s)).collect::<Vec<_>>(),
        "created_count": created.len(),
        "minimum_met": plan.minimum_met,
        "shortfall": plan.shortfall,
        "source": if fixture_signals.is_empty() { "local_state" } else { "fixture" },
        "calendar_events_created": false,
        "choices": plan.sessions,
    });
    let mut event = emit(db, "planning.week_planned", now, json!({"request": request, "result": result}))?;
    if let Some(key) = request_id {
        event.correlation_id = Some(key.into());
        event.update(db)?;
    }
    Ok(result)
}
