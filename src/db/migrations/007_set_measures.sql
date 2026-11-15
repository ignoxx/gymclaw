-- Distance sets (rower 2 km, carry 40 m): a working set is reps, a time in seconds or a distance in
-- meters. Rebuilt like 006, since SQLite can't alter a CHECK (ids are kept).
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
	meters DOUBLE,
	PRIMARY KEY (id),
	CONSTRAINT ck_set_log_legacy CHECK (set_type IN ('WARMUP','WORKING')),
	CONSTRAINT uq_exercise_set UNIQUE (workout_exercise_id, set_type, set_number),
	CONSTRAINT ck_set_values CHECK (weight >= 0 AND set_number > 0 AND reps >= 0 AND (reps > 0 OR seconds > 0 OR meters > 0)),
	FOREIGN KEY(workout_exercise_id) REFERENCES workout_exercise (id)
);
INSERT INTO set_log_new (id, workout_exercise_id, set_number, set_type, weight, reps, rir_optional, logged_at, rest_started_at, rest_due_at, seconds)
	SELECT id, workout_exercise_id, set_number, set_type, weight, reps, rir_optional, logged_at, rest_started_at, rest_due_at, seconds FROM set_log;
DROP TABLE set_log;
ALTER TABLE set_log_new RENAME TO set_log;

-- On an assisted machine the weight is help, so it progresses downwards. Saved plans with assisted
-- exercises (by catalog picture or name) get load "assisted", or the next target would get easier.
UPDATE workout_template SET definition_json = json_set(definition_json,
	'$.exercises', (SELECT json_group_array(CASE
		WHEN json_extract(value, '$.load') IS NULL AND (lower(json_extract(value, '$.name')) LIKE '%assisted%'
			OR lower(coalesce(json_extract(value, '$.guide_id'), '')) LIKE '%assisted%')
		THEN json_set(value, '$.load', 'assisted') ELSE json(value) END)
		FROM (SELECT value FROM json_each(definition_json, '$.exercises') ORDER BY key)),
	'$.alternatives', (SELECT json_group_array(CASE
		WHEN json_extract(value, '$.load') IS NULL AND (lower(json_extract(value, '$.name')) LIKE '%assisted%'
			OR lower(coalesce(json_extract(value, '$.guide_id'), '')) LIKE '%assisted%')
		THEN json_set(value, '$.load', 'assisted') ELSE json(value) END)
		FROM (SELECT value FROM json_each(definition_json, '$.alternatives') ORDER BY key)))
WHERE lower(definition_json) LIKE '%assisted%';
