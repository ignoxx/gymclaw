//! Read-only projection for the dashboard (port of gymclaw/dashboard_state.py). Same JSON, so the
//! existing frontend (`web/`) works unchanged.
use std::collections::{BTreeMap, HashMap};

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, ToSpan};
use serde_json::{Value, json};

use crate::db::Connection;
use crate::db::models::{
    AgentEvent, BodyWeight, CrowdFeedback, CrowdObservation, NoteKind, PlannedSession, RuntimeSettings, SetLog, SetType, UserProfile, WorkoutSession,
    WorkoutTemplate,
};
use crate::domain::profile::{Profile, get_profile};
use crate::domain::templates::get_template;
use crate::domain::{crowd, illustrations, preview, workout};
use crate::error::Result;
use crate::time::{Ts, iso};

const ACTIVITY: [(&str, &str); 9] = [
    ("workout.started", "Workout started"),
    ("workout.completed", "Workout finished"),
    ("workout.set_logged", "Set logged"),
    ("workout.substituted", "Exercise swapped"),
    ("planning.week_planned", "Week planned"),
    ("planning.rotation_assigned", "Split rotated"),
    ("calendar.write_applied", "Calendar updated"),
    ("notifications.sent", "Reminder sent"),
    ("crowd.feedback_received", "Crowd rated"),
];

fn hhmm(at: Ts, tz: &TimeZone) -> String {
    at.local(tz).strftime("%H:%M").to_string()
}

fn fmt(at: Ts, tz: &TimeZone, pattern: &str) -> String {
    at.local(tz).strftime(pattern).to_string()
}

fn date_fmt(d: Date, pattern: &str) -> String {
    d.strftime(pattern).to_string()
}

/// Python `f"{x:g}"` for weights: 40.0 → "40", 22.5 → "22.5".
pub fn g(x: f64) -> String {
    let text = format!("{x}");
    text.strip_suffix(".0").map(String::from).unwrap_or(text)
}

