//! Row types for every table. Status columns are enums, so `match` covers each state.
use serde_json::Value;

use crate::db::Json;
use crate::time::Ts;
use crate::{sql_enum, table};

sql_enum!(SessionStatus { Tentative = "TENTATIVE", Committed = "COMMITTED", Started = "STARTED", Completed = "COMPLETED", Cancelled = "CANCELLED", Skipped = "SKIPPED", Missed = "MISSED" });
sql_enum!(WorkoutStatus {
    Scheduled = "SCHEDULED", Preparing = "PREPARING", Commuting = "COMMUTING", Arrived = "ARRIVED",
    SessionStarted = "SESSION_STARTED", ExerciseActive = "EXERCISE_ACTIVE", SetActive = "SET_ACTIVE", Resting = "RESTING",
    ExerciseComplete = "EXERCISE_COMPLETE", NextExercise = "NEXT_EXERCISE", WorkoutComplete = "WORKOUT_COMPLETE",
    PostAnalysis = "POST_ANALYSIS", PlanUpdated = "PLAN_UPDATED",
});
sql_enum!(ExerciseStatus { Pending = "PENDING", Active = "ACTIVE", Deferred = "DEFERRED", Completed = "COMPLETED", Skipped = "SKIPPED", Substituted = "SUBSTITUTED" });
sql_enum!(SetType { Warmup = "WARMUP", Working = "WORKING" });
sql_enum!(JobKind { Rest = "REST", GetReady = "GET_READY", Leave = "LEAVE", SessionStart = "SESSION_START" });
sql_enum!(JobStatus { Pending = "PENDING", Cancelled = "CANCELLED", Fired = "FIRED" });
sql_enum!(DeliveryStatus { Pending = "PENDING", Sending = "SENDING", Sent = "SENT", Unknown = "UNKNOWN", Cancelled = "CANCELLED" });
sql_enum!(WriteStatus { Pending = "PENDING", Applied = "APPLIED", Conflict = "CONFLICT", Cancelled = "CANCELLED" });
sql_enum!(WriteAction { Create = "CREATE", Update = "UPDATE", Delete = "DELETE" });
sql_enum!(BlockKind { Travel = "TRAVEL", Sick = "SICK", Unavailable = "UNAVAILABLE" });
sql_enum!(BlockStatus { Active = "ACTIVE", Retracted = "RETRACTED" });
sql_enum!(ChatRole { Owner = "owner", Assistant = "assistant", Bot = "bot" });
sql_enum!(NoteKind { Owner = "owner", Lesson = "lesson" });

impl SessionStatus {
    /// Sessions that take a slot in the week.
    pub const ACTIVE: [SessionStatus; 4] = [SessionStatus::Tentative, SessionStatus::Committed, SessionStatus::Started, SessionStatus::Completed];

    pub fn is_active(self) -> bool {
        Self::ACTIVE.contains(&self)
    }

    /// Upcoming and still editable.
    pub fn is_open(self) -> bool {
        matches!(self, SessionStatus::Tentative | SessionStatus::Committed)
    }
}

table!(UserProfile = "user_profile" {
    id: i64,
    timezone: String,
    weekly_min_sessions: i64,
    weekly_target_sessions: i64,
    weekly_max_sessions: i64,
    weekdays_allowed: Json<Vec<i64>>,
    minimum_full_rest_days_between_sessions: i64,
    earliest_workout_start: String,
    latest_workout_finish: String,
    preferred_workout_minutes: i64,
    minimum_workout_minutes: i64,
    prep_minutes: i64,
    commute_to_gym_minutes: i64,
    commute_home_minutes: i64,
    warmup_policy: String,
    default_compound_rest_seconds: i64,
    default_accessory_rest_seconds: i64,
    crowd_preference_weight: f64,
    calendar_preference_weight: f64,
    recovery_weight: f64,
    gym_address: Option<String>,
    created_at: Ts,
    updated_at: Ts,
});

table!(OnboardingState = "onboarding_state" {
    id: i64,
    goal: Option<String>,
    profile_fingerprint: Option<String>,
    template_id: Option<String>,
    template_fingerprint: Option<String>,
    completed_fingerprint: Option<String>,
    interview_json: Json<Value>,
    /// Ordered template IDs; planned sessions rotate through them.
    split_json: Json<Vec<String>>,
});

table!(LearnedPreference = "learned_preference" {
    id: String,
    key: String,
    value_json: Json<Value>,
    confidence: f64,
    evidence_count: i64,
    first_observed_at: Ts,
    last_observed_at: Ts,
    source: String,
});

