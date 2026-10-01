"""Deterministic slots; maximize session count first, then aggregate score.

Recovery uses local calendar dates, not elapsed 24-hour periods. Search keeps
best slot per day then considers all weekday combinations (at most 128/week).
No provider data means unknown crowd, never fabricated occupancy.
"""
from datetime import date, datetime, timedelta, timezone
from itertools import combinations
from typing import Annotated
from zoneinfo import ZoneInfo

from pydantic import AwareDatetime, BaseModel, ConfigDict, Field, model_validator

from gymclaw.services.profile import Profile

Unit = Annotated[float, Field(ge=0, le=1, allow_inf_nan=False)]


class DomainType(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)


class Interval(DomainType):
    start: AwareDatetime
    end: AwareDatetime

    @model_validator(mode="after")
    def ordered(self):
        if self.start.astimezone(timezone.utc) >= self.end.astimezone(timezone.utc):
            raise ValueError("Interval end must follow start")
        return self


class SlotSignal(DomainType):
    start: AwareDatetime
    crowd: Unit | None = None
    confidence: Unit = 0
    calendar_convenience: Unit = 0
    adherence: Unit = 0
    preferred_time: Unit = 0


class PlannerConfig(DomainType):
    step_minutes: Annotated[int, Field(ge=1, le=60)] = 15
    adherence_weight: Annotated[float, Field(ge=0, allow_inf_nan=False)] = 1
    preferred_time_weight: Annotated[float, Field(ge=0, allow_inf_nan=False)] = 1
    uncertainty_weight: Annotated[float, Field(ge=0, allow_inf_nan=False)] = 0.5
    shortened_duration_penalty: Annotated[float, Field(ge=0, allow_inf_nan=False)] = 1


class Candidate(DomainType):
    start: AwareDatetime
    end: AwareDatetime
    prep_start: AwareDatetime
    leave_home: AwareDatetime
    home_at: AwareDatetime
    crowd: Unit | None
    confidence: Unit
    score: float
    score_parts: dict[str, float]


class Plan(DomainType):
    week_start: date
    sessions: tuple[Candidate, ...]
    existing_count: int
    minimum_met: bool
    shortfall: int


def overlaps(start, end, interval: Interval) -> bool:
    return start < interval.end.astimezone(timezone.utc) and interval.start.astimezone(timezone.utc) < end


def recovery_ok(start: datetime, end: datetime, other: Interval, profile: Profile) -> bool:
    zone = ZoneInfo(profile.timezone)
    first, last = start.astimezone(zone).date(), end.astimezone(zone).date()
    other_first, other_last = other.start.astimezone(zone).date(), other.end.astimezone(zone).date()
    required = profile.minimum_full_rest_days_between_sessions + 1
    return (first - other_last).days >= required or (other_first - last).days >= required


def generate_candidates(profile: Profile, week_start: date, *, busy: tuple[Interval, ...] = (), unavailable: tuple[Interval, ...] = (), existing: tuple[Interval, ...] = (), signals: tuple[SlotSignal, ...] = (), now: datetime, config: PlannerConfig = PlannerConfig()) -> tuple[Candidate, ...]:
    if week_start.weekday() != 0:
        raise ValueError("week_start must be Monday")
    if now.tzinfo is None or now.utcoffset() is None:
        raise ValueError("now must include timezone")
    now = now.astimezone(timezone.utc)
    zone = ZoneInfo(profile.timezone)
    signal_map = {s.start.astimezone(timezone.utc): s for s in signals}
    result = []
    for offset in range(7):
        day = week_start + timedelta(days=offset)
        if day.weekday() not in profile.weekdays_allowed:
            continue
        earliest = datetime.combine(day, profile.earliest_workout_start, zone)
        latest = datetime.combine(day, profile.latest_workout_finish, zone).astimezone(timezone.utc)
        # Iterate real instants: skips nonexistent spring times, keeps both fall folds.
        start = earliest.astimezone(timezone.utc)
        while start < latest:
            local_start = start.astimezone(zone)
            if local_start.date() != day or local_start.time().replace(tzinfo=None) < profile.earliest_workout_start:
                start += timedelta(minutes=config.step_minutes)
                continue
            for duration in sorted({profile.preferred_workout_minutes, profile.minimum_workout_minutes}, reverse=True):
                end = start + timedelta(minutes=duration)
                prep = start - timedelta(minutes=profile.prep_minutes + profile.commute_to_gym_minutes)
                home = end + timedelta(minutes=profile.commute_home_minutes)
                if prep < now or end > latest:
                    continue
                if any(overlaps(prep, home, blocker) for blocker in busy + unavailable):
                    continue
                if any(not recovery_ok(start, end, session, profile) for session in existing):
                    continue
                signal = signal_map.get(start, SlotSignal(start=start))
                confidence = signal.confidence if signal.crowd is not None else 0
                parts = {
                    "crowd": profile.crowd_preference_weight * (1 - signal.crowd) if signal.crowd is not None else 0,
                    "calendar": profile.calendar_preference_weight * signal.calendar_convenience,
                    "adherence": config.adherence_weight * signal.adherence,
                    "preferred_time": config.preferred_time_weight * signal.preferred_time,
                    "uncertainty": -config.uncertainty_weight * (1 - confidence),
                    "duration": -config.shortened_duration_penalty * (profile.preferred_workout_minutes - duration) / profile.preferred_workout_minutes,
                }
                result.append(Candidate(start=start, end=end, prep_start=prep, leave_home=start - timedelta(minutes=profile.commute_to_gym_minutes), home_at=home, crowd=signal.crowd, confidence=confidence, score=sum(parts.values()), score_parts=parts))
            start += timedelta(minutes=config.step_minutes)
    return tuple(result)


def plan_week(profile: Profile, week_start: date, *, busy: tuple[Interval, ...] = (), unavailable: tuple[Interval, ...] = (), existing: tuple[Interval, ...] = (), signals: tuple[SlotSignal, ...] = (), now: datetime, config: PlannerConfig = PlannerConfig()) -> Plan:
    zone = ZoneInfo(profile.timezone)
    existing_count = sum(week_start <= s.start.astimezone(zone).date() < week_start + timedelta(days=7) for s in existing)
    capacity = max(0, min(profile.weekly_target_sessions, profile.weekly_max_sessions) - existing_count)
    candidates = generate_candidates(profile, week_start, busy=busy, unavailable=unavailable, existing=existing, signals=signals, now=now, config=config)
    by_day: dict[date, Candidate] = {}
    for candidate in candidates:
        day = candidate.start.astimezone(zone).date()
        current = by_day.get(day)
        if current is None or candidate.score > current.score:
            by_day[day] = candidate
    days = sorted(by_day)
    best: tuple[Candidate, ...] = ()
    best_key = (-1, float("-inf"))
    for size in range(min(capacity, len(days)) + 1):
        for selected in combinations(days, size):
            if any((right - left).days <= profile.minimum_full_rest_days_between_sessions for left, right in zip(selected, selected[1:])):
                continue
            slots = tuple(by_day[d] for d in selected)
            key = (size, sum(c.score for c in slots))
            if key > best_key:
                best, best_key = slots, key
    total = existing_count + len(best)
    return Plan(week_start=week_start, sessions=best, existing_count=existing_count, minimum_met=total >= profile.weekly_min_sessions, shortfall=max(0, profile.weekly_target_sessions - total))
