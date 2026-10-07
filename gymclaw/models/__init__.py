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
    gym_address: Mapped[str | None]
    created_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow)
    updated_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow, onupdate=utcnow)


class OnboardingState(Base):
    __tablename__ = "onboarding_state"
    __table_args__ = (CheckConstraint("id = 1"),)
    id: Mapped[int] = mapped_column(primary_key=True, default=1)
    goal: Mapped[str | None]
    profile_fingerprint: Mapped[str | None]
    template_id: Mapped[str | None] = mapped_column(ForeignKey("workout_template.id"))
    template_fingerprint: Mapped[str | None]
    completed_fingerprint: Mapped[str | None]
    interview_json: Mapped[dict] = mapped_column(JSON, default=dict)
    # Ordered template IDs; planned sessions rotate through them.
    split_json: Mapped[list[str]] = mapped_column(JSON, default=list)


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


class AvailabilityBlock(Base):
    __tablename__ = "availability_block"
    __table_args__ = (CheckConstraint("end_at > start_at"), CheckConstraint("kind IN ('TRAVEL','SICK','UNAVAILABLE')"), CheckConstraint("status IN ('ACTIVE','RETRACTED')"))
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    start_at: Mapped[datetime] = mapped_column(UTCDateTime, index=True)
    end_at: Mapped[datetime] = mapped_column(UTCDateTime)
    kind: Mapped[str]
    status: Mapped[str] = mapped_column(default="ACTIVE")
    created_at: Mapped[datetime] = mapped_column(UTCDateTime)
    retracted_at: Mapped[datetime | None] = mapped_column(UTCDateTime)


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
    workout_plan_json: Mapped[dict] = mapped_column(JSON, default=dict)
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
    # Started from the reminder's ▶️ button at the gym (not via chat), so arrived_at is a real arrival.
    started_by_button: Mapped[bool] = mapped_column(default=False)
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
    __table_args__ = (CheckConstraint("status IN ('PENDING','CANCELLED','FIRED')"), CheckConstraint("(kind = 'REST' AND workout_session_id IS NOT NULL AND planned_session_id IS NULL) OR (kind IN ('GET_READY','LEAVE','SESSION_START') AND planned_session_id IS NOT NULL AND workout_session_id IS NULL)", name="ck_notification_owner"))
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    kind: Mapped[str]
    workout_session_id: Mapped[str | None] = mapped_column(ForeignKey("workout_session.id"))
    planned_session_id: Mapped[str | None] = mapped_column(ForeignKey("planned_session.id"))
    set_log_id: Mapped[str | None] = mapped_column(ForeignKey("set_log.id"), unique=True)
    due_at: Mapped[datetime] = mapped_column(UTCDateTime, index=True)
    status: Mapped[str] = mapped_column(default="PENDING")
    payload_json: Mapped[dict] = mapped_column(JSON, default=dict)
    external_job_id: Mapped[str | None]
    handled_at: Mapped[datetime | None] = mapped_column(UTCDateTime)


class NotificationDelivery(Base):
    """Commit domain output before delivery. Ambiguous sends require human resolution."""
    __tablename__ = "notification_delivery"
    __table_args__ = (CheckConstraint("status IN ('PENDING','SENDING','SENT','UNKNOWN','CANCELLED')", name="ck_delivery_status"), CheckConstraint("(notification_job_id IS NOT NULL AND agent_event_id IS NULL) OR (notification_job_id IS NULL AND agent_event_id IS NOT NULL)", name="ck_delivery_owner"))
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    notification_job_id: Mapped[str | None] = mapped_column(ForeignKey("notification_job.id"), unique=True)
    agent_event_id: Mapped[str | None] = mapped_column(ForeignKey("agent_event.id"), unique=True)
    recipient: Mapped[str]
    runtime_profile: Mapped[str]
    message: Mapped[str] = mapped_column(Text)
    result_json: Mapped[dict] = mapped_column(JSON)
    status: Mapped[str] = mapped_column(default="PENDING")
    created_at: Mapped[datetime] = mapped_column(UTCDateTime)
    handled_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    external_message_id: Mapped[str | None]


class PersonalCalendarState(Base):
    """Read-only personal ICS feed. URL is a capability secret and never leaves the DB."""
    __tablename__ = "personal_calendar_state"
    __table_args__ = (CheckConstraint("id = 1"),)
    id: Mapped[int] = mapped_column(primary_key=True, default=1)
    url: Mapped[str]
    fetched_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    last_success_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    last_error_code: Mapped[str | None]


