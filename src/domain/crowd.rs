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
use crate::providers::crowd::{CrowdProvider, METRIC};
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

/// The crowd source row, created on first use. A DB bound to another gym/provider refuses to mix data.
fn bind_source(db: &Connection, source: &str, provider_id: &str) -> Result<CrowdSourceState> {
    match CrowdSourceState::get(db, source)? {
        Some(state) if state.provider_id != provider_id => crate::bail!("CROWD_SOURCE_MISMATCH", "DB is bound to another crowd studio/provider; use separate DB"),
        Some(state) => Ok(state),
        None => {
            let state = CrowdSourceState {
                source: source.into(), provider_id: provider_id.into(), last_success_at: None, last_failure_at: None, last_error_code: None,
                last_change_at: None, last_raw_value: None, consecutive_failures: 0, reliability: 0.5, evidence_count: 0,
            };
            state.insert(db)?;
            Ok(state)
        }
    }
}

/// Take one reading and store it as an observation. A failed read is recorded (health, event) and
/// returned as unavailable, so the caller commits it. `now` is taken after the fetch when None.
pub fn poll(db: &Connection, provider: &dyn CrowdProvider, now: Option<Ts>) -> Result<Value> {
    if let Some(existing) = CrowdSourceState::get(db, provider.source())?
        && existing.provider_id != provider.provider_id()
    {
        crate::bail!("CROWD_SOURCE_MISMATCH", "DB is bound to another crowd studio/provider; use separate DB");
    }
    // Fetch before writing so a slow gym API holds no SQLite write lock.
    let reading = provider.get_reading();
    let now = now.unwrap_or_else(Ts::now);
    let mut state = bind_source(db, provider.source(), provider.provider_id())?;
    if state.last_success_at.is_some_and(|last| now < last) {
        crate::bail!("TIME_REVERSED", "Poll precedes last successful retrieval");
    }
    let reading = match reading {
        Ok(reading) => reading,
        Err(error @ crate::error::Error::Domain { .. }) => {
            state.last_failure_at = Some(now);
            state.last_error_code = Some(error.code().to_string());
            state.consecutive_failures += 1;
            state.update(db)?;
            let event = emit(db, "crowd.source_unavailable", now, json!({"source": provider.source(), "code": error.code()}))?;
            return Ok(json!({"data": {"available": false, "source": provider.source(), "error_code": error.code(), "count_stored": false},
                "events": [{"id": event.id, "type": event.r#type}], "user_message_hint": null}));
        }
        Err(other) => return Err(other),
    };
    let raw_value = reading.raw_value as f64;
    let previous_at = state.last_success_at;
    let changed = state.last_raw_value.is_some_and(|last| last != raw_value);
    if state.last_raw_value.is_none() || changed {
        state.last_change_at = Some(now);
    }
    let unchanged_for = state.last_change_at.map_or(0, |at| (now - at).as_secs());
    let window = match previous_at {
        Some(after) if changed => json!({"after": after, "by": now}),
        _ => Value::Null,
    };
    let metadata = json!({"provider_id": provider.provider_id(), "metric": METRIC, "demo": provider.demo(), "retrieved_at": now,
        "provider_timestamp": null, "freshness_known": reading.freshness_seconds.is_some(),
        "changed_since_previous": if previous_at.is_some() { json!(changed) } else { Value::Null },
        "unchanged_for_seconds": unchanged_for, "possible_change_window": window});
    let row = CrowdObservation {
        id: new_id(), observed_at: now, source: provider.source().into(), raw_value, normalized_value: None,
        freshness_seconds: reading.freshness_seconds.map(|s| s as i64), metadata_json: Json(metadata.clone()),
    };
    row.insert(db)?;
    state.last_success_at = Some(now);
    state.last_raw_value = Some(raw_value);
    state.last_error_code = None;
    state.consecutive_failures = 0;
    state.update(db)?;
    Ok(json!({"data": {"available": true, "observation_id": row.id, "source": row.source, "metric": METRIC, "raw_value": reading.raw_value,
        "normalized_value": null, "retrieved_at": now, "provider_freshness_seconds": row.freshness_seconds, "metadata": metadata},
        "events": [], "user_message_hint": null}))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{AgentEvent, WorkoutSession};
    use crate::providers::crowd::{CrowdReading, FixtureCrowdProvider, MySportsProvider};
    use crate::providers::http::{Request, Response};

    /// Wednesday 20:00 Berlin.
    fn now(minutes: i64) -> Ts {
        Ts::parse("2026-10-14T18:00:00+00:00", None).unwrap().mins(minutes)
    }

    fn provider(count: u64) -> FixtureCrowdProvider {
        FixtureCrowdProvider { reading: CrowdReading::count(count) }
    }

    fn visit(db: &Connection, at: Ts) -> String {
        let id = new_id();
        db.execute(
            "INSERT INTO workout_session (id, status, started_at, arrived_at, completed_at, waited_for_equipment_count, template_snapshot) VALUES (?1, 'PLAN_UPDATED', ?2, ?2, ?3, 0, '{\"id\": \"historical-test-visit\"}')",
            (&id, at, at.mins(60)),
        )
        .unwrap();
        id
    }

    fn db() -> Connection {
        crate::domain::workout::tests::db()
    }

    #[test]
    fn unknown_freshness_change_window_and_restart() {
        let db = db();
        let first = poll(&db, &provider(7), Some(now(0))).unwrap();
        let again = poll(&db, &provider(7), Some(now(15))).unwrap();
        let changed = poll(&db, &provider(9), Some(now(30))).unwrap();
        assert_eq!(first["data"]["provider_freshness_seconds"], Value::Null);
        assert_eq!(again["data"]["metadata"]["unchanged_for_seconds"], 900);
        assert_eq!(changed["data"]["metadata"]["possible_change_window"], json!({"after": now(15).iso(), "by": now(30).iso()}));
        assert_eq!(CrowdObservation::select(&db, "", []).unwrap().len(), 3);
        let health = &source_health(&db, now(31)).unwrap()["sources"][0];
        assert_eq!((&health["retrieval_age_seconds"], &health["provider_freshness_known"]), (&json!(60), &json!(false)));
        let prediction = CrowdModel::new(&db, now(31)).unwrap().predict(now(31));
        assert_eq!(prediction.score, None); // A count alone is not personalized occupancy.
        assert_eq!(prediction.json["features"]["GYM_API"]["raw_value"], 9.0);
    }

    struct Broken;

    impl CrowdProvider for Broken {
        fn provider_id(&self) -> &str {
            "fixture:GYM_API"
        }
        fn get_reading(&self) -> Result<CrowdReading> {
            crate::bail!("GYM_API_UNAVAILABLE", "No source")
        }
    }

    #[test]
    fn unavailable_source_keeps_history_and_health() {
        let db = db();
        poll(&db, &provider(7), Some(now(0))).unwrap();
        assert_eq!(poll(&db, &Broken, Some(now(15))).unwrap()["data"]["available"], false);
        assert_eq!(CrowdObservation::select(&db, "", []).unwrap().len(), 1);
        let health = &source_health(&db, now(16)).unwrap()["sources"][0];
        assert_eq!(health["consecutive_failures"], 1);
        assert_eq!(health["last_raw_value"], 7.0);
        assert_eq!(health["last_success_at"], now(0).iso());
        assert_eq!(health["last_error_code"], "GYM_API_UNAVAILABLE");
    }

    #[test]
    fn feedback_uses_only_prearrival_signals_and_is_idempotent() {
        let db = db();
        poll(&db, &provider(40), Some(now(-5))).unwrap();
        let workout = visit(&db, now(0));
        poll(&db, &provider(100), Some(now(30))).unwrap(); // A later poll is not evidence at arrival.
        let original = record_feedback(&db, &workout, "BUSY", now(60), "arrival").unwrap();
        assert_eq!(record_feedback(&db, &workout, "BUSY", now(120), "arrival").unwrap(), original);
        let label = CrowdFeedback::first(&db, "", []).unwrap().unwrap();
        assert_eq!(label.observed_at, now(0));
        assert_eq!(label.features_json["GYM_API"]["raw_value"], 40.0);
        assert_eq!(label.features_json["GYM_API"]["prediction_before_label"], Value::Null);
        assert_eq!(label.features_json["demo"], true);
        assert_eq!(WorkoutSession::get(&db, &workout).unwrap().unwrap().crowd_feedback.as_deref(), Some("BUSY"));
        let prediction = CrowdModel::new(&db, now(60)).unwrap().predict(now(7 * 24 * 60));
        assert!((prediction.score.unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert!(0.0 < prediction.confidence && prediction.confidence < 0.5);
        assert!(prediction.demo && prediction.json["forecast_accuracy_established"] == false);
        assert!(record_feedback(&db, &workout, "EMPTY", now(120), "different").unwrap_err().to_string().contains("already has"));
        assert!(record_feedback(&db, &workout, "EMPTY", now(120), "arrival").unwrap_err().to_string().contains("different intent"));
    }

    #[test]
    fn old_signal_not_paired_and_future_labels_do_not_leak() {
        let db = db();
        poll(&db, &provider(7), Some(now(-120))).unwrap();
        let workout = visit(&db, now(0));
        record_feedback(&db, &workout, "FINE", now(60), "late").unwrap();
        assert!(CrowdFeedback::first(&db, "", []).unwrap().unwrap().features_json.get("GYM_API").is_none());
        assert_eq!(CrowdModel::new(&db, now(0)).unwrap().predict(now(0)).score, None);
        let score = CrowdModel::new(&db, now(60)).unwrap().predict(now(7 * 24 * 60)).score.unwrap();
        assert!((score - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn discomfort_evidence_from_labels() {
        let db = db();
        for index in 0..3 {
            let at = now(-(2 - index) * 7 * 24 * 60);
            poll(&db, &provider(60 + index as u64), Some(at)).unwrap();
            let workout = visit(&db, at);
            record_feedback(&db, &workout, "PACKED", at.mins(60), &format!("label-{index}")).unwrap();
        }
        assert_eq!(CrowdSourceState::get(&db, "GYM_API").unwrap().unwrap().evidence_count, 2);
        let preference = LearnedPreference::first(&db, "WHERE key = 'crowd_discomfort_reported_count'", []).unwrap().unwrap();
        assert_eq!(preference.evidence_count, 3);
        assert_eq!(preference.value_json["median_reported_count"], 61.0);
        assert!(preference.value_json["meaning"].as_str().unwrap().contains("not capacity"));
    }

    #[test]
    fn calibration_monotonic_after_ten_labels_not_capacity_conversion() {
        let pairs = [(0.0, 0.0), (10.0, 0.0), (20.0, 0.3), (30.0, 0.2), (40.0, 0.6), (50.0, 0.7), (60.0, 0.5), (70.0, 0.8), (80.0, 1.0), (90.0, 1.0)];
        let values: Vec<f64> = (0..100).map(|x| calibrated_count(x as f64, &pairs).0.unwrap()).collect();
        assert!(values.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(calibrated_count(30.0, &pairs).2, "personalized_isotonic");
        assert!(calibrated_count(500.0, &pairs).1 < calibrated_count(50.0, &pairs).1);
        assert_eq!(calibrated_count(100.0, &[]).0, None);
    }

    #[test]
    fn many_polls_do_not_inflate_independent_evidence_days() {
        let db = db();
        for minute in 0..15 {
            poll(&db, &provider(7), Some(now(minute))).unwrap();
        }
        let predicted = CrowdModel::new(&db, now(60)).unwrap().predict(now(7 * 24 * 60));
        assert_eq!(predicted.json["features"]["GYM_API"]["evidence_days"], 1);
        assert_eq!(predicted.score, None);
    }

    #[test]
    fn calendar_planning_uses_arrival_labels() {
        let db = db();
        let changes = json!({"weekly_min_sessions": 1, "weekly_target_sessions": 1, "weekly_max_sessions": 1, "weekdays_allowed": [2],
            "earliest_workout_start": "19:00", "latest_workout_finish": "22:00"});
        crate::domain::profile::update_profile(&db, changes.as_object().unwrap()).unwrap();
        for (hour, rating) in [(17, "PACKED"), (18, "EMPTY")] {
            let at = Ts::parse(&format!("2026-10-07T{hour}:00:00+00:00"), None).unwrap();
            let workout = visit(&db, at);
            record_feedback(&db, &workout, rating, at.mins(60), &format!("label-{hour}")).unwrap();
        }
        let plan = crate::domain::scheduling::schedule_week(&db, Date::constant(2026, 10, 19), now(0), Some("future-week"), None, &[]).unwrap();
        assert_eq!(plan["created_count"], 1);
        assert_eq!(Ts::parse(plan["sessions"][0]["start"].as_str().unwrap(), None).unwrap().0.strftime("%H").to_string(), "18");
        assert_eq!(plan["sessions"][0]["crowd"], 0.0);
        assert_eq!(plan["sessions"][0]["workout_plan"]["crowd_score_kind"], "personal_perceived_crowd_proxy");
    }

    #[test]
    fn fixture_source_cannot_replace_live_studio() {
        let db = db();
        poll(&db, &provider(7), Some(now(0))).unwrap();
        let transport = |_: &Request| Ok(Response { status: 200, body: br#"{"value": 7}"#.to_vec() });
        let live = MySportsProvider::new("1234567890", "fixture-tenant", &transport).unwrap();
        assert_eq!(poll(&db, &live, Some(now(0))).unwrap_err().code(), "CROWD_SOURCE_MISMATCH");
    }

    #[test]
    fn polling_alert_once_when_stale_and_once_when_back() {
        let db = db();
        poll(&db, &provider(7), Some(now(0))).unwrap();
        assert_eq!(polling_health(&db, now(30)).unwrap()["status"], "ok");
        assert_eq!(polling_alert(&db, now(30)).unwrap(), None);
        let (event_id, message) = polling_alert(&db, now(60)).unwrap().unwrap();
        assert!(message.starts_with("⚠️ No gym check-in data since"), "{message}");
        let mut event = AgentEvent::get(&db, &event_id).unwrap().unwrap();
        event.handled_at = Some(now(60)); // Delivered.
        event.update(&db).unwrap();
        assert_eq!(polling_alert(&db, now(120)).unwrap(), None); // No repeats.
        poll(&db, &provider(9), Some(now(180))).unwrap();
        assert!(polling_alert(&db, now(181)).unwrap().unwrap().1.starts_with('✅'));
        assert_eq!(polling_alert(&db, now(182)).unwrap(), None);
    }
}
