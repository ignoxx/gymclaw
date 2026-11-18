-- Generated from baseline.sql plus every migration (user_version 11); don't edit. When a
-- migration changes the schema, `cargo test schema_snapshot` rewrites this file: review and
-- commit it. Fresh databases are created from this file.

CREATE TABLE activity (
	id VARCHAR PRIMARY KEY,
	kind TEXT NOT NULL,
	started_at DATETIME NOT NULL,
	minutes INTEGER NOT NULL CHECK (minutes > 0),
	intensity VARCHAR NOT NULL CHECK (intensity IN ('easy', 'moderate', 'hard')),
	notes TEXT,
	weekly BOOLEAN NOT NULL DEFAULT 0,
	until_at DATETIME,
	removed_at DATETIME,
	created_at DATETIME NOT NULL
);

CREATE INDEX ix_activity_started_at ON activity (started_at);

CREATE TABLE agent_button (
	id VARCHAR PRIMARY KEY,
	label VARCHAR NOT NULL,
	value TEXT NOT NULL,
	created_at DATETIME NOT NULL
);

CREATE TABLE agent_data (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	collection VARCHAR NOT NULL,
	at DATETIME NOT NULL,
	data_json JSON NOT NULL,
	created_at DATETIME NOT NULL
);

CREATE INDEX ix_agent_data_collection_at ON agent_data (collection, at);

CREATE TABLE agent_event (
	id VARCHAR NOT NULL, 
	type VARCHAR NOT NULL, 
	created_at DATETIME NOT NULL, 
	payload_json JSON NOT NULL, 
	handled_at DATETIME, 
	correlation_id VARCHAR, 
	PRIMARY KEY (id), 
	UNIQUE (correlation_id)
);

CREATE INDEX ix_agent_event_type ON agent_event (type);

CREATE TABLE "agent_note" (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	kind VARCHAR NOT NULL CHECK (kind IN ('owner', 'learned')),
	text TEXT NOT NULL,
	created_at DATETIME NOT NULL,
	updated_at DATETIME NOT NULL
);

CREATE TABLE alembic_version (
	version_num VARCHAR(32) NOT NULL, 
	CONSTRAINT alembic_version_pkc PRIMARY KEY (version_num)
);

CREATE TABLE availability_block (
	id VARCHAR NOT NULL, 
	start_at DATETIME NOT NULL, 
	end_at DATETIME NOT NULL, 
	kind VARCHAR NOT NULL, 
	status VARCHAR NOT NULL, 
	created_at DATETIME NOT NULL, 
	retracted_at DATETIME, 
	PRIMARY KEY (id), 
	CHECK (end_at > start_at), 
	CHECK (kind IN ('TRAVEL','SICK','UNAVAILABLE')), 
	CHECK (status IN ('ACTIVE','RETRACTED'))
);

CREATE INDEX ix_availability_block_start_at ON availability_block (start_at);

CREATE TABLE body_weight (
	id VARCHAR NOT NULL, 
	measured_at DATETIME NOT NULL, 
	kg DOUBLE NOT NULL, 
	created_at DATETIME NOT NULL, 
	PRIMARY KEY (id), 
	CHECK (kg >= 20 AND kg <= 400), 
	UNIQUE (measured_at)
);

CREATE TABLE calendar_credential (
	id INTEGER NOT NULL, 
	credentials_json JSON NOT NULL, 
	PRIMARY KEY (id), 
	CHECK (id = 1)
);

CREATE TABLE calendar_event_snapshot (
	calendar_event_id VARCHAR NOT NULL, 
	etag VARCHAR, 
	title VARCHAR NOT NULL, 
	start_at DATETIME NOT NULL, 
	end_at DATETIME NOT NULL, 
	status VARCHAR NOT NULL, 
	gymclaw_managed BOOLEAN NOT NULL, 
	gymclaw_session_id VARCHAR, 
	user_locked BOOLEAN NOT NULL, 
	raw_json JSON NOT NULL, 
	PRIMARY KEY (calendar_event_id)
);

