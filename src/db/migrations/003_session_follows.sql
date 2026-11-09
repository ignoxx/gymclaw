-- A missed or skipped session keeps its outcome; rescheduling it makes a new session that follows it.
ALTER TABLE planned_session ADD COLUMN follows_session_id VARCHAR REFERENCES planned_session (id);
