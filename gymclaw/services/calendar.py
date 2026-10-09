"""Incremental sync and user-edit reconciliation. Sync never performs remote writes."""
from datetime import date, datetime, timedelta
from math import ceil
from zoneinfo import ZoneInfo

from sqlalchemy import func, select
from sqlalchemy.orm import Session

from gymclaw.models import CalendarEventSnapshot, CalendarSyncState, CalendarWrite, PlannedSession
from gymclaw.providers.calendar import CalendarEvent, CalendarProvider, SyncTokenExpired
from gymclaw.providers.google_auth import bind_calendar
from gymclaw.services.compression import compress_template
from gymclaw.services.errors import DomainError
from gymclaw.services.notifications import cancel_session_jobs, schedule_session_jobs
from gymclaw.services.profile import get_profile
from gymclaw.services.scheduling import session_data
from gymclaw.services.templates import Template, get_template
from gymclaw.services.workout import emit, historical_set_duration, utc


def week_of(stamp: datetime, zone: ZoneInfo) -> date:
    day = stamp.astimezone(zone).date()
    return day - timedelta(days=day.weekday())


def persist_snapshot(db: Session, event: CalendarEvent, *, now: datetime, locked: bool = False):
    row = db.get(CalendarEventSnapshot, event.id)
    if row is None:
        row = CalendarEventSnapshot(calendar_event_id=event.id, start_at=event.start or now, end_at=event.end or now)
        db.add(row)
    row.ical_uid = event.ical_uid or row.ical_uid
    row.etag = event.etag
    row.title = event.title or row.title or ""
    row.start_at = event.start or row.start_at
    row.end_at = event.end or row.end_at
    row.status = event.status
    row.updated_at_remote = event.updated
    # Minimal deletion payloads omit ownership. Keep provenance for tombstones.
    if event.status != "cancelled":
        row.gymclaw_managed = event.managed
        row.gymclaw_session_id = event.session_id
        row.gymclaw_plan_revision = event.revision
    elif row.gymclaw_managed is None:
        row.gymclaw_managed = event.managed
    row.user_locked = row.user_locked or locked
    row.last_seen_at = now
    row.raw_json = (row.raw_json or {}) | event.raw if event.status == "cancelled" else event.raw
    db.flush()
    return row


def allocation(db: Session, session: PlannedSession):
    if not session.workout_template_id:
        return
    snapshot = session.workout_plan_json.get("template")
    template = Template.model_validate(snapshot) if snapshot else get_template(db, session.workout_template_id)
    template = template.model_copy(update={"set_duration_seconds": ceil(historical_set_duration(db, template.set_duration_seconds))})
    budget = int((session.planned_end_at - session.planned_start_at).total_seconds())
    crowd_metadata = {k: v for k, v in session.workout_plan_json.items() if k.startswith("crowd_")}
    try:
        session.workout_plan_json = compress_template(template, get_profile(db), budget)
    except DomainError as error:
        session.workout_plan_json = {"template": template.model_dump(mode="json"), "budget_seconds": budget, "error": error.code}
    session.workout_plan_json |= crowd_metadata


