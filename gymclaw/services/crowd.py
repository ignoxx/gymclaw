"""Deterministic crowd collection, conservative label calibration and slot signals.

No gym capacity assumption or count→occupancy-percent conversion. Ratings define
personal score; confidence is heuristic evidence strength, not forecast accuracy.
"""
from collections import defaultdict
from datetime import date, datetime, timedelta, timezone
from statistics import mean, median
from zoneinfo import ZoneInfo

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent, CrowdFeedback, CrowdObservation, CrowdSourceState, LearnedPreference, WorkoutSession
from gymclaw.providers.crowd import CrowdProvider
from gymclaw.services.errors import DomainError
from gymclaw.services.planning import SlotSignal
from gymclaw.services.profile import get_profile
from gymclaw.services.workout import emit, load_workout, mutate, utc

RATINGS = {"EMPTY": 0.0, "FINE": 1 / 3, "BUSY": 2 / 3, "PACKED": 1.0}
MAX_PAIR_AGE = timedelta(minutes=30)
HISTORY = timedelta(days=90)


def bind_source(db: Session, source: str, provider_id: str) -> CrowdSourceState:
    if source != "GYM_API":
        raise ValueError("Unknown crowd source")
    state = db.get(CrowdSourceState, source)
    if state is None:
        state = CrowdSourceState(source=source, provider_id=provider_id)
        db.add(state); db.flush()
    elif state.provider_id != provider_id:
        raise DomainError("CROWD_SOURCE_MISMATCH", "DB is bound to another crowd studio/provider; use separate DB")
    return state


def poll(db: Session, provider: CrowdProvider, *, now: datetime | None = None) -> dict:
    """Caller commits even unavailable result, preserving failed-read health."""
    existing = db.get(CrowdSourceState, provider.source)
    if existing and existing.provider_id != provider.provider_id:
        raise DomainError("CROWD_SOURCE_MISMATCH", "DB is bound to another crowd studio/provider; use separate DB")
    # Fetch before inserting/updating state so CLI poll holds no SQLite write lock.
    try:
        reading = provider.get_reading()
    except DomainError as error:
        now = utc(now or datetime.now(timezone.utc))
        state = bind_source(db, provider.source, provider.provider_id)
        if state.last_success_at and now < state.last_success_at:
            raise DomainError("TIME_REVERSED", "Poll precedes last successful retrieval")
        state.last_failure_at, state.last_error_code = now, error.code
        state.consecutive_failures += 1
        event = emit(db, "crowd.source_unavailable", now, {"source": provider.source, "code": error.code})
        return {"data": {"available": False, "source": provider.source, "error_code": error.code, "count_stored": False},
            "events": [{"id": event.id, "type": event.type}], "user_message_hint": None}
    now = utc(now or datetime.now(timezone.utc))  # Local retrieval timestamp, after response.
    state = bind_source(db, provider.source, provider.provider_id)
    if state.last_success_at and now < state.last_success_at:
        raise DomainError("TIME_REVERSED", "Poll precedes last successful retrieval")
    if reading.source != provider.source:
        raise DomainError("CROWD_SOURCE_MISMATCH", "Provider returned another source's reading")
    previous_at = state.last_success_at
    changed = state.last_raw_value is not None and state.last_raw_value != reading.raw_value
    if state.last_raw_value is None or changed:
        state.last_change_at = now
    metadata = {"provider_id": provider.provider_id, "metric": reading.metric,
        "demo": provider.demo, "retrieved_at": now.isoformat(), "provider_timestamp": None,
        "freshness_known": reading.freshness_seconds is not None, "changed_since_previous": changed if previous_at else None,
        "unchanged_for_seconds": int((now - state.last_change_at).total_seconds()),
        "possible_change_window": {"after": previous_at.isoformat(), "by": now.isoformat()} if changed and previous_at else None}
    row = CrowdObservation(observed_at=now, source=reading.source, raw_value=reading.raw_value,
        normalized_value=reading.normalized_value, freshness_seconds=reading.freshness_seconds, metadata_json=metadata)
    db.add(row)
    state.last_success_at, state.last_raw_value = now, reading.raw_value
    state.last_error_code, state.consecutive_failures = None, 0
    db.flush()
    return {"data": {"available": True, "observation_id": row.id, "source": row.source,
        "metric": reading.metric, "raw_value": row.raw_value, "normalized_value": row.normalized_value,
        "retrieved_at": now.isoformat(), "provider_freshness_seconds": row.freshness_seconds,
        "metadata": metadata}, "events": [], "user_message_hint": None}


