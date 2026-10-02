"""Domain-backed video scenario. Synthetic inputs, simulated Telegram/time, isolated DB."""
from datetime import date, datetime, timedelta, timezone
import json
from tempfile import TemporaryDirectory
from zoneinfo import ZoneInfo

from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.demo import demo_templates
from gymclaw.models import CrowdFeedback, PlannedSession, WorkoutSession
from gymclaw.providers.crowd import CrowdReading, FixtureCrowdProvider
from gymclaw.services import adaptation, audit, crowd, notifications, workout
from gymclaw.services.compression import compress_template
from gymclaw.services.planning import Interval
from gymclaw.services.profile import update_profile
from gymclaw.services.scheduling import PlanningFixture, schedule_week
from gymclaw.services.set_parser import SetInput
from gymclaw.services.templates import import_template


def build_story() -> dict:
    zone = ZoneInfo("Europe/Berlin")
    sunday = datetime(2026, 10, 11, 7, 15, tzinfo=zone)
    with TemporaryDirectory(prefix="gymclaw-video-") as tmp:
        engine = make_engine(f"sqlite:///{tmp}/story.sqlite")
        try:
            initialize(engine)
            with Session(engine) as db, db.begin():
                profile = update_profile(db, {"earliest_workout_start": "18:00", "latest_workout_finish": "22:00"})
                templates = demo_templates()
                for template in templates:
                    import_template(db, template)
                # Labelled fixture history demonstrates calibration, not owner's history or accuracy.
                for index, count in reversed(list(enumerate([9, 11, 13, 14, 16, 17, 20, 24]))):
                    at = datetime(2026, 10, 5, 19, 45, tzinfo=zone)-timedelta(weeks=index)
                    crowd.poll(db, FixtureCrowdProvider(CrowdReading(source="GYM_API", metric="reported_active_count", raw_value=count)), now=at)
                    old = WorkoutSession(status="PLAN_UPDATED", template_id=templates[0].id,
                        template_snapshot=templates[0].model_dump(mode="json"), started_at=at, arrived_at=at, completed_at=at+timedelta(minutes=30))
                    db.add(old); db.flush()
                    rating = "FINE" if count < 20 else "BUSY"
                    db.add(CrowdFeedback(workout_session_id=old.id, observed_at=at, rating=rating,
                        normalized_score=crowd.RATINGS[rating], features_json={"demo": True,
                            "recorded_at": at.isoformat(), "GYM_API": {"raw_value": count}}))
                db.flush()
                fixture = PlanningFixture(busy=(Interval(start=datetime(2026,10,12,18,tzinfo=zone), end=datetime(2026,10,12,19,tzinfo=zone)),))
                result = schedule_week(db, date(2026,10,12), now=sunday, fixture=fixture, template_id=templates[0].id, request_id="video-week")
                week = []
                for index, item in enumerate(result["sessions"]):
                    row = db.get(PlannedSession, item["id"]); template = templates[index]
                    row.workout_template_id = template.id
                    row.workout_plan_json = compress_template(template, profile, int((row.planned_end_at-row.planned_start_at).total_seconds()))
                    week.append({"day":row.planned_start_at.astimezone(zone).strftime("%a"), "name":template.name,
                        "time":row.planned_start_at.astimezone(zone).strftime("%H:%M")})
                planned = db.get(PlannedSession, result["sessions"][0]["id"])
                prediction = crowd.CrowdModel(db, now=sunday).predict(planned.planned_start_at)
                estimate = f"{prediction['score']*5:.1f} / 5" if prediction['score'] is not None else "Unknown"
                # Explicit local commitment is a fixture; no Google publication/provider is invoked.
                planned.status = "COMMITTED"
                notifications.schedule_session_jobs(db, planned, now=sunday)
                ready = planned.prep_start_at; arrival = planned.planned_start_at
                notifications.dispatch_due(db, now=ready)
                crowd.poll(db, FixtureCrowdProvider(CrowdReading(source="GYM_API", metric="reported_active_count", raw_value=14)), now=arrival-timedelta(minutes=5))
                started = workout.start(db, templates[0].id, now=arrival, request_id="video-start", planned_session_id=planned.id)["data"]
                wid = started["workout_id"]
                feedback = crowd.record_feedback(db, wid, rating="FINE", now=arrival, request_id="video-arrival")
                warmup = workout.log_set(db, wid, SetInput(weight=50,reps=8,set_type="WARMUP"), now=arrival+timedelta(seconds=35), request_id="video-warmup")["data"]
                first = workout.log_set(db, wid, SetInput(weight=80,reps=9), now=arrival+timedelta(seconds=80), request_id="video-working-1")["data"]
                due = datetime.fromisoformat(first["rest_job"]["due_at"])
                resumed = workout.rest_complete(db, first["rest_job"]["id"], now=due, request_id="video-rest-1")["data"]
                second = workout.log_set(db,wid,SetInput(weight=80,reps=10),now=due+timedelta(seconds=40),request_id="video-working-2")["data"]
                next_due = datetime.fromisoformat(second["rest_job"]["due_at"])
                workout.rest_complete(db,second["rest_job"]["id"],now=next_due,request_id="video-rest-2")
                third = workout.log_set(db,wid,SetInput(weight=80,reps=10),now=next_due+timedelta(seconds=40),request_id="video-working-3")["data"]
                deferred_id = third["active_exercise"]["id"]
                busy = adaptation.machine_busy(db,wid,now=next_due+timedelta(seconds=45),request_id="video-busy")["data"]
                at = next_due+timedelta(seconds=50)
                adaptation.machine_free(db,wid,deferred_id,now=at,request_id="video-free")
                for index in range(30):
                    state = workout.current(db,wid,now=at)
                    if state['status']=="WORKOUT_COMPLETE": break
                    if state['rest_job']:
                        at=datetime.fromisoformat(state['rest_job']['due_at'])
                        workout.rest_complete(db,state['rest_job']['id'],now=at,request_id=f"video-rest-tail-{index}")
                    state=workout.current(db,wid,now=at); active=state['active_exercise']
                    if active is None: raise RuntimeError("Unresolved demo exercise")
                    at+=timedelta(seconds=40)
                    workout.log_set(db,wid,SetInput(weight=active['target_weight'],reps=active['rep_max'],set_type=active['set_type']),now=at,request_id=f"video-tail-{index}")
                finished=audit.finish(db,wid,now=at,request_id="video-finish")["data"]
                return {"demo":True,"simulation":"Telegram UI and time; synthetic counts, feedback and weights",
                    "week":week,"estimate":estimate,"confidence":"low", "prediction_demo":prediction['demo'],
                    "ready_time":ready.astimezone(zone).strftime("%H:%M"),"leave_time":planned.leave_home_at.astimezone(zone).strftime("%H:%M"),
                    "arrival_time":arrival.astimezone(zone).strftime("%H:%M"),"warmup":warmup['active_exercise'],
                    "first":first['active_exercise'],"rest_seconds":int((due-(arrival+timedelta(seconds=80))).total_seconds()),
                    "resumed":resumed['active_exercise'],"next":third['active_exercise'],"busy":busy['active_exercise'],
                    "completed_sets":finished['completed_sets'],"status":workout.current(db,wid,now=at)['status']}
        finally: engine.dispose()


if __name__ == "__main__":
    print(json.dumps(build_story(),allow_nan=False))
