"""Offline demo: real domain services in a disposable DB, rendered by the same dashboard projection."""
from datetime import date, datetime, timedelta, timezone
from tempfile import TemporaryDirectory
from zoneinfo import ZoneInfo

from sqlalchemy.orm import Session

from gymclaw.dashboard_state import snapshot
from gymclaw.db import initialize, make_engine
from gymclaw.models import OnboardingState, PlannedSession
from gymclaw.providers.crowd import CrowdReading, FixtureCrowdProvider
from gymclaw.services import audit, crowd, workout
from gymclaw.services.planning import Interval
from gymclaw.services.profile import update_profile
from gymclaw.services.scheduling import PlanningFixture, schedule_week
from gymclaw.services.set_parser import SetInput
from gymclaw.services.templates import Template, import_template
from gymclaw.services.weekly import assign_rotation, refresh_crowd

# Typical evening check-ins at a mid-size gym, one value per 15 minutes from 16:00.
EVENING = [6, 7, 9, 11, 13, 15, 18, 21, 23, 22, 19, 17, 14, 12, 10, 8]


def demo_templates() -> tuple[Template, ...]:
    """PPL examples, never owner plans or weights."""
    return (
        Template(id="demo-push", name="Push", source="demo", exercises=[
            {"id": "bench", "name": "Bench press", "role": "press", "guide_id": "bench-press", "primary": True, "working_sets": 3, "target_weight": 80, "warmup_weight": 50, "rest_seconds": 90},
            {"id": "overhead", "name": "Overhead press", "role": "press", "guide_id": "overhead-press", "target_weight": 30},
            {"id": "triceps", "name": "Tricep pushdown", "role": "accessory", "guide_id": "tricep-pushdown", "working_sets": 2, "target_weight": 25, "rep_min": 12, "rep_max": 15}]),
        Template(id="demo-pull", name="Pull", source="demo", exercises=[
            {"id": "row", "name": "Seated cable row", "role": "pull", "guide_id": "seated-row", "primary": True, "target_weight": 60, "warmup_weight": 30, "rep_min": 10, "rep_max": 12, "rest_seconds": 90},
            {"id": "pulldown", "name": "Lat pulldown", "role": "pull", "guide_id": "lat-pulldown", "target_weight": 55},
            {"id": "curl", "name": "Bicep curl", "role": "accessory", "guide_id": "bicep-curl", "working_sets": 2, "target_weight": 14}]),
        Template(id="demo-legs", name="Legs", source="demo", exercises=[
            {"id": "leg-press", "name": "Leg press", "role": "legs", "guide_id": "leg-press", "primary": True, "target_weight": 120, "warmup_weight": 60},
            {"id": "leg-curl", "name": "Lying leg curl", "role": "accessory", "guide_id": "lying-leg-curl", "target_weight": 40, "rep_min": 12, "rep_max": 15},
            {"id": "leg-extension", "name": "Leg extension", "role": "accessory", "guide_id": "leg-extension", "target_weight": 45, "rep_min": 12, "rep_max": 15}]),
    )


def demo_history(db: Session, templates: tuple[Template, ...], first_monday: date, weeks: int, zone: ZoneInfo):
    """Weeks of finished workouts (Mon/Wed/Fri rotation, one missed session) with slow progression,
    plus a month of evening check-ins so the insight charts have something real to show."""
    session = 0
    for week in range(weeks):
        for weekday in (0, 2, 4):
            day = first_monday + timedelta(weeks=week, days=weekday)
            for index, count in enumerate(EVENING):
                at = datetime(day.year, day.month, day.day, 16, tzinfo=zone) + timedelta(minutes=15 * index)
                crowd.poll(db, FixtureCrowdProvider(CrowdReading(source="GYM_API", metric="reported_active_count", raw_value=round(count * (0.8 + 0.1 * weekday)))), now=at)
            if (week, weekday) == (2, 4):
                continue  # one missed session keeps the consistency chart honest
            template = templates[session % len(templates)]
            start = datetime(day.year, day.month, day.day, 18, 30, tzinfo=zone)
            result = workout.start(db, template.id, now=start, request_id=f"demo-history-{session}")
            workout_id, clock = result["data"]["workout_id"], start
            while (active := workout.current(db, workout_id, now=clock)["active_exercise"]) is not None:
                clock += timedelta(minutes=2)
                if active["set_type"] == "WARMUP":
                    value = SetInput(weight=active["target_weight"], reps=active["rep_max"], set_type="WARMUP")
                else:
                    value = SetInput(weight=active["target_weight"] or 20, reps=active["rep_max"] if week % 2 else active["rep_min"])
                workout.log_set(db, workout_id, value, now=clock, request_id=f"demo-history-{session}-{clock.isoformat()}")
            audit.finish(db, workout_id, now=clock + timedelta(minutes=3), request_id=f"demo-history-{session}-finish")
            if session % 3 == 1:
                crowd.record_feedback(db, workout_id, rating="BUSY" if weekday == 4 else "FINE", now=clock + timedelta(minutes=4), request_id=f"demo-history-{session}-crowd")
            session += 1


def build_demo_snapshot() -> dict:
    """Never opens the default DB or contacts providers; all inputs are synthetic."""
    zone = ZoneInfo("Europe/Berlin")
    week = date(2026, 10, 12)
    planned_at = datetime(2026, 10, 11, 10, tzinfo=timezone.utc)
    with TemporaryDirectory(prefix="gymclaw-demo-") as tmp:
        engine = make_engine(f"sqlite:///{tmp}/demo.sqlite")
        try:
            initialize(engine)
            with Session(engine) as db, db.begin():
                update_profile(db, {"earliest_workout_start": "18:00", "latest_workout_finish": "22:00"})
                templates = demo_templates()
                for template in templates:
                    import_template(db, template)
                db.add(OnboardingState(id=1, interview_json={}, split_json=[t.id for t in templates]))
                demo_history(db, templates, week - timedelta(weeks=6), 6, zone)
                busy = PlanningFixture(busy=(Interval(start=datetime(2026, 10, 12, 18, tzinfo=zone), end=datetime(2026, 10, 12, 19, tzinfo=zone)),))
                result = schedule_week(db, week, now=planned_at, fixture=busy, request_id="demo-plan", template_id=templates[0].id)
                # Today's readings lead up to the workout; history above shapes the "typical" line.
                arrival = db.get(PlannedSession, result["sessions"][1]["id"]).planned_start_at
                first = arrival.replace(hour=14, minute=0)
                for index, count in enumerate(EVENING):
                    at = first + timedelta(minutes=15 * index)
                    if at > arrival:
                        break
                    crowd.poll(db, FixtureCrowdProvider(CrowdReading(source="GYM_API", metric="reported_active_count", raw_value=round(count * 0.9))), now=at)
                assign_rotation(db, now=planned_at)
                refresh_crowd(db, now=planned_at)
                pull = db.get(PlannedSession, result["sessions"][1]["id"])
                started = workout.start(db, pull.workout_template_id, now=arrival, request_id="demo-start", planned_session_id=pull.id)
                workout_id = started["data"]["workout_id"]
                workout.log_set(db, workout_id, SetInput(weight=30, reps=8, set_type="WARMUP"), now=arrival + timedelta(seconds=35), request_id="demo-warmup")
                workout.log_set(db, workout_id, SetInput(weight=60, reps=11), now=arrival + timedelta(seconds=80), request_id="demo-set")
                return snapshot(db, now=arrival + timedelta(seconds=110), demo=True)
        finally:
            engine.dispose()