def source_health(db: Session, *, now: datetime) -> dict:
    now = utc(now)
    sources = []
    for state in db.scalars(select(CrowdSourceState).order_by(CrowdSourceState.source)):
        latest = db.scalar(select(CrowdObservation).where(CrowdObservation.source == state.source).order_by(CrowdObservation.observed_at.desc(), CrowdObservation.id.desc()).limit(1))
        sources.append({"source": state.source, "provider_id": state.provider_id,
            "last_success_at": state.last_success_at.isoformat() if state.last_success_at else None,
            "last_failure_at": state.last_failure_at.isoformat() if state.last_failure_at else None,
            "retrieval_age_seconds": max(0, int((now - state.last_success_at).total_seconds())) if state.last_success_at else None,
            "provider_freshness_known": latest is not None and latest.freshness_seconds is not None,
            "provider_freshness_seconds_at_retrieval": latest.freshness_seconds if latest else None,
            "consecutive_failures": state.consecutive_failures,
            "last_error_code": state.last_error_code, "last_raw_value": state.last_raw_value,
            "last_change_at": state.last_change_at.isoformat() if state.last_change_at else None,
            "reliability": state.reliability, "reliability_evidence_count": state.evidence_count})
    return {"sources": sources}


def calibrated_count(count: float, pairs: list[tuple[float, float]]) -> tuple[float | None, float, str]:
    if not pairs:
        return None, 0, "uncalibrated_reported_count"
    if len(pairs) < 10:
        neighbors = sorted(pairs, key=lambda pair: abs(pair[0] - count))[:3]
        weights = [1 / (1 + abs(x - count)) for x, _ in neighbors]
        value = sum(y * w for (_, y), w in zip(neighbors, weights)) / sum(weights)
        distance = min(abs(x - count) for x, _ in pairs)
        return value, min(0.35, len(pairs) / 20) / (1 + distance / 20), "label_neighbor_heuristic"
    # Pool-adjacent-violators: small monotonic, label-calibrated regression.
    grouped = defaultdict(list)
    for x, y in pairs:
        grouped[x].append(y)
    blocks = []
    for x, values in sorted(grouped.items()):
        blocks.append([x, x, sum(values), len(values)])
        while len(blocks) >= 2 and blocks[-2][2] / blocks[-2][3] > blocks[-1][2] / blocks[-1][3]:
            right, left = blocks.pop(), blocks.pop()
            blocks.append([left[0], right[1], left[2] + right[2], left[3] + right[3]])
    points = [((lo + hi) / 2, total / size) for lo, hi, total, size in blocks]
    score = points[0][1] if count <= points[0][0] else points[-1][1]
    for (x0, y0), (x1, y1) in zip(points, points[1:]):
        if x0 <= count <= x1:
            score = y0 + (y1 - y0) * (count - x0) / (x1 - x0)
            break
    low, high = min(x for x, _ in pairs), max(x for x, _ in pairs)
    extrapolation_penalty = 0.5 if count < low or count > high else 1
    return score, min(0.8, len(pairs) / (len(pairs) + 10)) * extrapolation_penalty, "personalized_isotonic"


FEELS = (("Empty", 1 / 6), ("Fine", 0.5), ("Busy", 5 / 6), ("Packed", float("inf")))


def feel(score: float | None) -> str | None:
    """Personal 0–1 score as the owner's own words (Empty/Fine/Busy/Packed)."""
    return None if score is None else next(label for label, limit in FEELS if score < limit)


