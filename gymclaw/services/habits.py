"""Time-of-day habits learned from what the owner actually does.

Evidence: when the owner arrives at the gym (workouts started with the reminder's ▶️ button; chat starts
may be logged late, so they don't count), where the owner moves sessions to,
and the planned times they move sessions away from. Each point is a Gaussian bump on the clock,
decayed by age. The result is a 0–1 affinity per time of day (0.5 = no opinion), fed to the planner
as `SlotSignal.preferred_time`. Its weight sits below crowd, so a habitual but busy time still loses
to a quiet one; habits break ties and steer away from times the owner keeps moving off.
"""
from datetime import datetime, timedelta
from math import exp
from zoneinfo import ZoneInfo

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import AgentEvent, WorkoutSession

HISTORY = timedelta(days=120)
HALF_LIFE_DAYS = 42
BANDWIDTH_MINUTES = 45
# Pseudo-evidence pulling sparse habits toward "no opinion": one data point can't dominate.
PRIOR_WEIGHT = 3

Point = tuple[float, float]  # (minute of day, weight); negative weight = moved away from


def evidence(db: Session, *, now: datetime, zone: ZoneInfo) -> list[Point]:
    since = now - HISTORY

    def point(at: datetime, sign: float) -> Point:
        local = at.astimezone(zone)
        return local.hour * 60 + local.minute, sign * 0.5 ** ((now - at).total_seconds() / 86400 / HALF_LIFE_DAYS)

    points = [point(w.arrived_at, 1) for w in db.scalars(select(WorkoutSession).where(WorkoutSession.started_by_button, WorkoutSession.arrived_at >= since))]
    # One owner decision per session: the slot it was planned at first is disliked, the last explicit
    # pick is liked. A chain of moves (10:30 → 14:30 → 11:45) only counts its ends.
    moves: dict[str, tuple[dict, dict | None]] = {}
    for event in db.scalars(select(AgentEvent).where(AgentEvent.type == "planning.session_moved", AgentEvent.created_at >= since).order_by(AgentEvent.created_at)):
        data = event.payload_json.get("result", {}).get("data", {})
        if "session_id" not in data:
            continue
        explicit = data if event.payload_json["request"]["intent"].get("to") else None
        first, last = moves.get(data["session_id"], (data, None))
        moves[data["session_id"]] = (first, explicit or last)
    for first, last in moves.values():
        points.append(point(datetime.fromisoformat(first["from"]), -1))
        if last:
            points.append(point(datetime.fromisoformat(last["to"]), 1))
    return points


def affinity(points: list[Point], minute: float) -> float:
    """0–1 liking of a time of day; 0.5 without evidence."""
    def bump(other: float) -> float:
        distance = min(abs(minute - other), 1440 - abs(minute - other))
        return exp(-0.5 * (distance / BANDWIDTH_MINUTES) ** 2)

    net = sum(w * bump(m) for m, w in points) / (sum(abs(w) for _, w in points) + PRIOR_WEIGHT)
    return 0.5 + 0.5 * net
