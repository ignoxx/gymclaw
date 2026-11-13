-- Timed exercises (bike, rower, plank): a working set is reps × weight or a time in seconds. SQLite
-- can't alter a CHECK, so rebuild set_log (foreign keys are off while migrating; ids are kept).
CREATE TABLE set_log_new (
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
	seconds INTEGER,
	PRIMARY KEY (id),
	CONSTRAINT ck_set_log_legacy CHECK (set_type IN ('WARMUP','WORKING')),
	CONSTRAINT uq_exercise_set UNIQUE (workout_exercise_id, set_type, set_number),
	CONSTRAINT ck_set_values CHECK (weight >= 0 AND set_number > 0 AND reps >= 0 AND (reps > 0 OR seconds > 0)),
	FOREIGN KEY(workout_exercise_id) REFERENCES workout_exercise (id)
);
INSERT INTO set_log_new (id, workout_exercise_id, set_number, set_type, weight, reps, rir_optional, logged_at, rest_started_at, rest_due_at)
	SELECT id, workout_exercise_id, set_number, set_type, weight, reps, rir_optional, logged_at, rest_started_at, rest_due_at FROM set_log;
DROP TABLE set_log;
ALTER TABLE set_log_new RENAME TO set_log;

-- The owner's cardio template stored Cycling as 3 × 8–10 at 0 kg, so 25 minutes of cycling couldn't be
-- logged (2026-11-12). It becomes one timed block.
UPDATE workout_template SET definition_json = (
	SELECT json_set(definition_json,
		'$.exercises[' || e.key || '].measure', 'time',
		'$.exercises[' || e.key || '].minutes', 25,
		'$.exercises[' || e.key || '].working_sets', 1)
	FROM json_each(definition_json, '$.exercises') e
	WHERE lower(json_extract(e.value, '$.name')) = 'cycling'
)
WHERE id = 'cardio' AND EXISTS (
	SELECT 1 FROM json_each(definition_json, '$.exercises') e WHERE lower(json_extract(e.value, '$.name')) = 'cycling'
);
