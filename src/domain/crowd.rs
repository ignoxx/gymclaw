//! Gym crowd: deterministic collection, conservative label calibration and slot signals (port of
//! crowd.py). No gym capacity assumption or count→occupancy conversion: the owner's ratings define the
//! personal score; confidence is heuristic evidence strength, not forecast accuracy. Owner: lead.
use std::collections::HashMap;

use jiff::SignedDuration;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde_json::{Map, Value, json};

use crate::db::models::{CrowdFeedback, CrowdObservation, CrowdSourceState, LearnedPreference};
use crate::db::{Connection, Json, new_id};
use crate::domain::core::{emit, mutate};
use crate::domain::events::outage_open;
use crate::domain::planning::SlotSignal;
use crate::domain::profile::get_profile;
use crate::error::Result;
use crate::time::Ts;

/// Felt-crowd ratings the owner can pick, in order. FINE is stored as before and reads "A few".
pub const LABELS: [(&str, &str); 4] = [("EMPTY", "Empty"), ("FINE", "A few"), ("BUSY", "Busy"), ("PACKED", "Packed")];
const RATINGS: [(&str, f64); 4] = [("EMPTY", 0.0), ("FINE", 1.0 / 3.0), ("BUSY", 2.0 / 3.0), ("PACKED", 1.0)];
const MAX_PAIR_AGE: SignedDuration = SignedDuration::from_mins(30);
const HISTORY: SignedDuration = SignedDuration::from_hours(24 * 90);
const STALE_AFTER: SignedDuration = SignedDuration::from_mins(45);
const SOURCE: &str = "GYM_API";

fn rating_score(rating: &str) -> Option<f64> {
    RATINGS.iter().find(|(r, _)| *r == rating).map(|(_, s)| *s)
}

/// Owner-facing word for a 0–1 crowd score.
pub fn feel(score: Option<f64>) -> Option<&'static str> {
    let score = score?;
    let limits = [1.0 / 6.0, 0.5, 5.0 / 6.0, f64::INFINITY];
    LABELS.iter().zip(limits).find(|(_, limit)| score < *limit).map(|((_, label), _)| *label)
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0 }
}

/// Mean per local date, then the mean of those (repeated polls are not independent days). Returns (mean, days).
fn daily_mean<'a>(rows: impl Iterator<Item = &'a CrowdObservation>, zone: &TimeZone) -> Option<(f64, usize)> {
    let mut days: Vec<(Date, Vec<f64>)> = Vec::new();
    for row in rows {
        let day = row.observed_at.local(zone).date();
        match days.iter_mut().find(|(d, _)| *d == day) {
            Some((_, values)) => values.push(row.raw_value),
            None => days.push((day, vec![row.raw_value])),
        }
    }
    if days.is_empty() {
        return None;
    }
    let means: Vec<f64> = days.iter().map(|(_, v)| mean(v)).collect();
    Some((mean(&means), days.len()))
}

/// Score for a reported count from the owner's labels → (score, confidence, method).
pub fn calibrated_count(count: f64, pairs: &[(f64, f64)]) -> (Option<f64>, f64, &'static str) {
    if pairs.is_empty() {
        return (None, 0.0, "uncalibrated_reported_count");
    }
    if pairs.len() < 10 {
        let mut neighbors = pairs.to_vec();
        neighbors.sort_by(|a, b| (a.0 - count).abs().total_cmp(&(b.0 - count).abs()));
        neighbors.truncate(3);
        let weights: Vec<f64> = neighbors.iter().map(|(x, _)| 1.0 / (1.0 + (x - count).abs())).collect();
        let value = neighbors.iter().zip(&weights).map(|((_, y), w)| y * w).sum::<f64>() / weights.iter().sum::<f64>();
        let distance = pairs.iter().map(|(x, _)| (x - count).abs()).fold(f64::INFINITY, f64::min);
        return (Some(value), (pairs.len() as f64 / 20.0).min(0.35) / (1.0 + distance / 20.0), "label_neighbor_heuristic");
    }
    // Pool-adjacent-violators: small monotonic, label-calibrated regression.
    let mut grouped: Vec<(f64, Vec<f64>)> = Vec::new();
    for &(x, y) in pairs {
        match grouped.iter_mut().find(|(gx, _)| *gx == x) {
            Some((_, ys)) => ys.push(y),
            None => grouped.push((x, vec![y])),
        }
    }
    grouped.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut blocks: Vec<(f64, f64, f64, f64)> = Vec::new();
    for (x, values) in grouped {
        blocks.push((x, x, values.iter().sum(), values.len() as f64));
        while blocks.len() >= 2 && {
            let (l, r) = (blocks[blocks.len() - 2], blocks[blocks.len() - 1]);
            l.2 / l.3 > r.2 / r.3
        } {
            let right = blocks.pop().expect("two blocks");
            let left = blocks.pop().expect("two blocks");
            blocks.push((left.0, right.1, left.2 + right.2, left.3 + right.3));
        }
    }
    let points: Vec<(f64, f64)> = blocks.iter().map(|(lo, hi, total, size)| ((lo + hi) / 2.0, total / size)).collect();
    let mut score = if count <= points[0].0 { points[0].1 } else { points[points.len() - 1].1 };
    for w in points.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if x0 <= count && count <= x1 {
            score = y0 + (y1 - y0) * (count - x0) / (x1 - x0);
            break;
        }
    }
    let low = pairs.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let high = pairs.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
    let penalty = if count < low || count > high { 0.5 } else { 1.0 };
    let n = pairs.len() as f64;
    (Some(score), (n / (n + 10.0)).min(0.8) * penalty, "personalized_isotonic")
}

