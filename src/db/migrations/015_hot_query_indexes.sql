-- Partial and foreign-key indexes for the recurring runtime polls and workout/dashboard reads.
CREATE INDEX IF NOT EXISTS ix_agent_event_unhandled
    ON agent_event (type)
    WHERE handled_at IS NULL;

CREATE INDEX IF NOT EXISTS ix_notification_delivery_status
    ON notification_delivery (status, recipient, created_at, id);

DROP INDEX IF EXISTS ix_notification_job_due_at;
CREATE INDEX IF NOT EXISTS ix_notification_job_pending
    ON notification_job (due_at, id)
    WHERE status = 'PENDING';

CREATE INDEX IF NOT EXISTS ix_workout_exercise_session
    ON workout_exercise (workout_session_id);