class PersonalBusyInterval(Base):
    """Busy times expanded from the personal feed. Times only, no titles."""
    __tablename__ = "personal_busy_interval"
    __table_args__ = (CheckConstraint("end_at > start_at"),)
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    start_at: Mapped[datetime] = mapped_column(UTCDateTime, index=True)
    end_at: Mapped[datetime] = mapped_column(UTCDateTime)


class RuntimeSettings(Base):
    """Explicit local activation authority; callbacks cannot grant/re-enable it."""
    __tablename__ = "runtime_settings"
    __table_args__ = (CheckConstraint("id = 1"),)
    id: Mapped[int] = mapped_column(primary_key=True, default=1)
    profile: Mapped[str]
    recipient: Mapped[str]
    project_root: Mapped[str]
    python_path: Mapped[str]
    template_id: Mapped[str | None] = mapped_column(ForeignKey("workout_template.id"))
    enabled: Mapped[bool] = mapped_column(default=True)
    calendar_writes_enabled: Mapped[bool] = mapped_column(default=False)
    crowd_polling_enabled: Mapped[bool] = mapped_column(default=False)


class CalendarSyncState(Base):
    """One explicit calendar; replacing its ID requires a separate DB."""
    __tablename__ = "calendar_sync_state"
    __table_args__ = (CheckConstraint("id = 1"),)
    id: Mapped[int] = mapped_column(primary_key=True, default=1)
    calendar_id: Mapped[str]
    timezone: Mapped[str] = mapped_column(default="Europe/Berlin")
    sync_token: Mapped[str | None]
    last_synced_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    source: Mapped[str]


class CalendarCredential(Base):
    """OAuth tokens stay local/private and never appear in tool results."""
    __tablename__ = "calendar_credential"
    __table_args__ = (CheckConstraint("id = 1"),)
    id: Mapped[int] = mapped_column(primary_key=True, default=1)
    credentials_json: Mapped[dict] = mapped_column(JSON)


class CalendarWrite(Base):
    """Persist before network writes; stable event IDs make retries safe."""
    __tablename__ = "calendar_write"
    __table_args__ = (UniqueConstraint("planned_session_id", "revision", "action"), CheckConstraint("status IN ('PENDING','APPLIED','CONFLICT','CANCELLED')"), CheckConstraint("action IN ('CREATE','UPDATE','DELETE')"))
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    planned_session_id: Mapped[str] = mapped_column(ForeignKey("planned_session.id"))
    revision: Mapped[int]
    action: Mapped[str]
    event_id: Mapped[str]
    expected_etag: Mapped[str | None]
    body_json: Mapped[dict] = mapped_column(JSON, default=dict)
    status: Mapped[str] = mapped_column(default="PENDING")
    created_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow)
    handled_at: Mapped[datetime | None] = mapped_column(UTCDateTime)


class CrowdObservation(Base):
    __tablename__ = "crowd_observation"
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    observed_at: Mapped[datetime] = mapped_column(UTCDateTime)
    source: Mapped[str]
    raw_value: Mapped[float]
    normalized_value: Mapped[float | None]
    freshness_seconds: Mapped[int | None]
    metadata_json: Mapped[dict] = mapped_column(JSON, default=dict)


class CrowdSourceState(Base):
    """Retrieval health is separate from unknown provider/cache freshness."""
    __tablename__ = "crowd_source_state"
    source: Mapped[str] = mapped_column(primary_key=True)
    provider_id: Mapped[str]
    last_success_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    last_failure_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    last_error_code: Mapped[str | None]
    last_change_at: Mapped[datetime | None] = mapped_column(UTCDateTime)
    last_raw_value: Mapped[float | None]
    consecutive_failures: Mapped[int] = mapped_column(default=0)
    reliability: Mapped[float] = mapped_column(default=0.5)
    evidence_count: Mapped[int] = mapped_column(default=0)


class CrowdFeedback(Base):
    __tablename__ = "crowd_feedback"
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    workout_session_id: Mapped[str] = mapped_column(ForeignKey("workout_session.id"))
    observed_at: Mapped[datetime] = mapped_column(UTCDateTime)
    rating: Mapped[str]
    normalized_score: Mapped[float]
    features_json: Mapped[dict] = mapped_column(JSON, default=dict)
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


class BodyWeight(Base):
    """One weigh-in. `measured_at` is unique, so re-importing the same scale photo is a no-op."""
    __tablename__ = "body_weight"
    __table_args__ = (CheckConstraint("kg >= 20 AND kg <= 400"),)
    id: Mapped[str] = mapped_column(primary_key=True, default=new_id)
    measured_at: Mapped[datetime] = mapped_column(UTCDateTime, unique=True)
    kg: Mapped[float]
    created_at: Mapped[datetime] = mapped_column(UTCDateTime, default=utcnow)
