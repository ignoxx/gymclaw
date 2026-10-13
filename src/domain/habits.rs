//! Time-of-day habits learned from what the owner actually does (port of habits.py). Owner: lead.
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
    let mut points: Vec<Point> = WorkoutSession::select(db, "WHERE started_by_button AND arrived_at >= ?1 ORDER BY rowid", [since])?
        .into_iter()
        .filter_map(|w| w.arrived_at.map(|at| point(at, 1.0)))
        .collect();
    // One owner decision per session: the slot it was planned at first is disliked, the last explicit
    // pick is liked. A chain of moves (10:30 → 14:30 → 11:45) only counts its ends.
    let mut order: Vec<String> = Vec::new();
    let mut moves: HashMap<String, (Value, Option<Value>)> = HashMap::new();
    for event in AgentEvent::select(db, "WHERE type = 'planning.session_moved' AND created_at >= ?1 ORDER BY created_at", [since])? {
        let data = &event.payload_json["result"]["data"];
        let Some(session_id) = data.get("session_id").and_then(Value::as_str) else { continue };
        let explicit = event.payload_json["request"]["intent"].get("to").is_some_and(|to| !to.is_null() && to != "");
        let entry = moves.entry(session_id.to_string()).or_insert_with(|| {
            order.push(session_id.to_string());
            (data.clone(), None)
        });
        if explicit {
            entry.1 = Some(data.clone());
        }
    }
    for id in order {
        let (first, last) = &moves[&id];
        if let Some(from) = first["from"].as_str().and_then(|s| Ts::parse(s, None).ok()) {
            points.push(point(from, -1.0));
        }
        if let Some(to) = last.as_ref().and_then(|l| l["to"].as_str()).and_then(|s| Ts::parse(s, None).ok()) {
            points.push(point(to, 1.0));
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
    let net = points.iter().map(|(m, w)| w * bump(*m)).sum::<f64>() / (points.iter().map(|(_, w)| w.abs()).sum::<f64>() + PRIOR_WEIGHT);
    0.5 + 0.5 * net
}