/// Evidence loaded once per planning pass; no per-candidate DB queries.
pub struct CrowdModel {
    pub now: Ts,
    pub zone: TimeZone,
    pub observations: Vec<CrowdObservation>,
    pub labels: Vec<CrowdFeedback>,
    states: HashMap<String, CrowdSourceState>,
    count_pairs: Vec<(f64, f64)>,
}

pub struct Prediction {
    pub score: Option<f64>,
    pub confidence: f64,
    pub demo: bool,
    pub json: Value,
}

impl CrowdModel {
    pub fn new(db: &Connection, now: Ts) -> Result<CrowdModel> {
        let zone = get_profile(db)?.tz();
        let since = now - HISTORY;
        let observations = CrowdObservation::select(db, "WHERE observed_at <= ?1 AND observed_at >= ?2 ORDER BY observed_at, id", (now, since))?;
        let labels: Vec<CrowdFeedback> = CrowdFeedback::select(db, "WHERE observed_at <= ?1 AND observed_at >= ?2 ORDER BY observed_at, id", (now, since))?
            .into_iter()
            .filter(|l| l.features_json.get("recorded_at").and_then(Value::as_str).and_then(|s| Ts::parse(s, None).ok()).unwrap_or(l.observed_at) <= now)
            .collect();
        let states = CrowdSourceState::select(db, "", [])?.into_iter().map(|s| (s.source.clone(), s)).collect();
        let count_pairs = labels.iter().filter_map(|f| f.features_json.get(SOURCE).and_then(|g| g["raw_value"].as_f64()).map(|raw| (raw, f.normalized_score))).collect();
        Ok(CrowdModel { now, zone, observations, labels, states, count_pairs })
    }

    pub fn has_data(&self) -> bool {
        !self.observations.is_empty() || !self.labels.is_empty()
    }

    fn same_slot(&self, left: Ts, right: Ts) -> bool {
        let (l, r) = (left.local(&self.zone), right.local(&self.zone));
        (l.weekday(), l.hour()) == (r.weekday(), r.hour())
    }

    /// Typical reported check-ins: same weekday and hour, else the same hour on other days.
    pub fn expected_count(&self, at: Ts) -> Option<Value> {
        let rows: Vec<&CrowdObservation> = self.observations.iter().filter(|r| r.source == SOURCE).collect();
        let hour = at.local(&self.zone).hour();
        let same: Vec<&CrowdObservation> = rows.iter().copied().filter(|r| self.same_slot(r.observed_at, at)).collect();
        let (selected, basis) = if same.is_empty() {
            (rows.into_iter().filter(|r| r.observed_at.local(&self.zone).hour() == hour).collect::<Vec<_>>(), "hour")
        } else {
            (same, "weekday_hour")
        };
        let (value, days) = daily_mean(selected.into_iter(), &self.zone)?;
        Some(json!({"count": value.round_ties_even() as i64, "basis": basis, "days": days}))
    }

    pub fn forecast(&self, at: Ts) -> Option<Value> {
        let expected = self.expected_count(at);
        let score = self.predict(at).score;
        if expected.is_none() && score.is_none() {
            return None;
        }
        let mut out = expected.unwrap_or_else(|| json!({}));
        out["feel"] = json!(feel(score));
        out["score"] = json!(score);
        Some(out)
    }