CREATE TABLE calendar_sync_state (
	id INTEGER NOT NULL, 
	calendar_id VARCHAR NOT NULL, 
	timezone VARCHAR NOT NULL, 
	sync_token VARCHAR, 
	last_synced_at DATETIME, 
	source VARCHAR NOT NULL, 
	PRIMARY KEY (id), 
	CHECK (id = 1)
);

CREATE TABLE calendar_write (
	id VARCHAR NOT NULL, 
	planned_session_id VARCHAR NOT NULL, 
	revision INTEGER NOT NULL, 
	action VARCHAR NOT NULL, 
	event_id VARCHAR NOT NULL, 
	expected_etag VARCHAR, 
	body_json JSON NOT NULL, 
	status VARCHAR NOT NULL, 
	created_at DATETIME NOT NULL, 
	handled_at DATETIME, 
	PRIMARY KEY (id), 
	UNIQUE (planned_session_id, revision, action), 
	CHECK (status IN ('PENDING','APPLIED','CONFLICT','CANCELLED')), 
	CHECK (action IN ('CREATE','UPDATE','DELETE')), 
	FOREIGN KEY(planned_session_id) REFERENCES planned_session (id)
);

CREATE TABLE capability_gap (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	at DATETIME NOT NULL,
	source VARCHAR NOT NULL CHECK (source IN ('agent', 'step_cap', 'deadline', 'repeated_call')),
	asked TEXT NOT NULL,
	tried TEXT NOT NULL,
	why TEXT NOT NULL,
	run_id VARCHAR
);

CREATE TABLE chat_message (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	at DATETIME NOT NULL,
	role VARCHAR NOT NULL CHECK (role IN ('owner', 'assistant', 'bot')),
	text TEXT NOT NULL,
	telegram_message_id INTEGER,
	meta_json JSON NOT NULL DEFAULT '{}'
);

CREATE INDEX ix_chat_message_at ON chat_message (at);

CREATE TABLE crowd_feedback (
	id VARCHAR NOT NULL, 
	workout_session_id VARCHAR NOT NULL, 
	observed_at DATETIME NOT NULL, 
	rating VARCHAR NOT NULL, 
	normalized_score DOUBLE NOT NULL, 
	features_json JSON DEFAULT '{}' NOT NULL, 
	PRIMARY KEY (id), 
	FOREIGN KEY(workout_session_id) REFERENCES workout_session (id)
);

CREATE TABLE "crowd_observation" (
	id VARCHAR NOT NULL, 
	observed_at DATETIME NOT NULL, 
	source VARCHAR NOT NULL, 
	raw_value DOUBLE NOT NULL, 
	freshness_seconds INTEGER, 
	metadata_json JSON NOT NULL, 
	PRIMARY KEY (id)
);

CREATE TABLE crowd_source_state (
	source VARCHAR NOT NULL, 
	provider_id VARCHAR NOT NULL, 
	last_success_at DATETIME, 
	last_failure_at DATETIME, 
	last_error_code VARCHAR, 
	last_change_at DATETIME, 
	last_raw_value DOUBLE, 
	consecutive_failures INTEGER NOT NULL, 
	reliability DOUBLE NOT NULL, 
	evidence_count INTEGER NOT NULL, 
	PRIMARY KEY (source)
);

CREATE TABLE exercise_progression (
	exercise_id VARCHAR NOT NULL, 
	next_weight DOUBLE NOT NULL, 
	updated_at DATETIME NOT NULL, 
	source_workout_id VARCHAR NOT NULL, 
	PRIMARY KEY (exercise_id), 
	FOREIGN KEY(source_workout_id) REFERENCES workout_session (id)
);

CREATE TABLE extension (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	kind VARCHAR NOT NULL CHECK (kind IN ('playbook', 'collection', 'trigger')),
	name VARCHAR NOT NULL,
	spec_json JSON NOT NULL,
	enabled BOOLEAN NOT NULL,
	version INTEGER NOT NULL,
	created_at DATETIME NOT NULL,
	updated_at DATETIME NOT NULL,
	UNIQUE (kind, name)
);

