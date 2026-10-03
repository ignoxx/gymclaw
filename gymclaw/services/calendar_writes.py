"""Durable calendar write outbox. Only owned events; explicit live-write opt-in."""
from datetime import datetime, timedelta
from hashlib import sha256
from zoneinfo import ZoneInfo

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import CalendarEventSnapshot, CalendarWrite, PlannedSession
from gymclaw.providers.calendar import CalendarConflict, CalendarEvent, CalendarProvider
from gymclaw.providers.google_auth import bind_calendar
from gymclaw.services.calendar import persist_snapshot, reconcile_event
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import get_profile
from gymclaw.services.workout import emit, utc


def event_id_for(session_id: str) -> str:
    # Hex is valid Google base32hex ID alphabet. Stable across retries/restarts.
    return sha256(("gymclaw:" + session_id).encode()).hexdigest()


def describe_crowd(forecast: dict | None, start: datetime) -> str:
    """'Busy · ~13 people checked in (Fridays 18:00, 2 wk)'; honest when there is no data yet."""
    if not forecast:
        return "not enough data yet"
    parts = [forecast["feel"]] if forecast.get("feel") else []
    if forecast.get("count") is not None:
        basis = f"{start:%A}s {start:%H}:00, {forecast['days']} wk" if forecast["basis"] == "weekday_hour" else f"{start:%H}:00 on other days"
        parts.append(f"~{forecast['count']} people checked in ({basis})")
    return " · ".join(parts)


# Google Calendar event colours (peacock, tangerine, basil, grape, banana, tomato, blueberry),
# one per template in the owner's split so Push/Pull/Legs are recognisable at a glance.
TEMPLATE_COLORS = ("7", "6", "10", "3", "5", "11", "9")


def event_body(db: Session, session: PlannedSession) -> dict:
    """Everything the owner needs without asking: plan, crowd, timings, gym location (calendar apps
    use it for travel time / time to leave) and a colour per workout type."""
    from gymclaw.models import OnboardingState
    from gymclaw.services.preview import plan_lines
    profile = get_profile(db)
    zone = ZoneInfo(profile.timezone)
    template_name = session.workout_plan_json.get("template", {}).get("name", "Workout")
    crowd = describe_crowd(session.workout_plan_json.get("crowd_forecast"), session.planned_start_at.astimezone(zone))
    if session.workout_plan_json.get("crowd_source") == "demo_fixture":
        crowd += " (demo fixture)"
    home = session.expected_finish_at + timedelta(minutes=profile.commute_home_minutes)
    lines = [f"Expected crowd: {crowd}",
        f"Get ready {session.prep_start_at.astimezone(zone):%H:%M} · Leave {session.leave_home_at.astimezone(zone):%H:%M} · Home ~{home.astimezone(zone):%H:%M}"]
    if session.workout_template_id:
        lines += ["", "Plan:", *plan_lines(db, session)]
    lines += ["", "Managed by GymClaw. Move or resize freely; it adapts."]
    onboarding = db.get(OnboardingState, 1)
    split = onboarding.split_json if onboarding else []
    color = TEMPLATE_COLORS[split.index(session.workout_template_id) % len(TEMPLATE_COLORS)] if session.workout_template_id in split else None
    return {
        "id": session.calendar_event_id or event_id_for(session.id),
        "summary": f"🏋️ {template_name}",
        "description": "\n".join(lines),
        **({"location": profile.gym_address} if profile.gym_address else {}),
        **({"colorId": color} if color else {}),
        "start": {"dateTime": session.planned_start_at.astimezone(zone).isoformat(), "timeZone": profile.timezone},
        "end": {"dateTime": session.planned_end_at.astimezone(zone).isoformat(), "timeZone": profile.timezone},
        "status": "confirmed" if session.status == "COMMITTED" else "tentative",
        "extendedProperties": {"private": {"gymclawManaged": "true", "gymclawSessionId": session.id, "gymclawPlanRevision": str(session.source_revision)}},
        "reminders": {"useDefault": False},
    }


