"""SQLite schema. Timestamps stored as UTC, returned timezone-aware."""
from datetime import datetime, timezone
from uuid import uuid4

from sqlalchemy import JSON, ForeignKey, String, Text, CheckConstraint, UniqueConstraint
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column
from sqlalchemy.types import TypeDecorator, DateTime


class UTCDateTime(TypeDecorator[datetime]):
    impl = DateTime
    cache_ok = True

    def process_bind_param(self, value, dialect):
        if value is None:
            return None
        if value.tzinfo is None or value.utcoffset() is None:
            raise ValueError("Timestamp must include timezone")
        return value.astimezone(timezone.utc).replace(tzinfo=None)

    def process_result_value(self, value, dialect):
        return value.replace(tzinfo=timezone.utc) if value else None


def utcnow():
    return datetime.now(timezone.utc)


def new_id():
    return str(uuid4())


class Base(DeclarativeBase):
    pass


class UserProfile(Base):
    __tablename__ = "user_profile"
    __table_args__ = (CheckConstraint("id = 1"), CheckConstraint("weekly_min_sessions >= 0 AND weekly_min_sessions <= weekly_target_sessions AND weekly_target_sessions <= weekly_max_sessions"))
    id: Mapped[int] = mapped_column(primary_key=True, default=1)
    timezone: Mapped[str] = mapped_column(default="Europe/Berlin")
    weekly_min_sessions: Mapped[int] = mapped_column(default=2)
    weekly_target_sessions: Mapped[int] = mapped_column(default=3)
    weekly_max_sessions: Mapped[int] = mapped_column(default=3)
    weekdays_allowed: Mapped[list[int]] = mapped_column(JSON, default=lambda: [0, 1, 2, 3, 4])
    minimum_full_rest_days_between_sessions: Mapped[int] = mapped_column(default=1)
    earliest_workout_start: Mapped[str] = mapped_column(default="07:00")
    latest_workout_finish: Mapped[str] = mapped_column(default="22:00")
    preferred_workout_minutes: Mapped[int] = mapped_column(default=70)
    minimum_workout_minutes: Mapped[int] = mapped_column(default=45)
    prep_minutes: Mapped[int] = mapped_column(default=15)
    commute_to_gym_minutes: Mapped[int] = mapped_column(default=20)
    commute_home_minutes: Mapped[int] = mapped_column(default=20)
    warmup_policy: Mapped[str] = mapped_column(default="first_primary_exercise_one_warmup_set")
    default_compound_rest_seconds: Mapped[int] = mapped_column(default=150)
    default_accessory_rest_seconds: Mapped[int] = mapped_column(default=90)
    crowd_preference_weight: Mapped[float] = mapped_column(default=1.0)
    calendar_preference_weight: Mapped[float] = mapped_column(default=1.0)
    recovery_weight: Mapped[float] = mapped_column(default=1.0)
    created_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow)
    updated_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow, onupdate=utcnow)


class LearnedPreference(Base):
    __tablename__ = "learned_preference"
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    key: Mapped[str] = mapped_column(unique=True)
    value_json: Mapped[dict] = mapped_column(JSON)
    confidence: Mapped[float]
    evidence_count: Mapped[int]
    first_observed_at: Mapped[datetime] = mapped_column(UTCDateTime)
    last_observed_at: Mapped[datetime] = mapped_column(UTCDateTime)
    source: Mapped[str]


class CalendarEventSnapshot(Base):
    __tablename__ = "calendar_event_snapshot"
    calendar_event_id: Mapped[str] = mapped_column(primary_key=True)
    ical_uid: Mapped[str | None]
    etag: Mapped[str | None]
    title: Mapped[str]
    start_at: Mapped[datetime] = mapped_column(UTCDateTime)
    end_at: Mapped[datetime] = mapped_column(UTCDateTime)
    status: Mapped[str] = mapped_column(default="confirmed")
    updated_at_remote: Mapped[datetime | None] = mapped_column(UTCDateTime)
    gymclaw_managed: Mapped[bool] = mapped_column(default=False)
    gymclaw_session_id: Mapped[str | None]
    gymclaw_plan_revision: Mapped[int | None]
    user_locked: Mapped[bool] = mapped_column(default=False)
    last_seen_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow)
    raw_json: Mapped[dict] = mapped_column(JSON, default=dict)


class PlannedSession(Base):
    __tablename__ = "planned_session"
    __table_args__ = (CheckConstraint("status IN ('TENTATIVE','COMMITTED','STARTED','COMPLETED','CANCELLED','SKIPPED','MISSED')"),)
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    week_id: Mapped[str] = mapped_column(index=True)
    workout_template_id: Mapped[str | None]
    status: Mapped[str] = mapped_column(default="TENTATIVE")
    planned_start_at: Mapped[datetime] = mapped_column(UTCDateTime)
    planned_end_at: Mapped[datetime] = mapped_column(UTCDateTime)
    prep_start_at: Mapped[datetime] = mapped_column(UTCDateTime)
    leave_home_at: Mapped[datetime] = mapped_column(UTCDateTime)
    expected_finish_at: Mapped[datetime] = mapped_column(UTCDateTime)
    calendar_event_id: Mapped[str | None] = mapped_column(unique=True)
    crowd_prediction: Mapped[float | None]
    crowd_confidence: Mapped[float | None]
    user_locked: Mapped[bool] = mapped_column(default=False)
    source_revision: Mapped[int] = mapped_column(default=1)
    created_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow)
    updated_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow, onupdate=utcnow)