CREATE TABLE extension_version (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	extension_id INTEGER NOT NULL REFERENCES extension (id),
	version INTEGER NOT NULL,
	spec_json JSON NOT NULL,
	enabled BOOLEAN NOT NULL,
	author VARCHAR NOT NULL CHECK (author IN ('agent', 'owner')),
	created_at DATETIME NOT NULL,
	UNIQUE (extension_id, version)
);

CREATE TABLE goal (
	id VARCHAR PRIMARY KEY,
	text TEXT NOT NULL,
	kind VARCHAR NOT NULL CHECK (kind IN ('lift', 'body_weight', 'cardio_minutes_week', 'sessions_week', 'other')),
	exercise_id VARCHAR CHECK (kind != 'lift' OR exercise_id IS NOT NULL),
	target DOUBLE,
	target_date VARCHAR,
	status VARCHAR NOT NULL CHECK (status IN ('ACTIVE', 'DONE', 'DROPPED')),
	block_text TEXT,
	created_at DATETIME NOT NULL,
	updated_at DATETIME NOT NULL,
	closed_at DATETIME
);

CREATE TABLE gym_location (
	name VARCHAR NOT NULL,
	equipment_json JSON NOT NULL,
	notes TEXT,
	updated_at DATETIME NOT NULL,
	PRIMARY KEY (name)
);

CREATE VIEW lift_history AS
SELECT w.id AS workout_id, w.started_at, e.exercise_id, e.config_json ->> 'name' AS name,
	e.rep_min, e.rep_max, e.target_weight,
	max(s.weight) AS top_weight,
	group_concat(s.reps, '/' ORDER BY s.logged_at, s.set_number) AS reps,
	round(max(CASE WHEN s.reps <= 1 THEN s.weight ELSE s.weight * (1 + s.reps / 30.0) END), 1) AS e1rm
FROM workout_session w
JOIN workout_exercise e ON e.workout_session_id = w.id
JOIN set_log s ON s.workout_exercise_id = e.id
WHERE w.status = 'PLAN_UPDATED' AND s.set_type = 'WORKING' AND s.seconds IS NULL AND s.meters IS NULL
	AND coalesce(e.config_json ->> 'load', '') != 'assisted'
GROUP BY e.id;

CREATE TABLE "notification_delivery" (
	id VARCHAR NOT NULL, 
	notification_job_id VARCHAR, 
	recipient VARCHAR NOT NULL, 
	message TEXT NOT NULL, 
	result_json JSON NOT NULL, 
	status VARCHAR NOT NULL, 
	created_at DATETIME NOT NULL, 
	handled_at DATETIME, 
	external_message_id VARCHAR, 
	agent_event_id VARCHAR, 
	PRIMARY KEY (id), 
	CONSTRAINT ck_delivery_status CHECK (status IN ('PENDING','SENDING','SENT','UNKNOWN','CANCELLED')), 
	CONSTRAINT fk_delivery_event FOREIGN KEY(agent_event_id) REFERENCES agent_event (id), 
	CONSTRAINT uq_delivery_event UNIQUE (agent_event_id), 
	CONSTRAINT ck_delivery_owner CHECK ((notification_job_id IS NOT NULL AND agent_event_id IS NULL) OR (notification_job_id IS NULL AND agent_event_id IS NOT NULL)), 
	UNIQUE (notification_job_id), 
	FOREIGN KEY(notification_job_id) REFERENCES notification_job (id)
);

