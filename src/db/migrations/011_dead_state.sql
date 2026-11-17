-- State nothing reads. A workout row only ever rests in five states; older rows in a passing state
-- take the nearest kept one.
UPDATE workout_session SET status = 'EXERCISE_ACTIVE'
	WHERE status IN ('SCHEDULED','PREPARING','COMMUTING','ARRIVED','SESSION_STARTED','EXERCISE_COMPLETE','NEXT_EXERCISE');
UPDATE workout_session SET status = 'WORKOUT_COMPLETE' WHERE status = 'POST_ANALYSIS';

-- Written (typical duration, crowd discomfort), never read.
DROP TABLE learned_preference;

-- Left over from the Python runtime: an external scheduler's job id and a fixed profile name.
ALTER TABLE notification_job DROP COLUMN external_job_id;
ALTER TABLE notification_delivery DROP COLUMN runtime_profile;

-- The runtime plans from the owner's split; profile, paths and an activated template went with Python.
-- template_id has a foreign key, so rebuild (foreign keys are off while migrating).
CREATE TABLE runtime_settings_new (
	id INTEGER NOT NULL,
	recipient VARCHAR NOT NULL,
	enabled BOOLEAN NOT NULL,
	calendar_writes_enabled BOOLEAN NOT NULL,
	crowd_polling_enabled BOOLEAN NOT NULL,
	PRIMARY KEY (id),
	CHECK (id = 1)
);
INSERT INTO runtime_settings_new SELECT id, recipient, enabled, calendar_writes_enabled, crowd_polling_enabled FROM runtime_settings;
DROP TABLE runtime_settings;
ALTER TABLE runtime_settings_new RENAME TO runtime_settings;

-- goal repeats interview_json, template_id repeats split_json[0]; the two fingerprints are never set.
CREATE TABLE onboarding_state_new (
	id INTEGER NOT NULL,
	completed_fingerprint VARCHAR,
	interview_json JSON DEFAULT '{}' NOT NULL,
	split_json JSON DEFAULT '[]' NOT NULL,
	plan_text TEXT,
	PRIMARY KEY (id),
	CHECK (id = 1)
);
INSERT INTO onboarding_state_new SELECT id, completed_fingerprint, interview_json, split_json, plan_text FROM onboarding_state;
DROP TABLE onboarding_state;
ALTER TABLE onboarding_state_new RENAME TO onboarding_state;