class CrowdModel:
    """Load evidence once per planning pass; no per-candidate DB queries."""
    def __init__(self, db: Session, *, now: datetime):
        self.now = utc(now)
        self.zone = ZoneInfo(get_profile(db).timezone)
        self.observations = list(db.scalars(select(CrowdObservation).where(CrowdObservation.observed_at <= self.now,
            CrowdObservation.observed_at >= self.now - HISTORY).order_by(CrowdObservation.observed_at, CrowdObservation.id)))
        self.labels = [label for label in db.scalars(select(CrowdFeedback).where(CrowdFeedback.observed_at <= self.now,
            CrowdFeedback.observed_at >= self.now - HISTORY).order_by(CrowdFeedback.observed_at, CrowdFeedback.id))
            if utc(datetime.fromisoformat(label.features_json.get("recorded_at", label.observed_at.isoformat()))) <= self.now]
        self.states = {state.source: state for state in db.scalars(select(CrowdSourceState))}
        self.count_pairs = [(f.features_json["GYM_API"]["raw_value"], f.normalized_score) for f in self.labels if "GYM_API" in f.features_json]

    def same_slot(self, left: datetime, right: datetime) -> bool:
        left, right = left.astimezone(self.zone), right.astimezone(self.zone)
        return (left.weekday(), left.hour) == (right.weekday(), right.hour)

    def expected_count(self, at: datetime) -> dict | None:
        """Typical reported check-ins: same weekday and hour, else the same hour on other days."""
        at = utc(at)
        rows = [row for row in self.observations if row.source == "GYM_API"]
        local = at.astimezone(self.zone)
        same = [row for row in rows if self.same_slot(row.observed_at, at)]
        basis = "weekday_hour"
        if not same:
            same = [row for row in rows if row.observed_at.astimezone(self.zone).hour == local.hour]
            basis = "hour"
        if not same:
            return None
        daily = defaultdict(list)
        for row in same:
            daily[row.observed_at.astimezone(self.zone).date()].append(row.raw_value)
        return {"count": round(mean(mean(v) for v in daily.values())), "basis": basis, "days": len(daily)}

    def forecast(self, at: datetime) -> dict | None:
        expected = self.expected_count(at)
        score = self.predict(at)["score"]
        if expected is None and score is None:
            return None
        return (expected or {}) | {"feel": feel(score), "score": score}

    def signal_feature(self, source: str, at: datetime) -> dict | None:
        rows = [row for row in self.observations if row.source == source and row.observed_at <= at]
        latest = rows[-1] if rows else None
        if latest and self.now <= at <= self.now + MAX_PAIR_AGE and at - latest.observed_at <= MAX_PAIR_AGE:
            selected = [latest]
            kind = "recent_reported_signal"
        else:
            selected = [row for row in rows if self.same_slot(row.observed_at, at)]
            kind = "locally_collected_slot_history"
        if not selected:
            return None
        # One mean per dated hour: repeated polls/cache values are not independent days.
        daily = defaultdict(list)
        for row in selected:
            daily[row.observed_at.astimezone(self.zone).date()].append(row.raw_value)
        raw = mean(mean(values) for values in daily.values())
        days = len(daily)
        freshness_penalty = 0.5 if any(row.freshness_seconds is None for row in selected) else 1
        history_confidence = min(0.6, days / (days + 5)) * freshness_penalty
        if kind == "recent_reported_signal":
            history_confidence = 0.5 * freshness_penalty
        state = self.states.get(source)
        reliability = state.reliability if state else 0.5
        if state and state.consecutive_failures:
            history_confidence *= 0.5
        score, calibration_confidence, method = calibrated_count(raw, self.count_pairs)
        confidence = min(history_confidence, calibration_confidence)
        return {"raw_value": raw, "score": score, "confidence": confidence, "reliability": reliability,
            "method": method, "evidence_days": days, "signal_kind": kind,
            "observation_id": selected[-1].id, "demo": any(row.metadata_json.get("demo", False) for row in selected)}

    def predict(self, at: datetime) -> dict:
        at = utc(at)
        features = {source: feature for source in ("GYM_API",) if (feature := self.signal_feature(source, at)) is not None}
        slot_labels = [f for f in self.labels if f.observed_at <= at and self.same_slot(f.observed_at, at)]
        components = [(f["score"], f["confidence"] * f["reliability"]) for f in features.values() if f["score"] is not None and f["confidence"] > 0]
        if slot_labels:
            count = len(slot_labels)
            components.append((mean(f.normalized_score for f in slot_labels), min(0.8, count / (count + 5))))
        weight = sum(w for _, w in components)
        score = sum(value * w for value, w in components) / weight if weight else None
        confidence = min(0.85, weight / (weight + 0.5)) if weight else 0
        demo = any(f["demo"] for f in features.values()) or any(f.features_json.get("demo", False) for f in slot_labels)
        return {"at": at.isoformat(), "score": score, "confidence": confidence,
            "score_kind": "personal_perceived_crowd_proxy", "label_evidence": len(self.labels),
            "slot_label_evidence": len(slot_labels), "features": features,
            "demo": demo, "forecast_accuracy_established": False,
            "instruction": "Crowd unknown; reported count needs arrival labels." if score is None else "Crowd estimate uses available signals and user labels; confidence is heuristic."}


