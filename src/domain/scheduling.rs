//! Draft a whole week against persisted profile/calendar state.
use std::collections::HashMap;

use jiff::civil::Date;
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{AgentEvent, OnboardingState, PlannedSession, SessionStatus, WorkoutSession};
use crate::domain::calendar::allocation;
use crate::domain::calendar_busy::busy_intervals;
use crate::domain::calendar_writes::queue_session_write;
use crate::domain::core::emit;
use crate::domain::crowd::planning_signals;
use crate::domain::planning::{Inputs, Interval, SlotSignal, plan_week};
use crate::domain::profile::{Profile, get_profile};
use crate::domain::replanning::{commit_upcoming, place};
use crate::error::Result;
use crate::time::Ts;

/// A planned session as tools and the dashboard show it.
pub fn session_data(row: &PlannedSession) -> Value {
    json!({
        "id": row.id, "week_id": row.week_id, "status": row.status,
        "start": row.planned_start_at, "end": row.planned_end_at, "prep_start": row.prep_start_at,
        "leave_home": row.leave_home_at, "user_locked": row.user_locked,
        "workout_template_id": row.workout_template_id, "workout_plan": row.workout_plan_json, "follows_session_id": row.follows_session_id,
    })
}

pub fn interval(session: &PlannedSession) -> Interval {
    Interval { start: session.planned_start_at, end: session.planned_end_at }
}

/// What the planner works around in `week`: busy time from Sunday before to Tuesday after (calendar,
/// personal feed, availability blocks) and owner-deleted slots, where it never puts a session back, as
/// blockers; every active session for recovery and the weekly count; skipped ones for the count only.
/// No crowd signals.
pub fn planner_inputs(db: &Connection, week_start: Date, profile: &Profile) -> Result<Inputs> {
    let start = Ts(week_start.checked_sub(jiff::Span::new().days(1))?.to_zoned(profile.tz())?.timestamp());
    let end = Ts(week_start.checked_add(jiff::Span::new().days(8))?.to_zoned(profile.tz())?.timestamp());
    let mut inputs = Inputs { busy: busy_intervals(db, start, end)?, ..Inputs::default() };
    for session in PlannedSession::select(db, "ORDER BY rowid", [])? {
        match session.status {
            status if status.is_active() => inputs.existing.push(interval(&session)),
            SessionStatus::Skipped => inputs.skipped.push(interval(&session)),
            SessionStatus::Cancelled if session.deleted_by_user() => inputs.busy.push(interval(&session)),
            _ => {}
        }
    }
    Ok(inputs)
}

/// What `draft_week` added to a week.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Draft {
    /// The new sessions, in start order. Results stored before this field existed replay as none: their
    /// callers only replay keys of weeks that are still empty.
    #[serde(default)]
    pub created: Vec<String>,
    pub minimum_met: bool,
    pub shortfall: i64,
}

/// Fill a week with new sessions around the existing ones, none preparing before `not_before` (default
/// `now`). Each gets `template_id`, or with `rotate` the default rotation (`rotation`); then it is
/// allocated, committed when within 48 h and queued for the calendar. Idempotent per `request_id` (the
/// same week and template). `fixture_signals` replace the crowd model's signals (tests).
#[allow(clippy::too_many_arguments)]
pub fn draft_week(
    db: &Connection,
    week_start: Date,
    now: Ts,
    not_before: Option<Ts>,
    request_id: Option<&str>,
    template_id: Option<&str>,
    rotate: bool,
    fixture_signals: &[SlotSignal],
) -> Result<Draft> {
    let request = json!({"week_start": week_start.to_string(), "template_id": template_id});
    if let Some(key) = request_id
        && let Some(previous) = AgentEvent::first(db, "WHERE correlation_id = ?1", [key])?
    {
        // Older events also stored the planner's settings; only the week and template make the request.
        let same = |field: &str| previous.payload_json["request"].get(field).unwrap_or(&Value::Null) == &request[field];
        if !same("week_start") || !same("template_id") {
            crate::bail!("IDEMPOTENCY_CONFLICT", "Request ID already used with different planning input");
        }
        return Ok(serde_json::from_value(previous.payload_json["result"].clone())?);
    }
    let profile = get_profile(db)?;
    let mut inputs = planner_inputs(db, week_start, &profile)?;
    inputs.signals =
        if fixture_signals.is_empty() { planning_signals(db, week_start, now)? } else { fixture_signals.to_vec() };
    let plan = plan_week(&profile, week_start, &inputs, not_before.map_or(now, |t| t.max(now)))?;
    let mut created: Vec<PlannedSession> = plan
        .sessions
        .iter()
        .map(|slot| {
            let mut row = PlannedSession::new(week_start.to_string(), template_id.map(String::from), false);
            place(&mut row, slot.start, slot.end, &profile);
            row
        })
        .collect();
    if rotate {
        rotation(db, &mut created)?;
    }
    for row in &mut created {
        allocation(db, row)?;
        row.insert(db)?;
    }
    commit_upcoming(db, now)?;
    for row in &created {
        if let Some(session) = PlannedSession::get(db, &row.id)? {
            queue_session_write(db, &session, now, false)?;
        }
    }
    let draft = Draft {
        created: created.into_iter().map(|s| s.id).collect(),
        minimum_met: plan.minimum_met,
        shortfall: plan.shortfall,
    };
    let mut event = emit(db, "planning.week_planned", now, json!({"request": request, "result": draft}))?;
    if let Some(key) = request_id {
        event.correlation_id = Some(key.into());
        event.update(db)?;
    }
    Ok(draft)
}