class WorkoutSession(Base):
    __tablename__ = "workout_session"
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    planned_session_id: Mapped[str | None] = mapped_column(ForeignKey("planned_session.id"), unique=True)
    template_id: Mapped[str | None] = mapped_column(ForeignKey("workout_template.id"))
    template_snapshot: Mapped[dict] = mapped_column(JSON, default=dict)
    last_action_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    started_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    arrived_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    completed_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    status: Mapped[str] = mapped_column(default="SCHEDULED")
    initial_eta: Mapped[datetime | None] = mapped_column(UTCDateTime)
    final_eta: Mapped[datetime | None] = mapped_column(UTCDateTime)
    actual_duration_seconds: Mapped[int | None]
    crowd_feedback: Mapped[str | None]
    waited_for_equipment_count: Mapped[int] = mapped_column(default=0)
    notes: Mapped[str | None] = mapped_column(Text)


class WorkoutExercise(Base):
    __tablename__ = "workout_exercise"
    __table_args__ = (CheckConstraint("status IN ('PENDING','ACTIVE','DEFERRED','COMPLETED','SKIPPED','SUBSTITUTED')"),)
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    workout_session_id: Mapped[str] = mapped_column(ForeignKey("workout_session.id"))
    exercise_id: Mapped[str]
    position: Mapped[int]
    status: Mapped[str] = mapped_column(default="PENDING")
    planned_working_sets: Mapped[int]
    rep_min: Mapped[int]
    rep_max: Mapped[int]
    target_weight: Mapped[float]
    rest_seconds: Mapped[int]
    substituted_from_exercise_id: Mapped[str | None]
    deferred_reason: Mapped[str | None]
    config_json: Mapped[dict] = mapped_column(JSON, default=dict)


class SetLog(Base):
    __tablename__ = "set_log"
    __table_args__ = (CheckConstraint("set_type IN ('WARMUP','WORKING')"), UniqueConstraint("workout_exercise_id", "set_type", "set_number"), CheckConstraint("weight >= 0 AND reps > 0 AND set_number > 0"))
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    workout_exercise_id: Mapped[str] = mapped_column(ForeignKey("workout_exercise.id"))
    set_number: Mapped[int]
    set_type: Mapped[str]
    weight: Mapped[float]
    reps: Mapped[int]
    rir_optional: Mapped[float | None]
    logged_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow)
    rest_started_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    rest_due_at: Mapped[datetime | None] = mapped_column(UTCDateTime)


class WorkoutTemplate(Base):
    __tablename__ = "workout_template"
    id: Mapped[str] = mapped_column(primary_key=True)
    definition_json: Mapped[dict] = mapped_column(JSON)
    updated_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow, onupdate=utcnow)


class ExerciseProgression(Base):
    __tablename__ = "exercise_progression"
    exercise_id: Mapped[str] = mapped_column(primary_key=True)
    next_weight: Mapped[float]
    updated_at: Mapped[datetime] = mapped_column(UTCDateTime)
    source_workout_id: Mapped[str] = mapped_column(ForeignKey("workout_session.id"))


class NotificationJob(Base):
    """Durable outbox; delivery integration is separate from domain execution."""
    __tablename__ = "notification_job"
    __table_args__ = (CheckConstraint("status IN ('PENDING','CANCELLED','FIRED')"),)
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    kind: Mapped[str]
    workout_session_id: Mapped[str] = mapped_column(ForeignKey("workout_session.id"))
    set_log_id: Mapped[str | None] = mapped_column(ForeignKey("set_log.id"), unique=True)
    due_at: Mapped[datetime] = mapped_column(UTCDateTime, index=True)
    status: Mapped[str] = mapped_column(default="PENDING")
    payload_json: Mapped[dict] = mapped_column(JSON, default=dict)
    external_job_id: Mapped[str | None]
    handled_at: Mapped[datetime | None] = mapped_column(UTCDateTime)


class CrowdObservation(Base):
    __tablename__ = "crowd_observation"
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    observed_at: Mapped[datetime] = mapped_column(UTCDateTime)
    source: Mapped[str]
    raw_value: Mapped[float]
    normalized_value: Mapped[float]
    freshness_seconds: Mapped[int]
    metadata_json: Mapped[dict] = mapped_column(JSON, default=dict)


class CrowdFeedback(Base):
    __tablename__ = "crowd_feedback"
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    workout_session_id: Mapped[str] = mapped_column(ForeignKey("workout_session.id"))
    observed_at: Mapped[datetime] = mapped_column(UTCDateTime)
    rating: Mapped[str]
    normalized_score: Mapped[float]
    waited_for_equipment_count: Mapped[int] = mapped_column(default=0)
    notes: Mapped[str | None]


class AgentEvent(Base):
    __tablename__ = "agent_event"
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    type: Mapped[str] = mapped_column(index=True)
    created_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow)
    payload_json: Mapped[dict] = mapped_column(JSON, default=dict)
    handled_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    correlation_id: Mapped[str | None] = mapped_column(unique=True)