def record_feedback(db: Session, workout_id: str, *, rating: str, now: datetime, request_id: str,
                    observed_at: datetime | None = None, waited_count: int = 0, notes: str | None = None) -> dict:
    now = utc(now)
    rating = rating.upper()
    if rating not in RATINGS or not isinstance(waited_count, int) or isinstance(waited_count, bool) or not 0 <= waited_count <= 100 or notes is not None and len(notes) > 1000:
        raise ValueError("Use EMPTY/FINE/BUSY/PACKED, wait count 0–100 and notes ≤1000 chars")

    def action():
        workout = load_workout(db, workout_id)
        at = utc(observed_at or workout.arrived_at)
        if at < workout.arrived_at or at > (workout.completed_at or now) or now < at:
            raise DomainError("INVALID_FEEDBACK_TIME", "Feedback must describe this visit between arrival and completion/current time")
        if db.scalar(select(CrowdFeedback).where(CrowdFeedback.workout_session_id == workout_id)):
            raise DomainError("CROWD_FEEDBACK_EXISTS", "This visit already has crowd feedback; retry original request ID")
        model = CrowdModel(db, now=at)
        prediction = model.predict(at)
        features = {}
        for source in ("GYM_API",):
            rows = [row for row in model.observations if row.source == source and at - row.observed_at + timedelta(seconds=row.freshness_seconds or 0) <= MAX_PAIR_AGE]
            if not rows:
                continue
            row = rows[-1]
            score, confidence, method = calibrated_count(row.raw_value, model.count_pairs)
            features[source] = {"raw_value": row.raw_value, "observation_id": row.id, "age_seconds": int((at - row.observed_at).total_seconds()),
                "prediction_before_label": score, "prediction_confidence": confidence, "method": method,
                "provider_freshness_seconds": row.freshness_seconds, "demo": row.metadata_json.get("demo", False)}
            state = db.get(CrowdSourceState, source)
            if state and score is not None:
                # Prequential error only: label is never used to predict itself.
                quality = max(0.1, 1 - abs(score - RATINGS[rating]))
                state.reliability = 0.8 * state.reliability + 0.2 * quality
                state.evidence_count += 1
        demo = any(feature["demo"] for feature in features.values())
        feedback = CrowdFeedback(workout_session_id=workout_id, observed_at=at, rating=rating, normalized_score=RATINGS[rating],
            waited_for_equipment_count=waited_count, notes=notes,
            features_json=features | {"demo": demo, "prediction_before_label": prediction["score"], "recorded_at": now.isoformat()})
        db.add(feedback)
        workout.crowd_feedback = rating
        db.flush()
        pairs = list(db.scalars(select(CrowdFeedback).where(CrowdFeedback.rating.in_(["BUSY", "PACKED"]))))
        counts = [f.features_json["GYM_API"]["raw_value"] for f in pairs if "GYM_API" in f.features_json]
        if len(counts) >= 3:
            key = "crowd_discomfort_reported_count"
            preference = db.scalar(select(LearnedPreference).where(LearnedPreference.key == key))
            if preference is None:
                preference = LearnedPreference(key=key, first_observed_at=now, source="arrival_feedback")
                db.add(preference)
            preference.value_json = {"median_reported_count": median(counts), "meaning": "reported count associated with BUSY/PACKED labels, not capacity/guaranteed threshold", "demo": demo}
            preference.confidence, preference.evidence_count = len(counts) / (len(counts) + 5), len(counts)
            preference.last_observed_at = now
        emit(db, "planning.replan_required", now, {"reason": "crowd_feedback", "workout_id": workout_id})
        return {"feedback_id": feedback.id, "workout_id": workout_id, "rating": rating, "normalized_score": feedback.normalized_score,
            "observed_at": at.isoformat(), "paired_sources": list(features), "demo": demo,
            "instruction": "Crowd feedback saved. Future slot estimates now use this visit."}

    return mutate(db, "crowd.feedback_received", request_id, {"workout_id": workout_id, "rating": rating,
        "observed_at": utc(observed_at).isoformat() if observed_at else None, "waited_count": waited_count, "notes": notes}, now, action)