CREATE TABLE "notification_job" (
	id VARCHAR NOT NULL, 
	kind VARCHAR NOT NULL, 
	workout_session_id VARCHAR, 
	set_log_id VARCHAR, 
	due_at DATETIME NOT NULL, 
	status VARCHAR NOT NULL, 
	payload_json JSON NOT NULL, 
	handled_at DATETIME, 
	planned_session_id VARCHAR, 
	PRIMARY KEY (id), 
	CONSTRAINT ck_notification_job_legacy CHECK (status IN ('PENDING','CANCELLED','FIRED')), 
	CONSTRAINT fk_notification_plan FOREIGN KEY(planned_session_id) REFERENCES planned_session (id), 
	CONSTRAINT ck_notification_owner CHECK ((kind = 'REST' AND workout_session_id IS NOT NULL AND planned_session_id IS NULL) OR (kind IN ('GET_READY','LEAVE','SESSION_START') AND planned_session_id IS NOT NULL AND workout_session_id IS NULL)), 
	UNIQUE (set_log_id), 
	FOREIGN KEY(workout_session_id) REFERENCES workout_session (id), 
	FOREIGN KEY(set_log_id) REFERENCES set_log (id)
);

CREATE INDEX ix_notification_job_due_at ON notification_job (due_at);

CREATE TABLE "onboarding_state" (
	id INTEGER NOT NULL,
	completed_fingerprint VARCHAR,
	interview_json JSON DEFAULT '{}' NOT NULL,
	split_json JSON DEFAULT '[]' NOT NULL,
	plan_text TEXT,
	PRIMARY KEY (id),
	CHECK (id = 1)
);

CREATE TABLE personal_busy_interval (
	id INTEGER NOT NULL, 
	start_at DATETIME NOT NULL, 
	end_at DATETIME NOT NULL, 
	PRIMARY KEY (id), 
	CHECK (end_at > start_at)
);

CREATE INDEX ix_personal_busy_interval_start_at ON personal_busy_interval (start_at);

CREATE TABLE personal_calendar_state (
	id INTEGER NOT NULL, 
	url VARCHAR NOT NULL, 
	fetched_at DATETIME, 
	last_success_at DATETIME, 
	last_error_code VARCHAR, 
	PRIMARY KEY (id), 
	CHECK (id = 1)
);

CREATE TABLE planned_session (
	id VARCHAR NOT NULL, 
	week_id VARCHAR NOT NULL, 
	workout_template_id VARCHAR, 
	status VARCHAR NOT NULL, 
	planned_start_at DATETIME NOT NULL, 
	planned_end_at DATETIME NOT NULL, 
	prep_start_at DATETIME NOT NULL, 
	leave_home_at DATETIME NOT NULL, 
	calendar_event_id VARCHAR, 
	user_locked BOOLEAN NOT NULL, 
	source_revision INTEGER NOT NULL, 
	created_at DATETIME NOT NULL, 
	updated_at DATETIME NOT NULL, workout_plan_json JSON DEFAULT '{}' NOT NULL, follows_session_id VARCHAR REFERENCES planned_session (id), 
	PRIMARY KEY (id), 
	CHECK (status IN ('TENTATIVE','COMMITTED','STARTED','COMPLETED','CANCELLED','SKIPPED','MISSED')), 
	UNIQUE (calendar_event_id)
);

CREATE INDEX ix_planned_session_week_id ON planned_session (week_id);

CREATE TABLE "runtime_settings" (
	id INTEGER NOT NULL,
	recipient VARCHAR NOT NULL,
	enabled BOOLEAN NOT NULL,
	calendar_writes_enabled BOOLEAN NOT NULL,
	crowd_polling_enabled BOOLEAN NOT NULL,
	PRIMARY KEY (id),
	CHECK (id = 1)
);

CREATE TABLE "set_log" (
	id VARCHAR NOT NULL,
	workout_exercise_id VARCHAR NOT NULL,
	set_number INTEGER NOT NULL,
	set_type VARCHAR NOT NULL,
	weight DOUBLE NOT NULL,
	reps INTEGER NOT NULL,
	logged_at DATETIME NOT NULL,
	rest_due_at DATETIME,
	seconds INTEGER,
	meters DOUBLE,
	PRIMARY KEY (id),
	CONSTRAINT ck_set_log_legacy CHECK (set_type IN ('WARMUP','WORKING')),
	CONSTRAINT uq_exercise_set UNIQUE (workout_exercise_id, set_type, set_number),
	CONSTRAINT ck_set_values CHECK (weight >= 0 AND set_number > 0 AND reps >= 0 AND (reps > 0 OR seconds > 0 OR meters > 0)),
	FOREIGN KEY(workout_exercise_id) REFERENCES workout_exercise (id)
);