    fn signal_feature(&self, source: &str, at: Ts) -> Option<Value> {
        let rows: Vec<&CrowdObservation> = self.observations.iter().filter(|r| r.source == source && r.observed_at <= at).collect();
        let latest = rows.last().copied();
        let recent = latest.is_some_and(|l| self.now <= at && at <= self.now + MAX_PAIR_AGE && at - l.observed_at <= MAX_PAIR_AGE);
        let (selected, kind): (Vec<&CrowdObservation>, &str) = if recent {
            (vec![latest.expect("recent")], "recent_reported_signal")
        } else {
            (rows.iter().copied().filter(|r| self.same_slot(r.observed_at, at)).collect(), "locally_collected_slot_history")
        };
        let (raw, days) = daily_mean(selected.iter().copied(), &self.zone)?;
        let freshness_penalty = if selected.iter().any(|r| r.freshness_seconds.is_none()) { 0.5 } else { 1.0 };
        let mut history_confidence = (days as f64 / (days as f64 + 5.0)).min(0.6) * freshness_penalty;
        if recent {
            history_confidence = 0.5 * freshness_penalty;
        }
        let state = self.states.get(source);
        let reliability = state.map_or(0.5, |s| s.reliability);
        if state.is_some_and(|s| s.consecutive_failures > 0) {
            history_confidence *= 0.5;
        }
        let (score, calibration_confidence, method) = calibrated_count(raw, &self.count_pairs);
        Some(json!({"raw_value": raw, "score": score, "confidence": history_confidence.min(calibration_confidence), "reliability": reliability,
            "method": method, "evidence_days": days, "signal_kind": kind, "observation_id": selected.last().map(|r| &r.id),
            "demo": selected.iter().any(|r| r.metadata_json.get("demo").and_then(Value::as_bool).unwrap_or(false))}))
    }

    pub fn predict(&self, at: Ts) -> Prediction {
        let mut features = Map::new();
        if let Some(feature) = self.signal_feature(SOURCE, at) {
            features.insert(SOURCE.into(), feature);
        }
        let slot_labels: Vec<&CrowdFeedback> = self.labels.iter().filter(|f| f.observed_at <= at && self.same_slot(f.observed_at, at)).collect();
        let mut components: Vec<(f64, f64)> = features
            .values()
            .filter_map(|f| {
                let (score, confidence) = (f["score"].as_f64()?, f["confidence"].as_f64()?);
                (confidence > 0.0).then(|| (score, confidence * f["reliability"].as_f64().unwrap_or(0.0)))
            })
            .collect();
        if !slot_labels.is_empty() {
            let count = slot_labels.len() as f64;
            components.push((mean(&slot_labels.iter().map(|f| f.normalized_score).collect::<Vec<_>>()), (count / (count + 5.0)).min(0.8)));
        }
        let weight: f64 = components.iter().map(|(_, w)| w).sum();
        let score = (weight != 0.0).then(|| components.iter().map(|(v, w)| v * w).sum::<f64>() / weight);
        let confidence = if weight != 0.0 { (weight / (weight + 0.5)).min(0.85) } else { 0.0 };
        let demo = features.values().any(|f| f["demo"].as_bool().unwrap_or(false))
            || slot_labels.iter().any(|f| f.features_json.get("demo").and_then(Value::as_bool).unwrap_or(false));
        let instruction = if score.is_none() {
            "Crowd unknown; reported count needs arrival labels."
        } else {
            "Crowd estimate uses available signals and user labels; confidence is heuristic."
        };
        let json = json!({"at": at, "score": score, "confidence": confidence, "score_kind": "personal_perceived_crowd_proxy",
            "label_evidence": self.labels.len(), "slot_label_evidence": slot_labels.len(), "features": features, "demo": demo,
            "forecast_accuracy_established": false, "instruction": instruction});
        Prediction { score, confidence, demo, json }
    }
}

/// Crowd source health per source, for `crowd status`.
pub fn source_health(db: &Connection, now: Ts) -> Result<Value> {
    let mut sources = Vec::new();
    for state in CrowdSourceState::select(db, "ORDER BY source", [])? {
        let latest = CrowdObservation::first(db, "WHERE source = ?1 ORDER BY observed_at DESC, id DESC", [&state.source])?;
        sources.push(json!({"source": state.source, "provider_id": state.provider_id,
            "last_success_at": state.last_success_at, "last_failure_at": state.last_failure_at,
            "retrieval_age_seconds": state.last_success_at.map(|t| (now - t).as_secs().max(0)),
            "provider_freshness_known": latest.as_ref().is_some_and(|l| l.freshness_seconds.is_some()),
            "provider_freshness_seconds_at_retrieval": latest.as_ref().and_then(|l| l.freshness_seconds),
            "consecutive_failures": state.consecutive_failures, "last_error_code": state.last_error_code,
            "last_raw_value": state.last_raw_value, "last_change_at": state.last_change_at,
            "reliability": state.reliability, "reliability_evidence_count": state.evidence_count}));
    }
    Ok(json!({"sources": sources}))
}

