-- Schema at alembic head 3d5e7f9a1b20 (last Python revision), dumped from the live DB.
CREATE TABLE alembic_version (
	version_num VARCHAR(32) NOT NULL, 
	CONSTRAINT alembic_version_pkc PRIMARY KEY (version_num)
);
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
CREATE TABLE calendar_event_snapshot (
	calendar_event_id VARCHAR NOT NULL, 
	ical_uid VARCHAR, 
	etag VARCHAR, 
	title VARCHAR NOT NULL, 
	start_at DATETIME NOT NULL, 
	end_at DATETIME NOT NULL, 
	status VARCHAR NOT NULL, 
	updated_at_remote DATETIME, 
	gymclaw_managed BOOLEAN NOT NULL, 
	gymclaw_session_id VARCHAR, 
	gymclaw_plan_revision INTEGER, 
	user_locked BOOLEAN NOT NULL, 
	last_seen_at DATETIME NOT NULL, 
	raw_json JSON NOT NULL, 
	PRIMARY KEY (calendar_event_id)
);
CREATE TABLE learned_preference (
	id VARCHAR NOT NULL, 
	"key" VARCHAR NOT NULL, 
	value_json JSON NOT NULL, 
	confidence DOUBLE NOT NULL, 
	evidence_count INTEGER NOT NULL, 
	first_observed_at DATETIME NOT NULL, 
	last_observed_at DATETIME NOT NULL, 
	source VARCHAR NOT NULL, 
	PRIMARY KEY (id), 
	UNIQUE ("key")
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
	expected_finish_at DATETIME NOT NULL, 
	calendar_event_id VARCHAR, 
	crowd_prediction DOUBLE, 
	crowd_confidence DOUBLE, 
	user_locked BOOLEAN NOT NULL, 
	source_revision INTEGER NOT NULL, 
	created_at DATETIME NOT NULL, 
	updated_at DATETIME NOT NULL, workout_plan_json JSON DEFAULT '{}' NOT NULL, 
	PRIMARY KEY (id), 
	CHECK (status IN ('TENTATIVE','COMMITTED','STARTED','COMPLETED','CANCELLED','SKIPPED','MISSED')), 
	UNIQUE (calendar_event_id)
);
CREATE INDEX ix_planned_session_week_id ON planned_session (week_id);
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
	warmup_policy VARCHAR NOT NULL, 
	default_compound_rest_seconds INTEGER NOT NULL, 
	default_accessory_rest_seconds INTEGER NOT NULL, 
	crowd_preference_weight DOUBLE NOT NULL, 
	calendar_preference_weight DOUBLE NOT NULL, 
	recovery_weight DOUBLE NOT NULL, 
	created_at DATETIME NOT NULL, 
	updated_at DATETIME NOT NULL, gym_address VARCHAR, 
	PRIMARY KEY (id), 
	CHECK (id = 1), 
	CHECK (weekly_min_sessions >= 0 AND weekly_min_sessions <= weekly_target_sessions AND weekly_target_sessions <= weekly_max_sessions)
);
CREATE TABLE crowd_feedback (
	id VARCHAR NOT NULL, 
	workout_session_id VARCHAR NOT NULL, 
	observed_at DATETIME NOT NULL, 
	rating VARCHAR NOT NULL, 
	normalized_score DOUBLE NOT NULL, 
	waited_for_equipment_count INTEGER NOT NULL, 
	notes VARCHAR, features_json JSON DEFAULT '{}' NOT NULL, 
	PRIMARY KEY (id), 
	FOREIGN KEY(workout_session_id) REFERENCES workout_session (id)
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
CREATE TABLE workout_template (
	id VARCHAR NOT NULL, 
	definition_json JSON NOT NULL, 
	updated_at DATETIME NOT NULL, 
	PRIMARY KEY (id)
);
CREATE TABLE "set_log" (
	id VARCHAR NOT NULL, 
	workout_exercise_id VARCHAR NOT NULL, 
	set_number INTEGER NOT NULL, 
	set_type VARCHAR NOT NULL, 
	weight DOUBLE NOT NULL, 
	reps INTEGER NOT NULL, 
	rir_optional DOUBLE, 
	logged_at DATETIME NOT NULL, 
	rest_started_at DATETIME, 
	rest_due_at DATETIME, 
	PRIMARY KEY (id), 
	CONSTRAINT ck_set_log_legacy CHECK (set_type IN ('WARMUP','WORKING')), 
	CONSTRAINT uq_exercise_set UNIQUE (workout_exercise_id, set_type, set_number), 
	CONSTRAINT ck_set_values CHECK (weight >= 0 AND reps > 0 AND set_number > 0), 
	FOREIGN KEY(workout_exercise_id) REFERENCES workout_exercise (id)
);
CREATE TABLE exercise_progression (
	exercise_id VARCHAR NOT NULL, 
	next_weight DOUBLE NOT NULL, 
	updated_at DATETIME NOT NULL, 
	source_workout_id VARCHAR NOT NULL, 
	PRIMARY KEY (exercise_id), 
	FOREIGN KEY(source_workout_id) REFERENCES workout_session (id)
);
CREATE TABLE "notification_job" (
	id VARCHAR NOT NULL, 
	kind VARCHAR NOT NULL, 
	workout_session_id VARCHAR, 
	set_log_id VARCHAR, 
	due_at DATETIME NOT NULL, 
	status VARCHAR NOT NULL, 
	payload_json JSON NOT NULL, 
	external_job_id VARCHAR, 
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
CREATE TABLE calendar_credential (
	id INTEGER NOT NULL, 
	credentials_json JSON NOT NULL, 
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
CREATE TABLE "crowd_observation" (
	id VARCHAR NOT NULL, 
	observed_at DATETIME NOT NULL, 
	source VARCHAR NOT NULL, 
	raw_value DOUBLE NOT NULL, 
	normalized_value DOUBLE, 
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
CREATE TABLE "notification_delivery" (
	id VARCHAR NOT NULL, 
	notification_job_id VARCHAR, 
	recipient VARCHAR NOT NULL, 
	runtime_profile VARCHAR NOT NULL, 
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
CREATE TABLE runtime_settings (
	id INTEGER NOT NULL, 
	profile VARCHAR NOT NULL, 
	recipient VARCHAR NOT NULL, 
	project_root VARCHAR NOT NULL, 
	python_path VARCHAR NOT NULL, 
	template_id VARCHAR, 
	enabled BOOLEAN NOT NULL, 
	calendar_writes_enabled BOOLEAN NOT NULL, 
	crowd_polling_enabled BOOLEAN NOT NULL, 
	PRIMARY KEY (id), 
	CHECK (id = 1), 
	FOREIGN KEY(template_id) REFERENCES workout_template (id)
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
CREATE TABLE onboarding_state (
	id INTEGER NOT NULL, 
	goal VARCHAR, 
	profile_fingerprint VARCHAR, 
	template_id VARCHAR, 
	template_fingerprint VARCHAR, 
	completed_fingerprint VARCHAR, interview_json JSON DEFAULT '{}' NOT NULL, split_json JSON DEFAULT '[]' NOT NULL, 
	PRIMARY KEY (id), 
	CHECK (id = 1), 
	FOREIGN KEY(template_id) REFERENCES workout_template (id)
);
CREATE TABLE personal_calendar_state (
	id INTEGER NOT NULL, 
	url VARCHAR NOT NULL, 
	fetched_at DATETIME, 
	last_success_at DATETIME, 
	last_error_code VARCHAR, 
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
CREATE TABLE body_weight (
	id VARCHAR NOT NULL, 
	measured_at DATETIME NOT NULL, 
	kg DOUBLE NOT NULL, 
	created_at DATETIME NOT NULL, 
	PRIMARY KEY (id), 
	CHECK (kg >= 20 AND kg <= 400), 
	UNIQUE (measured_at)
);
CREATE TABLE "workout_session" (
	id VARCHAR NOT NULL, 
	planned_session_id VARCHAR, 
	started_at DATETIME, 
	arrived_at DATETIME, 
	completed_at DATETIME, 
	status VARCHAR NOT NULL, 
	initial_eta DATETIME, 
	final_eta DATETIME, 
	actual_duration_seconds INTEGER, 
	crowd_feedback VARCHAR, 
	waited_for_equipment_count INTEGER NOT NULL, 
	notes TEXT, 
	template_id VARCHAR, 
	template_snapshot JSON DEFAULT '{}' NOT NULL, 
	last_action_at DATETIME, 
	started_by_button BOOLEAN DEFAULT 0 NOT NULL, 
	PRIMARY KEY (id), 
	CONSTRAINT uq_workout_planned_session UNIQUE (planned_session_id), 
	CONSTRAINT fk_workout_template FOREIGN KEY(template_id) REFERENCES workout_template (id), 
	FOREIGN KEY(planned_session_id) REFERENCES planned_session (id)
);