def queue_session_write(db: Session, session: PlannedSession, *, now: datetime, metadata_only: bool = False) -> CalendarWrite | None:
    if session.user_locked and not metadata_only or session.status in {"STARTED", "COMPLETED", "SKIPPED", "MISSED"}:
        return None
    action = "DELETE" if session.status == "CANCELLED" else "UPDATE" if session.calendar_event_id else "CREATE"
    if action == "DELETE" and not session.calendar_event_id:
        for old in db.scalars(select(CalendarWrite).where(CalendarWrite.planned_session_id == session.id, CalendarWrite.status == "PENDING")):
            old.status, old.handled_at = "CANCELLED", now
        return None
    snapshot = db.get(CalendarEventSnapshot, session.calendar_event_id) if session.calendar_event_id else None
    if action != "CREATE" and (snapshot is None or not snapshot.etag or not snapshot.gymclaw_managed or snapshot.gymclaw_session_id != session.id):
        emit(db, "calendar.write_blocked", now, {"session_id": session.id, "reason": "ownership_or_etag_missing"})
        return None
    body = event_body(db, session) if action != "DELETE" else {}
    if metadata_only:
        body = {k: body[k] for k in ("summary", "description", "location", "colorId", "extendedProperties") if k in body}
    existing = db.scalar(select(CalendarWrite).where(CalendarWrite.planned_session_id == session.id, CalendarWrite.revision == session.source_revision, CalendarWrite.action == action))
    if existing:
        return existing
    for old in db.scalars(select(CalendarWrite).where(CalendarWrite.planned_session_id == session.id, CalendarWrite.status == "PENDING")):
        old.status = "CANCELLED"
        old.handled_at = now
    write = CalendarWrite(planned_session_id=session.id, revision=session.source_revision, action=action, event_id=session.calendar_event_id or event_id_for(session.id), expected_etag=snapshot.etag if snapshot else None, body_json=body, created_at=now)
    db.add(write)
    db.flush()
    emit(db, "calendar.write_queued", now, {"write_id": write.id, "session_id": session.id, "action": action})
    return write


def pending_writes(db: Session) -> list[dict]:
    return [{"id": w.id, "session_id": w.planned_session_id, "action": w.action, "event_id": w.event_id, "revision": w.revision, "body": w.body_json} for w in db.scalars(select(CalendarWrite).where(CalendarWrite.status == "PENDING").order_by(CalendarWrite.created_at, CalendarWrite.id))]


def apply_write(db: Session, provider: CalendarProvider, write_id: str, *, now: datetime, allow_writes: bool = False) -> dict:
    now = utc(now)
    if provider.source == "google" and not allow_writes:
        raise DomainError("CALENDAR_WRITES_NOT_APPROVED", "Live writes require explicit --allow-writes; inspect pending-writes first")
    bind_calendar(db, provider.calendar_id, provider.source)
    write = db.get(CalendarWrite, write_id)
    if write is None:
        raise DomainError("CALENDAR_WRITE_NOT_FOUND", "Unknown queued calendar write")
    if write.status != "PENDING":
        return {"write_id": write.id, "status": write.status, "retried": True}
    session = db.get(PlannedSession, write.planned_session_id)
    metadata_only = write.action == "UPDATE" and not {"start", "end"} & write.body_json.keys()
    if session.user_locked and not metadata_only or write.revision != session.source_revision:
        write.status = "CANCELLED"
        write.handled_at = now
        return {"write_id": write.id, "status": write.status}
    try:
        if write.action == "CREATE":
            remote = provider.create_gym_event(write.body_json)
        elif write.action == "UPDATE":
            remote = provider.update_gym_event(write.event_id, write.body_json, etag=write.expected_etag)
        else:
            provider.delete_gym_event(write.event_id, etag=write.expected_etag, session_id=session.id)
            remote = CalendarEvent(id=write.event_id, status="cancelled", raw={"id": write.event_id, "status": "cancelled"})
        if write.action != "DELETE":
            if not remote.managed or remote.session_id != session.id or remote.id != write.event_id:
                raise DomainError("CALENDAR_NOT_OWNED", "Provider returned unexpected event ownership; sync required")
            session.calendar_event_id = remote.id
            # Retry may find an event created previously and then manually edited.
            if remote.start != session.planned_start_at or remote.end != session.planned_end_at:
                affected, _ = reconcile_event(db, remote, now=now)
                from gymclaw.services.replanning import replan_weeks
                replan_weeks(db, affected, now=now)
            else:
                persist_snapshot(db, remote, now=now)
                if write.action == "CREATE" and remote.revision != session.source_revision:
                    queue_session_write(db, session, now=now)
        else:
            persist_snapshot(db, remote, now=now)
        write.status = "APPLIED"
        write.handled_at = now
        event = emit(db, "calendar.write_applied", now, {"write_id": write.id, "session_id": session.id, "action": write.action, "event_id": write.event_id})
        return {"write_id": write.id, "status": write.status, "event": {"id": event.id, "type": event.type}}
    except (CalendarConflict, DomainError) as error:
        if error.code not in {"CALENDAR_WRITE_CONFLICT", "CALENDAR_NOT_OWNED", "CALENDAR_EVENT_DELETED"}:
            raise
        write.status = "CONFLICT"
        write.handled_at = now
        event = emit(db, "calendar.sync_required", now, {"write_id": write.id, "reason": error.code})
        return {"write_id": write.id, "status": write.status, "error": error.code, "event": {"id": event.id, "type": event.type}}