def reconcile_event(db: Session, event: CalendarEvent, *, now: datetime) -> tuple[set[date], list[dict]]:
    profile = get_profile(db)
    zone = ZoneInfo(profile.timezone)
    previous = db.get(CalendarEventSnapshot, event.id)
    session = db.scalar(select(PlannedSession).where(PlannedSession.calendar_event_id == event.id))
    if session is None and event.managed and event.session_id:
        candidate = db.get(PlannedSession, event.session_id)
        if candidate and candidate.calendar_event_id is None:
            from gymclaw.services.calendar_writes import event_id_for
            if event.id == event_id_for(candidate.id):
                session = candidate
                session.calendar_event_id = event.id
    changed_weeks = set()
    emitted = []
    manual_update = False
    if session:
        old_week = week_of(session.planned_start_at, zone)
        if event.status == "cancelled":
            if session.status not in {"CANCELLED", "SKIPPED", "MISSED", "COMPLETED"}:
                session.status = "CANCELLED"
                session.user_locked = True
                session.workout_plan_json = session.workout_plan_json | {"deleted_by_user": True}
                session.source_revision += 1
                cancel_session_jobs(db, session.id, now=now)
                for write in db.scalars(select(CalendarWrite).where(CalendarWrite.planned_session_id == session.id, CalendarWrite.status == "PENDING")):
                    write.status = "CANCELLED"
                    write.handled_at = now
                event_log = emit(db, "calendar.gym_event_deleted", now, {"session_id": session.id, "event_id": event.id})
                emitted.append({"id": event_log.id, "type": event_log.type})
                changed_weeks.add(old_week)
        else:
            # An own-write echo matches revision + proposed times. Never interpret it as intent.
            own_echo = event.revision == session.source_revision and event.start == session.planned_start_at and event.end == session.planned_end_at
            times_changed = (previous.start_at != event.start or previous.end_at != event.end) if previous else (session.planned_start_at != event.start or session.planned_end_at != event.end)
            if times_changed and not own_echo and session.status not in {"CANCELLED", "SKIPPED", "MISSED", "COMPLETED"}:
                old_duration = session.planned_end_at - session.planned_start_at
                if event.start != session.planned_start_at:
                    # Old-slot forecast is not evidence about user's newly chosen slot.
                    session.crowd_prediction, session.crowd_confidence = None, 0
                session.planned_start_at, session.planned_end_at = event.start, event.end
                session.prep_start_at = event.start - timedelta(minutes=profile.prep_minutes + profile.commute_to_gym_minutes)
                session.leave_home_at = event.start - timedelta(minutes=profile.commute_to_gym_minutes)
                session.expected_finish_at = event.end
                session.week_id = week_of(event.start, zone).isoformat()
                session.user_locked = True
                manual_update = True
                session.status = "COMMITTED" if session.status != "STARTED" else "STARTED"
                session.source_revision += 1
                allocation(db, session)
                cancel_session_jobs(db, session.id, now=now)
                for write in db.scalars(select(CalendarWrite).where(CalendarWrite.planned_session_id == session.id, CalendarWrite.status == "PENDING")):
                    write.status = "CANCELLED"
                    write.handled_at = now
                schedule_session_jobs(db, session, now=now)
                kind = "calendar.gym_event_resized" if old_duration != event.end - event.start else "calendar.gym_event_changed"
                log = emit(db, kind, now, {"session": session_data(session), "event_id": event.id})
                emitted.append({"id": log.id, "type": kind, "session_id": session.id})
                changed_weeks |= {old_week, week_of(event.start, zone)}
    elif event.status != "cancelled":
        meaningful = previous is None or previous.start_at != event.start or previous.end_at != event.end or previous.status == "cancelled" or previous.raw_json.get("recurrence") != event.raw.get("recurrence") or previous.raw_json.get("transparency") != event.raw.get("transparency")
        if meaningful and (event.raw.get("transparency") != "transparent" or previous and previous.raw_json.get("transparency") != "transparent"):
            kind = "calendar.blocker_removed" if event.raw.get("transparency") == "transparent" else "calendar.blocker_added"
            log = emit(db, kind, now, {"event_id": event.id})
            emitted.append({"id": log.id, "type": log.type})
            changed_weeks |= {week_of(now, zone), week_of(now + timedelta(days=7), zone)}
    elif previous and previous.status != "cancelled" or previous is None and event.raw.get("recurringEventId"):
        log = emit(db, "calendar.blocker_removed", now, {"event_id": event.id})
        emitted.append({"id": log.id, "type": log.type})
        changed_weeks |= {week_of(now, zone), week_of(now + timedelta(days=7), zone)}
    persist_snapshot(db, event, now=now, locked=session.user_locked if session else False)
    if manual_update:
        from gymclaw.services.calendar_writes import queue_session_write
        queue_session_write(db, session, now=now, metadata_only=True)
    return changed_weeks, emitted