table!(AvailabilityBlock = "availability_block" {
    id: String,
    start_at: Ts,
    end_at: Ts,
    kind: BlockKind,
    status: BlockStatus,
    created_at: Ts,
    retracted_at: Option<Ts>,
});

table!(CalendarEventSnapshot = "calendar_event_snapshot" {
    calendar_event_id: String,
    ical_uid: Option<String>,
    etag: Option<String>,
    title: String,
    start_at: Ts,
    end_at: Ts,
    status: String,
    updated_at_remote: Option<Ts>,
    gymclaw_managed: bool,
    gymclaw_session_id: Option<String>,
    gymclaw_plan_revision: Option<i64>,
    user_locked: bool,
    last_seen_at: Ts,
    raw_json: Json<Value>,
});

table!(PlannedSession = "planned_session" {
    id: String,
    week_id: String,
    workout_template_id: Option<String>,
    workout_plan_json: Json<Value>,
    status: SessionStatus,
    planned_start_at: Ts,
    planned_end_at: Ts,
    prep_start_at: Ts,
    leave_home_at: Ts,
    expected_finish_at: Ts,
    calendar_event_id: Option<String>,
    crowd_prediction: Option<f64>,
    crowd_confidence: Option<f64>,
    user_locked: bool,
    source_revision: i64,
    created_at: Ts,
    updated_at: Ts,
});

table!(WorkoutSession = "workout_session" {
    id: String,
    planned_session_id: Option<String>,
    template_id: Option<String>,
    template_snapshot: Json<Value>,
    last_action_at: Option<Ts>,
    started_at: Option<Ts>,
    arrived_at: Option<Ts>,
    /// Started from the reminder's ▶️ button at the gym (not via chat), so arrived_at is a real arrival.
    started_by_button: bool,
    completed_at: Option<Ts>,
    status: WorkoutStatus,
    initial_eta: Option<Ts>,
    final_eta: Option<Ts>,
    actual_duration_seconds: Option<i64>,
    crowd_feedback: Option<String>,
    waited_for_equipment_count: i64,
    notes: Option<String>,
});

table!(WorkoutExercise = "workout_exercise" {
    id: String,
    workout_session_id: String,
    exercise_id: String,
    position: i64,
    status: ExerciseStatus,
    planned_working_sets: i64,
    rep_min: i64,
    rep_max: i64,
    target_weight: f64,
    rest_seconds: i64,
    substituted_from_exercise_id: Option<String>,
    deferred_reason: Option<String>,
    config_json: Json<Value>,
});

table!(SetLog = "set_log" {
    id: String,
    workout_exercise_id: String,
    set_number: i64,
    set_type: SetType,
    weight: f64,
    reps: i64,
    rir_optional: Option<f64>,
    logged_at: Ts,
    rest_started_at: Option<Ts>,
    rest_due_at: Option<Ts>,
});

table!(WorkoutTemplate = "workout_template" {
    id: String,
    definition_json: Json<Value>,
    updated_at: Ts,
});

table!(ExerciseProgression = "exercise_progression" {
    exercise_id: String,
    next_weight: f64,
    updated_at: Ts,
    source_workout_id: String,
});

table!(
    /// Durable reminders and rest timers; the scheduler fires them at `due_at`.
    NotificationJob = "notification_job" {
    id: String,
    kind: JobKind,
    workout_session_id: Option<String>,
    planned_session_id: Option<String>,
    set_log_id: Option<String>,
    due_at: Ts,
    status: JobStatus,
    payload_json: Json<Value>,
    external_job_id: Option<String>,
    handled_at: Option<Ts>,
});

table!(
    /// Outbox: committed before sending. SENDING/UNKNOWN never auto-retry (Telegram has no idempotent send).
    NotificationDelivery = "notification_delivery" {
    id: String,
    notification_job_id: Option<String>,
    agent_event_id: Option<String>,
    recipient: String,
    runtime_profile: String,
    message: String,
    result_json: Json<Value>,
    status: DeliveryStatus,
    created_at: Ts,
    handled_at: Option<Ts>,
    external_message_id: Option<String>,
});

table!(
    /// Read-only personal ICS feed. The URL is a capability secret and never leaves the DB.
    PersonalCalendarState = "personal_calendar_state" {
    id: i64,
    url: String,
    fetched_at: Option<Ts>,
    last_success_at: Option<Ts>,
    last_error_code: Option<String>,
});

table!(PersonalBusyInterval = "personal_busy_interval" {
    /// None on insert: SQLite assigns it.
    id: Option<i64>,
    start_at: Ts,
    end_at: Ts,
});

