//! Time-of-day habits learned from what the owner actually does.
//!
//! Evidence: when the owner arrives at the gym (workouts started with the reminder's ▶️ button; chat
//! starts may be logged late, so they don't count), where the owner moves sessions to, and the planned
//! times they move sessions away from. Each point is a Gaussian bump on the clock, decayed by age. The
//! result is a 0–1 affinity per time of day (0.5 = no opinion), fed to the planner as
//! `SlotSignal::preferred_time`. Its weight sits below crowd, so a habitual but busy time still loses to
//! a quiet one; habits break ties and steer away from times the owner keeps moving off.
use std::collections::HashMap;

use jiff::SignedDuration;
use jiff::tz::TimeZone;
use serde_json::Value;

use crate::db::Connection;
use crate::db::models::{AgentEvent, WorkoutSession};
use crate::error::Result;
use crate::time::Ts;

const HISTORY_DAYS: i64 = 120;
const HALF_LIFE_DAYS: f64 = 42.0;
const BANDWIDTH_MINUTES: f64 = 45.0;
/// Pseudo-evidence pulling sparse habits toward "no opinion": one data point can't dominate.
const PRIOR_WEIGHT: f64 = 3.0;

/// (minute of day, weight); negative weight = moved away from.
pub type Point = (f64, f64);

pub fn evidence(db: &Connection, now: Ts, zone: &TimeZone) -> Result<Vec<Point>> {
    let since = now - SignedDuration::from_hours(24 * HISTORY_DAYS);
    let point = |at: Ts, sign: f64| -> Point {
        let local = at.local(zone);
        let age_days = (now - at).as_secs_f64() / 86400.0;
        ((local.hour() as i64 * 60 + local.minute() as i64) as f64, sign * 0.5f64.powf(age_days / HALF_LIFE_DAYS))
    };
    let mut points: Vec<Point> =
        WorkoutSession::select(db, "WHERE started_by_button AND arrived_at >= ?1 ORDER BY rowid", [since])?
            .into_iter()
            .filter_map(|w| w.arrived_at.map(|at| point(at, 1.0)))
            .collect();
    // One owner decision per session: the slot it was planned at first is disliked, the last pick is
    // liked. A chain of moves (10:30 → 14:30 → 11:45) only counts its ends; edits that keep the time don't count.
    let mut order: Vec<String> = Vec::new();
    let mut moves: HashMap<String, (Value, Value)> = HashMap::new();
    for event in AgentEvent::select(
        db,
        "WHERE type = 'planning.session_edited' AND created_at >= ?1 ORDER BY created_at",
        [since],
    )? {
        if event.payload_json["request"]["intent"]["patch"]["start"].is_null() {
            continue;
        }
        let data = &event.payload_json["result"]["data"];
        let Some(session_id) = data["session"]["id"].as_str() else { continue };
        let entry = moves.entry(session_id.to_string()).or_insert_with(|| {
            order.push(session_id.to_string());
            (data["from"].clone(), Value::Null)
        });
        entry.1 = data["session"]["start"].clone();
    }
    for id in order {
        let (from, to) = &moves[&id];
        for (at, sign) in [(from, -1.0), (to, 1.0)] {
            if let Some(at) = at.as_str().and_then(|s| Ts::parse(s, None).ok()) {
                points.push(point(at, sign));
            }
        }
    }
    Ok(points)
}

/// 0–1 liking of a time of day; 0.5 without evidence.
pub fn affinity(points: &[Point], minute: f64) -> f64 {
    let bump = |other: f64| {
        let distance = (minute - other).abs().min(1440.0 - (minute - other).abs());
        (-0.5 * (distance / BANDWIDTH_MINUTES).powi(2)).exp()
    };
    let net = points.iter().map(|(m, w)| w * bump(*m)).sum::<f64>()
        / (points.iter().map(|(_, w)| w.abs()).sum::<f64>() + PRIOR_WEIGHT);
    0.5 + 0.5 * net
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;

    use super::*;
    use crate::domain::replanning::{SessionPatch, edit_session};
    use crate::domain::scheduling::schedule_week;
    use crate::domain::workout::tests::db;

    /// A chain of moves counts once: its first slot is disliked, its last pick liked.
    #[test]
    fn a_chain_of_moves_counts_its_ends() {
        let db = db();
        let now = Ts::parse("2026-10-11T12:00:00+00:00", None).unwrap();
        let planned = schedule_week(&db, jiff::civil::date(2026, 10, 12), now, None, None, &[]).unwrap();
        let id = planned["sessions"][0]["id"].as_str().unwrap();
        let first = crate::db::models::PlannedSession::get(&db, id).unwrap().unwrap().planned_start_at;
        for (hours, request) in [(2, "move-1"), (5, "move-2")] {
            edit_session(
                &db,
                Some(id),
                SessionPatch { start: Some(first + SignedDuration::from_hours(hours)), ..Default::default() },
                now,
                request,
            )
            .unwrap();
        }
        let tz = crate::time::zone("Europe/Berlin").unwrap();
        let minute = |at: Ts| (at.local(&tz).hour() as i64 * 60 + at.local(&tz).minute() as i64) as f64;
        let points: Vec<(f64, f64)> =
            evidence(&db, now, &tz).unwrap().into_iter().map(|(m, w)| (m, w.signum())).collect();
        assert_eq!(points, vec![(minute(first), -1.0), (minute(first + SignedDuration::from_hours(5)), 1.0)]);
    }
}