STALE_AFTER = timedelta(minutes=45)


def polling_health(db: Session, *, now: datetime) -> dict:
    """ok: a reading in the last 45 min; failing: the gym API errors; stale: no reading, no error."""
    state = db.get(CrowdSourceState, "GYM_API")
    if state is None or state.last_success_at is None:
        return {"status": "unknown", "last_success_at": None, "consecutive_failures": 0, "error": None}
    status = "ok"
    if now - state.last_success_at > STALE_AFTER:
        status = "failing" if state.consecutive_failures else "stale"
    return {"status": status, "last_success_at": state.last_success_at.isoformat(),
        "consecutive_failures": state.consecutive_failures, "error": state.last_error_code if status == "failing" else None}


def polling_alert(db: Session, *, now: datetime) -> dict | None:
    """One Telegram message when check-ins stop arriving, one when they're back. Never repeats."""
    health = polling_health(db, now=now)
    open_alert = db.scalar(select(AgentEvent).where(AgentEvent.type == "crowd.polling_stale", AgentEvent.handled_at.is_(None)))
    zone = ZoneInfo(get_profile(db).timezone)
    if health["status"] in {"stale", "failing"} and open_alert is None:
        since = datetime.fromisoformat(health["last_success_at"]).astimezone(zone)
        event = emit(db, "crowd.polling_stale", now, health)
        reason = f" (gym API: {health['error']})" if health["error"] else ""
        return {"event_id": event.id, "message": f"⚠️ No gym check-in data since {since:%a %H:%M}{reason}. Crowd forecasts are going stale."}
    if health["status"] == "ok" and open_alert is not None:
        open_alert.handled_at = now
        event = emit(db, "crowd.polling_recovered", now, health)
        return {"event_id": event.id, "message": "✅ Gym check-in data is flowing again."}
    return None


def planning_signals(db: Session, week_start: date, *, now: datetime, step_minutes: int = 15) -> tuple[SlotSignal, ...]:
    """Per-slot crowd forecast plus learned time-of-day habit (see `habits`). Empty without either."""
    from gymclaw.services.habits import affinity, evidence
    model = CrowdModel(db, now=now)
    has_crowd = bool(model.observations or model.labels)
    habits = evidence(db, now=utc(now), zone=model.zone)
    if not has_crowd and not habits:
        return ()
    profile = get_profile(db)
    result = []
    for day in (week_start + timedelta(days=i) for i in range(7)):
        start = datetime.combine(day, profile.earliest_workout_start, model.zone).astimezone(timezone.utc)
        end = datetime.combine(day, profile.latest_workout_finish, model.zone).astimezone(timezone.utc)
        while start < end:
            prediction = model.predict(start) if has_crowd else {"score": None, "confidence": 0}
            local = start.astimezone(model.zone)
            preferred = affinity(habits, local.hour * 60 + local.minute) if habits else 0
            if prediction["score"] is not None or habits:
                result.append(SlotSignal(start=start, crowd=prediction["score"], confidence=prediction["confidence"] if prediction["score"] is not None else 0, preferred_time=preferred))
            start += timedelta(minutes=step_minutes)
    return tuple(result)