/// Give each `new` session (not saved yet) the default workout: the split template (e.g. Push / Pull / Legs)
/// trained least recently, counting started workouts (a running one too) and the sessions before it. Only
/// a default for fresh slots: every other session keeps its workout, since which workout goes where is the
/// agent's call (re-rotating every tick undid its picks after a skip). Without a split of two or more, the
/// new sessions keep the template they were drafted with.
pub(crate) fn rotation(db: &Connection, new: &mut [PlannedSession]) -> Result<()> {
    let split = OnboardingState::get(db, 1)?.map(|o| o.split_json.0).unwrap_or_default();
    if split.len() < 2 {
        return Ok(());
    }
    let mut last: HashMap<String, Ts> = HashMap::new();
    for workout in WorkoutSession::select(db, "WHERE template_id IS NOT NULL AND started_at IS NOT NULL", [])? {
        let (Some(template), Some(at)) = (workout.template_id, workout.started_at) else { continue };
        let entry = last.entry(template).or_insert(at);
        *entry = (*entry).max(at);
    }
    // Saved sessions (their workout) and new ones (their index), in start order.
    let mut slots: Vec<(Ts, Option<String>, Option<usize>)> =
        PlannedSession::select(db, "WHERE status IN ('TENTATIVE', 'COMMITTED') ORDER BY planned_start_at, id", [])?
            .into_iter()
            .map(|s| (s.planned_start_at, s.workout_template_id, None))
            .collect();
    slots.extend(new.iter().enumerate().map(|(i, s)| (s.planned_start_at, None, Some(i))));
    slots.sort_by_key(|(at, _, _)| *at);
    for (at, template, index) in slots {
        let Some(index) = index else {
            if let Some(template) = template {
                last.insert(template, at);
            }
            continue;
        };
        // Never-trained workouts go first (None < Some), ties in split order.
        let wanted = split
            .iter()
            .enumerate()
            .min_by_key(|(i, t)| (last.get(*t).copied(), *i))
            .map(|(_, t)| t.clone())
            .expect("split has entries");
        last.insert(wanted.clone(), at);
        new[index].workout_template_id = Some(wanted);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Json;
    use crate::db::models::{CalendarEventSnapshot, SessionStatus};

    fn ts(text: &str) -> Ts {
        Ts::parse(text, None).unwrap()
    }

    #[test]
    fn calendar_snapshot_and_locked_session_preserved() {
        let db = crate::db::test::db();
        get_profile(&db).unwrap();
        let (start, end) = (ts("2026-10-15T19:00:00+02:00"), ts("2026-10-15T20:10:00+02:00"));
        CalendarEventSnapshot {
            calendar_event_id: "travel".into(),
            etag: None,
            title: "Travel".into(),
            start_at: ts("2026-10-12T00:00:00+02:00"),
            end_at: ts("2026-10-14T00:00:00+02:00"),
            status: "confirmed".into(),
            gymclaw_managed: false,
            gymclaw_session_id: None,
            user_locked: false,
            raw_json: Json(json!({})),
        }
        .insert(&db)
        .unwrap();
        let mut locked = PlannedSession::new("2026-10-12".into(), None, true);
        locked.id = "locked".into();
        locked.status = SessionStatus::Tentative;
        (locked.planned_start_at, locked.planned_end_at, locked.prep_start_at, locked.leave_home_at) =
            (start, end, start, start);
        locked.insert(&db).unwrap();
        let week = jiff::civil::date(2026, 10, 12);
        let draft = draft_week(&db, week, ts("2026-10-11T12:00:00+02:00"), None, None, None, false, &[]).unwrap();
        assert!(draft.created.is_empty(), "{draft:?}");
        assert_eq!(PlannedSession::get(&db, "locked").unwrap().unwrap().planned_start_at, start);
    }

    /// A skipped session counts toward its week: planning the week again doesn't make it up.
    #[test]
    fn skipped_slot_is_not_refilled() {
        let db = crate::db::test::with_short_template();
        let (week, now) = (jiff::civil::date(2026, 10, 12), ts("2026-10-11T12:00:00+02:00"));
        let first = draft_week(&db, week, now, None, None, Some("short"), false, &[]).unwrap();
        let mut skipped = PlannedSession::get(&db, &first.created[0]).unwrap().unwrap();
        skipped.status = SessionStatus::Skipped;
        skipped.update(&db).unwrap();
        assert!(draft_week(&db, week, now, None, None, Some("short"), false, &[]).unwrap().created.is_empty());
    }

    /// Keys stored before the request shrank to week and template, with a result listing the week's
    /// sessions, still replay.
    #[test]
    fn replays_a_key_stored_by_an_older_release() {
        let db = crate::db::test::with_short_template();
        let request = json!({"week_start": "2026-10-12", "template_id": "short", "fixture": {"busy": [], "signals": [],
            "config": {"step_minutes": 15}}});
        let result = json!({"sessions": [], "created_count": 0, "minimum_met": false, "shortfall": 3, "choices": []});
        let now = ts("2026-10-11T12:00:00Z");
        let mut event = emit(&db, "planning.week_planned", now, json!({"request": request, "result": result})).unwrap();
        event.correlation_id = Some("rolling:old".into());
        event.update(&db).unwrap();
        let week = jiff::civil::date(2026, 10, 12);
        let replay = draft_week(&db, week, now, None, Some("rolling:old"), Some("short"), true, &[]).unwrap();
        assert!(replay.created.is_empty() && replay.shortfall == 3);
        let other = draft_week(&db, week, now, None, Some("rolling:old"), None, true, &[]).unwrap_err();
        assert_eq!(other.code(), "IDEMPOTENCY_CONFLICT");
        assert!(PlannedSession::select(&db, "", []).unwrap().is_empty());
    }
}
