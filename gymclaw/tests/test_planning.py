from datetime import date, datetime, timedelta, timezone
from zoneinfo import ZoneInfo

import pytest
from pydantic import ValidationError

from gymclaw.services.planning import Interval, SlotSignal, generate_candidates, plan_week
from gymclaw.services.profile import Profile

ZONE = ZoneInfo("Europe/Berlin")
WEEK = date(2026, 10, 12)
NOW = datetime(2026, 10, 11, tzinfo=ZONE)


def instant(day=12, hour=19, minute=0):
    return datetime(2026, 10, day, hour, minute, tzinfo=ZONE)


def test_target_recovery_and_determinism():
    profile = Profile()
    plan = plan_week(profile, WEEK, now=NOW)
    assert plan == plan_week(profile, WEEK, now=NOW)
    assert [c.start.astimezone(ZONE).weekday() for c in plan.sessions] == [0, 2, 4]
    assert len(plan.sessions) == 3 and plan.minimum_met and plan.shortfall == 0
    for c in plan.sessions:
        assert c.crowd is None and c.confidence == 0
        assert c.start - c.prep_start == timedelta(minutes=35)
        assert c.home_at - c.end == timedelta(minutes=20)


def test_travel_entire_week():
    travel = Interval(start=instant(12, 0), end=instant(19, 0))
    plan = plan_week(Profile(), WEEK, unavailable=(travel,), now=NOW)
    assert not plan.sessions and not plan.minimum_met and plan.shortfall == 3


@pytest.mark.parametrize("start,end", [(18, 19), (20, 21)])
def test_full_block_rejects_prep_and_home_overlap(start, end):
    profile = Profile(earliest_workout_start="19:00", latest_workout_finish="20:10", weekdays_allowed=[0], minimum_workout_minutes=70)
    blocker = Interval(start=instant(hour=start, minute=40 if start == 18 else 20), end=instant(hour=end))
    assert not generate_candidates(profile, WEEK, busy=(blocker,), now=NOW)


def test_touching_boundaries_allowed():
    p = Profile(earliest_workout_start="19:00", latest_workout_finish="20:10", weekdays_allowed=[0], minimum_workout_minutes=70)
    busy = (Interval(start=instant(hour=18), end=instant(hour=18, minute=25)), Interval(start=instant(hour=20, minute=30), end=instant(hour=21)))
    assert len(generate_candidates(p, WEEK, busy=busy, now=NOW)) == 1


def test_cross_week_recovery_and_weekly_max():
    previous = Interval(start=instant(11), end=instant(11, 20))
    plan = plan_week(Profile(), WEEK, existing=(previous,), now=NOW)
    assert len(plan.sessions) == 2
    assert all(c.start.astimezone(ZONE).weekday() != 0 for c in plan.sessions)
    existing = tuple(Interval(start=c.start, end=c.end) for c in plan.sessions)
    full = plan_week(Profile(weekly_min_sessions=1, weekly_target_sessions=2, weekly_max_sessions=2), WEEK, existing=existing, now=NOW)
    assert not full.sessions and full.existing_count == 2


def test_global_search_beats_greedy_high_scoring_tuesday():
    signal = SlotSignal(start=instant(13), crowd=0, confidence=1, adherence=1)
    plan = plan_week(Profile(), WEEK, signals=(signal,), now=NOW)
    assert [c.start.astimezone(ZONE).weekday() for c in plan.sessions] == [0, 2, 4]


def test_score_selects_quiet_slot():
    p = Profile(weekly_min_sessions=1, weekly_target_sessions=1, weekly_max_sessions=1)
    signal = SlotSignal(start=instant(14), crowd=0.2, confidence=0.9)
    plan = plan_week(p, WEEK, signals=(signal,), now=NOW)
    assert plan.sessions[0].start == signal.start
    assert plan.sessions[0].score == pytest.approx(0.75)


def test_shortened_workout_when_only_minimum_fits():
    p = Profile(earliest_workout_start="19:00", latest_workout_finish="19:45", weekdays_allowed=[0])
    plan = plan_week(p, WEEK, now=NOW)
    assert len(plan.sessions) == 1
    assert plan.sessions[0].end - plan.sessions[0].start == timedelta(minutes=45)
    assert not plan.minimum_met


def test_no_past_preparation():
    candidates = generate_candidates(Profile(), WEEK, now=instant(hour=19))
    assert all(c.prep_start >= instant(hour=19) for c in candidates)


@pytest.mark.parametrize("changes", [{"weekly_target_sessions": 1}, {"minimum_workout_minutes": 80}, {"timezone": "bad"}, {"weekdays_allowed": [7]}, {"weekdays_allowed": [0, 0]}, {"prep_minutes": -1}, {"crowd_preference_weight": float("nan")}, {"latest_workout_finish": "06:00"}])
def test_invalid_profile(changes):
    with pytest.raises(ValidationError):
        Profile(**changes)


def test_invalid_inputs():
    with pytest.raises(ValueError, match="Monday"):
        plan_week(Profile(), date(2026, 10, 13), now=NOW)
    with pytest.raises(ValueError, match="timezone"):
        plan_week(Profile(), WEEK, now=datetime(2026, 10, 11))
    with pytest.raises(ValidationError):
        Interval(start=instant(), end=instant())


@pytest.mark.parametrize("week", [date(2026, 3, 23), date(2026, 10, 19)])
def test_dst_elapsed_duration(week):
    p = Profile(weekdays_allowed=[6], earliest_workout_start="01:00", latest_workout_finish="05:00")
    candidates = generate_candidates(p, week, now=datetime.combine(week, datetime.min.time(), ZONE))
    assert candidates
    for c in candidates:
        assert c.end - c.start in (timedelta(minutes=70), timedelta(minutes=45))
        local = c.start.astimezone(ZONE)
        assert local.astimezone(timezone.utc) == c.start
        assert c.end.astimezone(ZONE).hour <= 5


def test_habit_breaks_ties_but_a_quiet_slot_beats_a_busy_habitual_one():
    p = Profile(weekly_min_sessions=1, weekly_target_sessions=1, weekly_max_sessions=1, weekdays_allowed=[0])
    busy_habit = SlotSignal(start=instant(12, 13), crowd=2 / 3, confidence=0.9, preferred_time=0.9)
    quiet = SlotSignal(start=instant(12, 16), crowd=1 / 3, confidence=0.9, preferred_time=0.2)
    assert plan_week(p, WEEK, signals=(busy_habit, quiet), now=NOW).sessions[0].start == quiet.start
    same_crowd = quiet.model_copy(update={"crowd": 2 / 3})
    assert plan_week(p, WEEK, signals=(busy_habit, same_crowd), now=NOW).sessions[0].start == busy_habit.start