CREATE TABLE user_profile (
	id INTEGER NOT NULL, 
	timezone VARCHAR NOT NULL, 
	weekly_min_sessions INTEGER NOT NULL, 
	weekly_target_sessions INTEGER NOT NULL, 
	weekly_max_sessions INTEGER NOT NULL, 
	weekdays_allowed JSON NOT NULL, 
	minimum_full_rest_days_between_sessions INTEGER NOT NULL, 
	earliest_workout_start VARCHAR NOT NULL, 
	latest_workout_finish VARCHAR NOT NULL, 
	preferred_workout_minutes INTEGER NOT NULL, 
	minimum_workout_minutes INTEGER NOT NULL, 
	prep_minutes INTEGER NOT NULL, 
	commute_to_gym_minutes INTEGER NOT NULL, 
	commute_home_minutes INTEGER NOT NULL, 
	rest_seconds INTEGER NOT NULL, 
	crowd_preference_weight DOUBLE NOT NULL, 
	created_at DATETIME NOT NULL, 
	updated_at DATETIME NOT NULL, gym_address VARCHAR, 
	PRIMARY KEY (id), 
	CHECK (id = 1), 
	CHECK (weekly_min_sessions >= 0 AND weekly_min_sessions <= weekly_target_sessions AND weekly_target_sessions <= weekly_max_sessions)
);

CREATE TABLE workout_exercise (
	id VARCHAR NOT NULL, 
	workout_session_id VARCHAR NOT NULL, 
	exercise_id VARCHAR NOT NULL, 
	position INTEGER NOT NULL, 
	status VARCHAR NOT NULL, 
	planned_working_sets INTEGER NOT NULL, 
	rep_min INTEGER NOT NULL, 
	rep_max INTEGER NOT NULL, 
	target_weight DOUBLE NOT NULL, 
	rest_seconds INTEGER NOT NULL, 
	substituted_from_exercise_id VARCHAR, 
	deferred_reason VARCHAR, config_json JSON DEFAULT '{}' NOT NULL, 
	PRIMARY KEY (id), 
	CHECK (status IN ('PENDING','ACTIVE','DEFERRED','COMPLETED','SKIPPED','SUBSTITUTED')), 
	FOREIGN KEY(workout_session_id) REFERENCES workout_session (id)
);

CREATE TABLE "workout_session" (
	id VARCHAR NOT NULL, 
	planned_session_id VARCHAR, 
	started_at DATETIME, 
	completed_at DATETIME, 
	status VARCHAR NOT NULL, 
	initial_eta DATETIME, 
	final_eta DATETIME, 
	actual_duration_seconds INTEGER, 
	crowd_feedback VARCHAR, 
	waited_for_equipment_count INTEGER NOT NULL, 
	template_id VARCHAR, 
	template_snapshot JSON DEFAULT '{}' NOT NULL, 
	last_action_at DATETIME, 
	started_by_button BOOLEAN DEFAULT 0 NOT NULL, 
	PRIMARY KEY (id), 
	CONSTRAINT uq_workout_planned_session UNIQUE (planned_session_id), 
	CONSTRAINT fk_workout_template FOREIGN KEY(template_id) REFERENCES workout_template (id), 
	FOREIGN KEY(planned_session_id) REFERENCES planned_session (id)
);

CREATE TABLE workout_template (
	id VARCHAR NOT NULL, 
	definition_json JSON NOT NULL, 
	updated_at DATETIME NOT NULL, 
	PRIMARY KEY (id)
);