def sync_calendar(db: Session, provider: CalendarProvider, *, now: datetime) -> dict:
    now = utc(now)
    state = bind_calendar(db, provider.calendar_id, provider.source)
    if state.last_synced_at and now < state.last_synced_at:
        raise DomainError("TIME_REVERSED", "Sync timestamp precedes last calendar sync")
    # Fetch every page before mutating cached events or plans. Failed fetch leaves state intact.
    try:
        batch = provider.incremental_sync(state.sync_token)
    except SyncTokenExpired:
        batch = provider.incremental_sync(None)
    state.timezone = batch.timezone
    incoming = list(batch.events)
    if batch.full:
        ids = {e.id for e in incoming}
        for snapshot in db.scalars(select(CalendarEventSnapshot).where(CalendarEventSnapshot.status != "cancelled")):
            if snapshot.calendar_event_id not in ids:
                incoming.append(CalendarEvent(id=snapshot.calendar_event_id, status="cancelled", raw={"id": snapshot.calendar_event_id, "status": "cancelled"}))
    affected, emitted = set(), []
    # User changes all reconcile before recovery/replanning, so batch order is immaterial.
    for event in incoming:
        weeks, logs = reconcile_event(db, event, now=now)
        affected |= weeks
        emitted.extend(logs)
    state.sync_token = batch.next_sync_token
    state.last_synced_at = now
    from gymclaw.services.replanning import replan_weeks, commit_upcoming
    replanned = replan_weeks(db, affected, now=now) if affected else {"changed": [], "warnings": []}
    commit_upcoming(db, now=now)
    db.flush()
    meaningful = emitted or replanned["changed"] or replanned["warnings"]
    if meaningful:
        event = emit(db, "planning.replan_required", now, {"reason": "calendar_changed", "handled_locally": True, "changes": replanned["changed"], "warnings": replanned["warnings"]})
        # Deterministic replan already ran; agent should communicate, not duplicate work.
        event.handled_at = now
        emitted.append({"id": event.id, "type": event.type})
    zone = ZoneInfo(get_profile(db).timezone)
    # Name what happened: a vague "sessions replanned" hides that a workout was dropped.
    parts = []
    for e in emitted:
        if e["type"] in {"calendar.gym_event_changed", "calendar.gym_event_resized"}:
            session = db.get(PlannedSession, e["session_id"])
            start, end = session.planned_start_at.astimezone(zone), session.planned_end_at.astimezone(zone)
            parts.append(f"{workout_name(session)} moved to {start:%a %H:%M}." if e["type"] == "calendar.gym_event_changed" else f"{workout_name(session)} now {start:%a %H:%M}–{end:%H:%M}.")
    dropped = [db.get(PlannedSession, c["session_id"]) for c in replanned["changed"] if c["action"] == "cancelled_no_valid_slot"]
    if dropped:
        names = ", ".join(f"{workout_name(s)} on {s.planned_start_at.astimezone(zone):%a}" for s in dropped)
        parts.append(f"{names} dropped: no slot left this week that fits your rest days and calendar.")
    # The drop line already explains the shortfall warning.
    parts += [w for w in replanned["warnings"] if not (dropped and "sessions feasible" in w)]
    if any(e["type"] == "calendar.gym_event_resized" for e in emitted):
        parts.append("Workout compressed to the new slot length.")
    if not parts and replanned["changed"]:
        parts.append("Other gym sessions replanned; preparation/leave times updated.")
    hint = " ".join(parts) or None
    return {"data": {"calendar_id": provider.calendar_id, "source": provider.source, "full_sync": batch.full, "events_received": len(batch.events), "changes": replanned["changed"], "warnings": replanned["warnings"], "weeks": sorted(w.isoformat() for w in affected), "calendar_writes_queued": db.scalar(select(func.count()).select_from(CalendarWrite).where(CalendarWrite.status == "PENDING")), "remote_events_changed": False}, "events": emitted, "user_message_hint": hint}


def auth_alert(db: Session, *, ok: bool, now: datetime) -> dict | None:
    """One owner message when Google Calendar access is lost, one when it's back. Never repeats."""
    from gymclaw.services.events import outage_open
    lost = outage_open(db, "calendar.auth_lost", "calendar.auth_restored")
    if not ok and not lost:
        event = emit(db, "calendar.auth_lost", now, {})
        return {"event_id": event.id, "message": "⚠️ Lost access to Google Calendar, so I can't see calendar edits or update events. "
            "Moves via chat still work. Reconnect with `calendar auth` on the server."}
    if ok and lost:
        event = emit(db, "calendar.auth_restored", now, {})
        return {"event_id": event.id, "message": "✅ Google Calendar is connected again."}
    return None


def workout_name(session: PlannedSession) -> str:
    return session.workout_plan_json.get("template", {}).get("name") or (session.workout_template_id or "Workout").title()


def week_lineup(db: Session, week_start: date, *, now: datetime) -> str:
    """The week's planned workouts for owner messages, e.g. "This week: Mon 12:15 Legs · Wed 10:30 Push".
    Build it after rotation so names match the calendar."""
    zone = ZoneInfo(get_profile(db).timezone)
    sessions = [s for s in db.scalars(select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED", "STARTED", "COMPLETED"])).order_by(PlannedSession.planned_start_at))
        if week_of(s.planned_start_at, zone) == week_start]
    label = "This week" if week_start == week_of(now, zone) else f"Week of {week_start:%b} {week_start.day}"
    return f"{label}: " + (" · ".join(f"{s.planned_start_at.astimezone(zone):%a %H:%M} {workout_name(s)}" for s in sessions) or "no workouts") + "."


def get_week(db: Session, week_start: date) -> dict:
    if week_start.weekday() != 0:
        raise ValueError("week_start must be Monday")
    zone = ZoneInfo(get_profile(db).timezone)
    sessions = [session_data(s) for s in db.scalars(select(PlannedSession).order_by(PlannedSession.planned_start_at)) if week_of(s.planned_start_at, zone) == week_start]
    return {"week_start": week_start.isoformat(), "sessions": sessions}
