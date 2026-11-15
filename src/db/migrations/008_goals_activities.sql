-- Goals (#95): the owner's words, an optional measurable target, and the agent's current training block.
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

-- Training outside the gym (#98): a run, a match, a hike. `weekly` repeats it at the same local weekday and
-- time until `until_at`; `removed_at` marks one logged by mistake.
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

-- Lift trends on demand (#94): each exercise of each finished workout with its working sets (reps, in
-- order) and the best estimated 1-rep max (Epley: weight × (1 + reps / 30)). Same formula as
-- `domain::trends::e1rm`; bodyweight lifts have e1rm 0, their reps are the trend.
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