table!(
    /// Kept for schema compatibility. `profile`, `project_root` and `python_path` described OpenClaw
    /// callbacks and are unused now; `enabled` and the two authorities still gate the runtime.
    RuntimeSettings = "runtime_settings" {
    id: i64,
    profile: String,
    recipient: String,
    project_root: String,
    python_path: String,
    template_id: Option<String>,
    enabled: bool,
    calendar_writes_enabled: bool,
    crowd_polling_enabled: bool,
});

table!(CalendarSyncState = "calendar_sync_state" {
    id: i64,
    calendar_id: String,
    timezone: String,
    sync_token: Option<String>,
    last_synced_at: Option<Ts>,
    source: String,
});

table!(
    /// OAuth tokens (google-auth JSON). Never in tool results.
    CalendarCredential = "calendar_credential" {
    id: i64,
    credentials_json: Json<Value>,
});

table!(
    /// Persisted before network writes; stable event IDs make retries safe.
    CalendarWrite = "calendar_write" {
    id: String,
    planned_session_id: String,
    revision: i64,
    action: WriteAction,
    event_id: String,
    expected_etag: Option<String>,
    body_json: Json<Value>,
    status: WriteStatus,
    created_at: Ts,
    handled_at: Option<Ts>,
});

table!(CrowdObservation = "crowd_observation" {
    id: String,
    observed_at: Ts,
    source: String,
    raw_value: f64,
    normalized_value: Option<f64>,
    freshness_seconds: Option<i64>,
    metadata_json: Json<Value>,
});

table!(CrowdSourceState = "crowd_source_state" {
    source: String,
    provider_id: String,
    last_success_at: Option<Ts>,
    last_failure_at: Option<Ts>,
    last_error_code: Option<String>,
    last_change_at: Option<Ts>,
    last_raw_value: Option<f64>,
    consecutive_failures: i64,
    reliability: f64,
    evidence_count: i64,
});

table!(CrowdFeedback = "crowd_feedback" {
    id: String,
    workout_session_id: String,
    observed_at: Ts,
    rating: String,
    normalized_score: f64,
    features_json: Json<Value>,
    waited_for_equipment_count: i64,
    notes: Option<String>,
});

table!(AgentEvent = "agent_event" {
    id: String,
    r#type: String,
    created_at: Ts,
    payload_json: Json<Value>,
    handled_at: Option<Ts>,
    correlation_id: Option<String>,
});

table!(
    /// One weigh-in. `measured_at` is unique, so re-importing the same scale photo is a no-op.
    BodyWeight = "body_weight" {
    id: String,
    measured_at: Ts,
    kg: f64,
    created_at: Ts,
});

table!(ChatMessage = "chat_message" {
    /// None on insert: SQLite assigns it.
    id: Option<i64>,
    at: Ts,
    role: ChatRole,
    text: String,
    telegram_message_id: Option<i64>,
    meta_json: Json<Value>,
});

table!(AgentNote = "agent_note" {
    /// None on insert: SQLite assigns it.
    id: Option<i64>,
    kind: NoteKind,
    text: String,
    created_at: Ts,
    updated_at: Ts,
});

table!(AgentButton = "agent_button" {
    id: String,
    label: String,
    value: String,
    created_at: Ts,
});

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of a real database loads into the row types. Run against a copy of prod:
    /// `GYMCLAW_COMPAT_DB=/path/copy.db cargo test -- --ignored loads_real_db`
    #[test]
    #[ignore]
    fn loads_real_db() {
        let path = std::env::var("GYMCLAW_COMPAT_DB").expect("GYMCLAW_COMPAT_DB");
        let mut db = crate::db::open(std::path::Path::new(&path)).unwrap();
        crate::db::migrate(&mut db).unwrap();
        macro_rules! all {
            ($($t:ty),*) => {$(
                let rows = <$t>::select(&db, "", []).unwrap();
                for row in rows.iter().take(3) {
                    // Writing a row back unchanged round-trips (only JSON whitespace differs from Python).
                    row.update(&db).unwrap();
                }
                println!("{} {}", <$t>::TABLE, rows.len());
            )*};
        }
        all!(UserProfile, OnboardingState, LearnedPreference, AvailabilityBlock, CalendarEventSnapshot, PlannedSession, WorkoutSession,
            WorkoutExercise, SetLog, WorkoutTemplate, ExerciseProgression, NotificationJob, NotificationDelivery, PersonalCalendarState,
            PersonalBusyInterval, RuntimeSettings, CalendarSyncState, CalendarCredential, CalendarWrite, CrowdObservation,
            CrowdSourceState, CrowdFeedback, AgentEvent, BodyWeight, ChatMessage, AgentNote, AgentButton);
    }
}