/// Python's `round(x, digits)` (half to even).
fn round(x: f64, digits: i32) -> f64 {
    let scale = 10f64.powi(digits);
    (x * scale).round_ties_even() / scale
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

fn svg(name: &str, guide_id: Option<&str>) -> Option<String> {
    illustrations::for_exercise(name, guide_id).map(|e| e.svg_url)
}

fn start_of(day: Date, tz: &TimeZone) -> Result<Ts> {
    Ok(Ts(day.to_zoned(tz.clone())?.timestamp()))
}

/// The next seven days from today, each with its session (if any).
fn days(db: &Connection, tz: &TimeZone, today: Date) -> Result<Vec<Value>> {
    let start = start_of(today, tz)?;
    let end = start + SignedDuration::from_hours(24 * 7);
    let rows = PlannedSession::select(db, "WHERE planned_start_at >= ?1 AND planned_start_at < ?2 AND status NOT IN ('CANCELLED', 'MISSED') ORDER BY planned_start_at", [start, end])?;
    let by_day: HashMap<Date, &PlannedSession> = rows.iter().map(|r| (r.planned_start_at.local(tz).date(), r)).collect();
    let mut out = vec![];
    for offset in 0..7 {
        let day = today.checked_add(offset.days())?;
        let session = by_day.get(&day).map(|row| {
            let forecast = row.workout_plan_json.get("crowd_forecast").filter(|f| f.is_object()).cloned().unwrap_or(json!({}));
            json!({"start": hhmm(row.planned_start_at, tz), "end": hhmm(row.planned_end_at, tz), "status": row.status,
                "template": row.workout_plan_json.pointer("/template/name").cloned().unwrap_or(json!("Workout")),
                "crowd": forecast.get("feel"), "count": forecast.get("count")})
        });
        out.push(json!({"name": date_fmt(day, "%a"), "date": day.day(), "today": offset == 0, "session": session}));
    }
    Ok(out)
}

fn next_session(db: &Connection, tz: &TimeZone, now: Ts) -> Result<Value> {
    let Some(row) = PlannedSession::first(
        db,
        "WHERE status IN ('TENTATIVE', 'COMMITTED') AND planned_end_at > ?1 AND workout_template_id IS NOT NULL ORDER BY planned_start_at",
        [now],
    )?
    else {
        return Ok(Value::Null);
    };
    let template = get_template(db, row.workout_template_id.as_deref().unwrap_or_default())?;
    let sets = row.workout_plan_json.get("sets").cloned().unwrap_or(json!({}));
    let forecast = row.workout_plan_json.get("crowd_forecast").filter(|f| f.is_object()).cloned().unwrap_or(json!({}));
    let mut exercises = vec![];
    for spec in &template.exercises {
        let last = preview::last_set(db, &spec.id)?;
        let reps = if spec.rep_min == spec.rep_max { spec.rep_min.to_string() } else { format!("{}–{}", spec.rep_min, spec.rep_max) };
        exercises.push(json!({"name": spec.name, "svg_url": svg(&spec.name, spec.guide_id.as_deref()), "sets": sets.get(&spec.id).cloned().unwrap_or(json!(spec.working_sets)),
            "reps": reps, "last": last.map(|l| format!("{} × {} kg", l.reps, g(l.weight)))}));
    }
    Ok(json!({"template": template.name, "when": fmt(row.planned_start_at, tz, "%a %H:%M"), "leave": hhmm(row.leave_home_at, tz),
        "crowd": forecast.get("feel"), "count": forecast.get("count"), "exercises": exercises}))
}

fn active_workout(db: &Connection, tz: &TimeZone, now: Ts) -> Result<Value> {
    let Some(w) = WorkoutSession::first(db, "WHERE status != 'PLAN_UPDATED' AND started_at IS NOT NULL ORDER BY started_at DESC", [])? else {
        return Ok(Value::Null);
    };
    let data = workout::current(db, &w.id, now)?;
    let mut exercises = vec![];
    for row in workout::exercises(db, &w.id)?.iter().filter(|r| r.status != crate::db::models::ExerciseStatus::Substituted) {
        let name = row.config_json["name"].as_str().unwrap_or_default();
        exercises.push(json!({"id": row.id, "name": name, "svg_url": svg(name, row.config_json["guide_id"].as_str()), "sets": row.planned_working_sets,
            "done": workout::logs(db, &row.id, Some(SetType::Working))?.len(), "status": row.status}));
    }
    let active = &data["active_exercise"];
    let latest = SetLog::first(
        db,
        "WHERE set_type = 'WORKING' AND workout_exercise_id IN (SELECT id FROM workout_exercise WHERE workout_session_id = ?1) ORDER BY logged_at DESC",
        [&w.id],
    )?;
    let eta = data["eta"].as_str().and_then(|e| Ts::parse(e, None).ok()).map(|e| hhmm(e, tz));
    let active = if active.is_object() {
        json!({"id": active["id"], "name": active["name"], "set_type": active["set_type"], "set_number": active["set_number"], "sets": active["working_sets"],
            "weight": active["target_weight"], "rep_min": active["rep_min"], "rep_max": active["rep_max"]})
    } else {
        Value::Null
    };
    Ok(json!({"template": w.template_snapshot.get("name").cloned().unwrap_or(json!("Workout")), "eta": eta, "active": active,
        "rest_until": data["rest_job"].get("due_at"), "last_set": latest.map(|l| format!("{} × {} kg", l.reps, g(l.weight))), "exercises": exercises}))
}

/// Wall-clock minutes since 1970-01-01 in `tz`; lets the browser slice days/weeks without timezone math.
fn local_minutes(at: Ts, tz: &TimeZone) -> i64 {
    let local: DateTime = at.local(tz).datetime();
    let epoch = DateTime::constant(1970, 1, 1, 0, 0, 0, 0);
    local.duration_since(epoch).as_secs().div_euclid(60)
}

fn crowd_state(db: &Connection, tz: &TimeZone, now: Ts, polling: bool) -> Result<Value> {
    let rows = CrowdObservation::select(db, "WHERE source = 'GYM_API' AND observed_at <= ?1 AND observed_at >= ?2 ORDER BY observed_at", [now, now - SignedDuration::from_hours(24 * 400)])?;
    let latest = rows.last().map(|r| json!({"count": r.raw_value, "at": r.observed_at, "time": hhmm(r.observed_at, tz)}));
    Ok(json!({"now": latest, "local_now": local_minutes(now, tz), "series": rows.iter().map(|r| json!([local_minutes(r.observed_at, tz), r.raw_value])).collect::<Vec<_>>(),
        "polling": polling, "health": crowd::polling_health(db, now)?}))
}

fn label(rating: &str) -> Value {
    crowd::LABELS.iter().find(|(k, _)| *k == rating).map(|(_, v)| json!(v)).unwrap_or(Value::Null)
}

fn present(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.is_empty())
}