/// How busy it felt after a workout. Mutation result envelope.
pub fn record_feedback(db: &Connection, workout_id: &str, rating: &str, now: Ts, request_id: &str) -> Result<Value> {
    let rating = rating.to_uppercase();
    let Some(normalized) = rating_score(&rating) else {
        crate::bail!("INVALID_ARGUMENTS", "Use EMPTY/FINE/BUSY/PACKED, wait count 0–100 and notes ≤1000 chars");
    };
    let intent = json!({"workout_id": workout_id, "rating": rating, "observed_at": null, "waited_count": 0, "notes": null});
    mutate(db, "crowd.feedback_received", request_id, intent, now, || {
        let mut workout = crate::domain::workout::load_workout(db, workout_id, None)?;
        let Some(at) = workout.arrived_at else {
            crate::bail!("INVALID_FEEDBACK_TIME", "Feedback must describe this visit between arrival and completion/current time");
        };
        if now < at || at > workout.completed_at.unwrap_or(now) {
            crate::bail!("INVALID_FEEDBACK_TIME", "Feedback must describe this visit between arrival and completion/current time");
        }
        if CrowdFeedback::first(db, "WHERE workout_session_id = ?1", [workout_id])?.is_some() {
            crate::bail!("CROWD_FEEDBACK_EXISTS", "This visit already has crowd feedback; retry original request ID");
        }
        let model = CrowdModel::new(db, at)?;
        let prediction = model.predict(at);
        let mut features = Map::new();
        let rows: Vec<&CrowdObservation> = model
            .observations
            .iter()
            .filter(|r| r.source == SOURCE && (at - r.observed_at) + SignedDuration::from_secs(r.freshness_seconds.unwrap_or(0)) <= MAX_PAIR_AGE)
            .collect();
        if let Some(row) = rows.last() {
            let (score, confidence, method) = calibrated_count(row.raw_value, &model.count_pairs);
            features.insert(SOURCE.into(), json!({"raw_value": row.raw_value, "observation_id": row.id, "age_seconds": (at - row.observed_at).as_secs(),
                "prediction_before_label": score, "prediction_confidence": confidence, "method": method,
                "provider_freshness_seconds": row.freshness_seconds, "demo": row.metadata_json.get("demo").and_then(Value::as_bool).unwrap_or(false)}));
            if let (Some(mut state), Some(score)) = (CrowdSourceState::get(db, SOURCE)?, score) {
                // Prequential error only: label is never used to predict itself.
                let quality = (1.0 - (score - normalized).abs()).max(0.1);
                state.reliability = 0.8 * state.reliability + 0.2 * quality;
                state.evidence_count += 1;
                state.update(db)?;
            }
        }
        let demo = features.values().any(|f| f["demo"].as_bool().unwrap_or(false));
        let paired: Vec<String> = features.keys().cloned().collect();
        let mut stored = features.clone();
        stored.insert("demo".into(), json!(demo));
        stored.insert("prediction_before_label".into(), json!(prediction.score));
        stored.insert("recorded_at".into(), json!(now));
        let feedback = CrowdFeedback { id: new_id(), workout_session_id: workout_id.into(), observed_at: at, rating: rating.clone(), normalized_score: normalized,
            features_json: Json(Value::Object(stored)), waited_for_equipment_count: 0, notes: None };
        feedback.insert(db)?;
        workout.crowd_feedback = Some(rating.clone());
        workout.update(db)?;
        let counts: Vec<f64> = CrowdFeedback::select(db, "WHERE rating IN ('BUSY', 'PACKED')", [])?
            .iter()
            .filter_map(|f| f.features_json.get(SOURCE).and_then(|g| g["raw_value"].as_f64()))
            .collect();
        if counts.len() >= 3 {
            let key = "crowd_discomfort_reported_count";
            let n = counts.len() as f64;
            let value = json!({"median_reported_count": median(&counts), "meaning": "reported count associated with BUSY/PACKED labels, not capacity/guaranteed threshold", "demo": demo});
            match LearnedPreference::first(db, "WHERE key = ?1", [key])? {
                Some(mut pref) => {
                    pref.value_json = Json(value);
                    pref.confidence = n / (n + 5.0);
                    pref.evidence_count = counts.len() as i64;
                    pref.last_observed_at = now;
                    pref.update(db)?;
                }
                None => LearnedPreference { id: new_id(), key: key.into(), value_json: Json(value), confidence: n / (n + 5.0), evidence_count: counts.len() as i64,
                    first_observed_at: now, last_observed_at: now, source: "arrival_feedback".into() }
                .insert(db)?,
            }
        }
        emit(db, "planning.replan_required", now, json!({"reason": "crowd_feedback", "workout_id": workout_id}))?;
        Ok(json!({"feedback_id": feedback.id, "workout_id": workout_id, "rating": rating, "normalized_score": normalized, "observed_at": at,
            "paired_sources": paired, "demo": demo, "instruction": "Crowd feedback saved. Future slot estimates now use this visit."}))
    })
}

/// Crowd polling health: ok = a reading in the last 45 min; failing = the gym API errors; stale = no reading, no error.
pub fn polling_health(db: &Connection, now: Ts) -> Result<Value> {
    let state = CrowdSourceState::get(db, SOURCE)?;
    let Some((state, last)) = state.and_then(|s| s.last_success_at.map(|t| (s, t))) else {
        return Ok(json!({"status": "unknown", "last_success_at": null, "consecutive_failures": 0, "error": null}));
    };
    let status = if now - last <= STALE_AFTER {
        "ok"
    } else if state.consecutive_failures > 0 {
        "failing"
    } else {
        "stale"
    };
    Ok(json!({"status": status, "last_success_at": last, "consecutive_failures": state.consecutive_failures,
        "error": if status == "failing" { json!(state.last_error_code) } else { Value::Null }}))
}

/// One owner message when check-ins stop arriving, one when they're back. Never repeats. → (event id, message).
pub fn polling_alert(db: &Connection, now: Ts) -> Result<Option<(String, String)>> {
    let health = polling_health(db, now)?;
    let stale = outage_open(db, "crowd.polling_stale", "crowd.polling_recovered")?;
    let status = health["status"].as_str().unwrap_or("unknown");
    if matches!(status, "stale" | "failing") && !stale {
        let tz = get_profile(db)?.tz();
        let since = Ts::parse(health["last_success_at"].as_str().unwrap_or_default(), None)?.local(&tz);
        let event = emit(db, "crowd.polling_stale", now, health.clone())?;
        let reason = health["error"].as_str().map(|e| format!(" (gym API: {e})")).unwrap_or_default();
        return Ok(Some((event.id, format!("⚠️ No gym check-in data since {}{reason}. Crowd forecasts are going stale.", since.strftime("%a %H:%M")))));
    }
    if status == "ok" && stale {
        let event = emit(db, "crowd.polling_recovered", now, health)?;
        return Ok(Some((event.id, "✅ Gym check-in data is flowing again.".into())));
    }
    Ok(None)
}

/// Per-slot crowd forecast plus learned time-of-day habit (see `habits`). Empty without either.
pub fn planning_signals(db: &Connection, week_start: Date, now: Ts, step_minutes: i64) -> Result<Vec<SlotSignal>> {
    let model = CrowdModel::new(db, now)?;
    let has_crowd = model.has_data();
    let habits = crate::domain::habits::evidence(db, now, &model.zone)?;
    if !has_crowd && habits.is_empty() {
        return Ok(vec![]);
    }
    let profile = get_profile(db)?;
    let mut result = Vec::new();
    for offset in 0..7 {
        let day = week_start.checked_add(jiff::Span::new().days(offset))?;
        let mut start = Ts(day.to_datetime(profile.earliest_workout_start).to_zoned(model.zone.clone())?.timestamp());
        let end = Ts(day.to_datetime(profile.latest_workout_finish).to_zoned(model.zone.clone())?.timestamp());
        while start < end {
            let (score, confidence) = if has_crowd {
                let p = model.predict(start);
                (p.score, p.confidence)
            } else {
                (None, 0.0)
            };
            let local = start.local(&model.zone);
            let preferred = if habits.is_empty() { 0.0 } else { crate::domain::habits::affinity(&habits, (local.hour() as i64 * 60 + local.minute() as i64) as f64) };
            if score.is_some() || !habits.is_empty() {
                result.push(SlotSignal { start, crowd: score, confidence: if score.is_some() { confidence } else { 0.0 }, calendar_convenience: 0.0, adherence: 0.0, preferred_time: preferred });
            }
            start = start.mins(step_minutes);
        }
    }
    Ok(result)
}