/// Finished workouts, newest first, with every working set. PRs: a heavier top set than any earlier
/// session of that exercise.
fn history(db: &Connection, tz: &TimeZone) -> Result<Vec<Value>> {
    let workouts = WorkoutSession::select(db, "WHERE status = 'PLAN_UPDATED' AND completed_at IS NOT NULL ORDER BY completed_at", [])?;
    let ratings: HashMap<String, String> = CrowdFeedback::select(db, "", [])?.into_iter().map(|f| (f.workout_session_id, f.rating)).collect();
    // Early workouts predate guide_ids; current templates know each exercise's illustration/muscle.
    let mut guides: HashMap<String, Option<String>> = HashMap::new();
    for t in WorkoutTemplate::select(db, "", [])? {
        let list = |k: &str| t.definition_json.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        for spec in list("exercises").into_iter().chain(list("alternatives")) {
            if let Some(id) = spec["id"].as_str() {
                guides.insert(id.into(), spec["guide_id"].as_str().map(String::from));
            }
        }
    }
    let mut best: HashMap<String, f64> = HashMap::new();
    let mut result = vec![];
    for w in &workouts {
        let (mut exercises, mut volume, mut sets, mut prs, mut last_log, mut abandoned) = (vec![], 0.0, 0, vec![], None::<Ts>, false);
        for row in workout::exercises(db, &w.id)? {
            abandoned |= row.deferred_reason.as_deref() == Some("abandoned");
            let done = workout::logs(db, &row.id, Some(SetType::Working))?;
            let Some(last) = done.last() else { continue };
            last_log = Some(last_log.map_or(last.logged_at, |l| l.max(last.logged_at)));
            let top = done.iter().map(|s| s.weight).fold(f64::MIN, f64::max);
            let name = row.config_json["name"].as_str().unwrap_or_default().to_string();
            if best.get(&row.exercise_id).is_some_and(|b| top > *b) {
                prs.push(name.clone());
            }
            let entry = best.entry(row.exercise_id.clone()).or_insert(0.0);
            *entry = entry.max(top);
            volume += done.iter().map(|s| s.weight * s.reps as f64).sum::<f64>();
            sets += done.len();
            let guide = present(row.config_json["guide_id"].as_str())
                .or_else(|| present(guides.get(&row.exercise_id).and_then(|g| g.as_deref())))
                .unwrap_or(&row.exercise_id);
            let muscle = illustrations::primary_muscle(Some(guide)).or_else(|| illustrations::search(&name, None, None, 1).first().map(|e| e.primary_muscle.clone()));
            exercises.push(json!({"id": row.exercise_id, "name": name, "muscle": muscle, "sets": done.iter().map(|s| json!({"weight": s.weight, "reps": s.reps})).collect::<Vec<_>>()}));
        }
        let started_at = w.started_at.or(w.completed_at).expect("completed workout");
        let mut seconds = w.actual_duration_seconds.unwrap_or(0);
        if abandoned && let Some(last) = last_log {
            // Closed automatically hours later: count only the time actually training.
            seconds = (last - started_at).as_secs();
        }
        result.push(json!({"id": w.id, "date": started_at.local(tz).date().to_string(), "label": fmt(started_at, tz, "%a %-d %b"), "time": fmt(started_at, tz, "%H:%M"),
            "template": w.template_snapshot.get("name").cloned().unwrap_or(json!("Workout")), "minutes": seconds.div_euclid(60),
            "sets": sets, "volume": round(volume, 0) as i64, "prs": prs, "crowd": ratings.get(&w.id).map(|r| label(r)), "exercises": exercises}));
    }
    result.reverse();
    Ok(result)
}

/// Estimated 1-rep max (Epley), to the nearest 0.5 kg.
pub fn one_rep_max(weight: f64, reps: i64) -> f64 {
    if reps > 1 { round(weight * (1.0 + reps as f64 / 30.0) * 2.0, 0) / 2.0 } else { weight }
}

fn workout_date(w: &Value) -> Date {
    w["date"].as_str().and_then(|d| d.parse().ok()).expect("history date")
}

/// Numbers that answer: am I consistent, am I progressing, what am I training, when is it quiet.
fn insights(db: &Connection, tz: &TimeZone, now: Ts, profile: &Profile, done: &[Value]) -> Result<Value> {
    let today = now.local(tz).date();
    let monday = today.checked_sub((today.weekday().to_monday_zero_offset() as i64).days())?;
    let mut weeks = vec![];
    for back in (0..=7).rev() {
        let start = monday.checked_sub((7 * back).days())?;
        let end = start.checked_add(7.days())?;
        let count = done.iter().filter(|w| (start..end).contains(&workout_date(w))).count();
        weeks.push(json!({"label": date_fmt(start, "%-d %b"), "start": start.to_string(), "count": count}));
    }
    let volume_between = |start: Date, end: Date| -> i64 { done.iter().filter(|w| (start..end).contains(&workout_date(w))).map(|w| w["volume"].as_i64().unwrap_or(0)).sum() };
    let mut order: Vec<String> = vec![];
    let mut progress: HashMap<String, Value> = HashMap::new();
    for w in done.iter().rev() {
        for exercise in w["exercises"].as_array().into_iter().flatten() {
            let sets = exercise["sets"].as_array().cloned().unwrap_or_default();
            // Best set by estimated 1-rep max, so 12 × 40 and 8 × 50 compare on one line (first max wins, like Python).
            let key = |s: &Value| (one_rep_max(s["weight"].as_f64().unwrap_or(0.0), s["reps"].as_i64().unwrap_or(0)), s["reps"].as_i64().unwrap_or(0));
            let Some(top) = sets.iter().fold(None::<&Value>, |best, s| match best {
                Some(b) if key(b).partial_cmp(&key(s)) != Some(std::cmp::Ordering::Less) => Some(b),
                _ => Some(s),
            }) else {
                continue;
            };
            let id = exercise["id"].as_str().unwrap_or_default().to_string();
            let entry = progress.entry(id.clone()).or_insert_with(|| {
                order.push(id.clone());
                json!({"id": id, "name": exercise["name"], "points": []})
            });
            let (weight, reps) = (top["weight"].as_f64().unwrap_or(0.0), top["reps"].as_i64().unwrap_or(0));
            entry["points"].as_array_mut().expect("points").push(json!({"date": w["date"], "label": w["label"], "weight": top["weight"], "reps": top["reps"], "e1rm": one_rep_max(weight, reps)}));
        }
    }
    let mut muscle_order: Vec<String> = vec![];
    let mut muscles: HashMap<String, (f64, i64)> = HashMap::new();
    let cutoff = today.checked_sub(28.days())?;
    for w in done.iter().filter(|w| workout_date(w) >= cutoff) {
        for exercise in w["exercises"].as_array().into_iter().flatten() {
            let key = exercise["muscle"].as_str().filter(|m| !m.is_empty()).unwrap_or("Other").to_string();
            let sets = exercise["sets"].as_array().cloned().unwrap_or_default();
            let entry = muscles.entry(key.clone()).or_insert_with(|| {
                muscle_order.push(key);
                (0.0, 0)
            });
            entry.0 += sets.iter().map(|s| s["weight"].as_f64().unwrap_or(0.0) * s["reps"].as_f64().unwrap_or(0.0)).sum::<f64>();
            entry.1 += sets.len() as i64;
        }
    }
    let mut muscle_list: Vec<(String, f64, i64)> = muscle_order.into_iter().map(|m| (m.clone(), muscles[&m].0, muscles[&m].1)).collect();
    muscle_list.sort_by(|a, b| b.1.total_cmp(&a.1));
    // Gym crowd by weekday × hour: mean of daily means, so busy days don't count twice.
    let mut cells: BTreeMap<(i8, i8), BTreeMap<Date, Vec<f64>>> = BTreeMap::new();
    for row in CrowdObservation::select(db, "WHERE source = 'GYM_API' AND observed_at >= ?1", [now - SignedDuration::from_hours(24 * 90)])? {
        let local = row.observed_at.local(tz);
        cells.entry((local.weekday().to_monday_zero_offset(), local.hour())).or_default().entry(local.date()).or_default().push(row.raw_value);
    }
    let heatmap: Vec<Value> = cells
        .iter()
        .map(|((d, h), dates)| json!({"weekday": d, "hour": h, "count": round(mean(&dates.values().map(|v| mean(v)).collect::<Vec<_>>()), 1), "days": dates.len()}))
        .collect();
    let recent_cut = today.checked_sub(30.days())?;
    let recent: Vec<&Value> = done.iter().filter(|w| workout_date(w) >= recent_cut).collect();
    Ok(json!({"target": profile.weekly_target_sessions, "weeks": weeks,
        "kpis": {"workouts_30d": recent.len(), "this_week": weeks.last().map(|w| w["count"].clone()),
            // Rolling windows: a Monday morning shouldn't read as "-100% vs last week".
            "volume_7d": volume_between(today.checked_sub(6.days())?, today.checked_add(1.day())?),
            "volume_prev_7d": volume_between(today.checked_sub(13.days())?, today.checked_sub(6.days())?),
            "prs_30d": recent.iter().map(|w| w["prs"].as_array().map_or(0, Vec::len)).sum::<usize>()},
        "progress": order.iter().map(|id| progress[id].clone()).collect::<Vec<_>>(),
        "muscles": muscle_list.into_iter().map(|(m, v, s)| json!({"muscle": m, "volume": round(v, 0) as i64, "sets": s})).collect::<Vec<_>>(),
        "heatmap": heatmap}))
}

/// Every weigh-in, oldest first, with the average of the 7 days up to it (the trend line).
fn body_weight(db: &Connection, tz: &TimeZone, now: Ts) -> Result<Vec<Value>> {
    let rows = BodyWeight::select(db, "WHERE measured_at <= ?1 ORDER BY measured_at", [now])?;
    Ok(rows
        .iter()
        .map(|row| {
            let week: Vec<f64> = rows.iter().filter(|r| row.measured_at - SignedDuration::from_hours(24 * 7) < r.measured_at && r.measured_at <= row.measured_at).map(|r| r.kg).collect();
            json!({"date": row.measured_at.local(tz).date().to_string(), "label": fmt(row.measured_at, tz, "%a %-d %b %Y"), "kg": row.kg, "avg": round(mean(&week), 1)})
        })
        .collect())
}

/// What the agent remembers: the owner's notes and its own lessons, each oldest first.
fn memory(db: &Connection) -> Result<Value> {
    let notes = crate::tools::meta::notes(db)?;
    let list = |kind: NoteKind| -> Vec<Value> {
        notes.iter().filter(|n| n.kind == kind).map(|n| json!({"id": n.id, "text": n.text, "updated_at": iso(&n.updated_at.local(&TimeZone::UTC))})).collect()
    };
    Ok(json!({"owner": list(NoteKind::Owner), "lesson": list(NoteKind::Lesson)}))
}

/// The whole dashboard state.
pub fn snapshot(db: &Connection, now: Ts) -> Result<Value> {
    let profile = if UserProfile::get(db, 1)?.is_some() { get_profile(db)? } else { Profile::default() };
    let tz = profile.tz();
    let today = now.local(&tz).date();
    let settings = RuntimeSettings::get(db, 1)?;
    let marks = ACTIVITY.iter().map(|(t, _)| format!("'{t}'")).collect::<Vec<_>>().join(", ");
    let events = AgentEvent::select(db, &format!("WHERE type IN ({marks}) ORDER BY created_at DESC LIMIT 12"), [])?;
    let done = history(db, &tz)?;
    let mut insights = insights(db, &tz, now, &profile, &done)?;
    insights["weight"] = json!(body_weight(db, &tz, now)?);
    let polling = settings.is_some_and(|s| s.enabled && s.crowd_polling_enabled);
    Ok(json!({
        "demo": false, "live_database_accessed": true, "captured_at": iso(&now.local(&TimeZone::UTC)), "timezone": profile.timezone,
        "range": format!("{} – {}", date_fmt(today, "%-d %b"), date_fmt(today.checked_add(6.days())?, "%-d %b")),
        "days": days(db, &tz, today)?, "next": next_session(db, &tz, now)?, "workout": active_workout(db, &tz, now)?,
        "crowd": crowd_state(db, &tz, now, polling)?, "history": done, "insights": insights, "memory": memory(db)?,
        "activity": events.iter().map(|e| json!({"time": hhmm(e.created_at, &tz), "day": fmt(e.created_at, &tz, "%a"),
            "text": ACTIVITY.iter().find(|(t, _)| *t == e.r#type).map(|(_, text)| *text)})).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_number_formats() {
        assert_eq!((g(40.0), g(22.5)), ("40".to_string(), "22.5".to_string()));
        assert_eq!(one_rep_max(100.0, 5), 116.5);
        assert_eq!(one_rep_max(40.0, 12), 56.0);
        assert_eq!(round(2.5, 0), 2.0);
    }

    #[test]
    fn one_rep_max_makes_different_rep_counts_comparable() {
        assert_eq!((one_rep_max(50.0, 8), one_rep_max(100.0, 1), one_rep_max(0.0, 15)), (63.5, 100.0, 0.0));
    }
}
