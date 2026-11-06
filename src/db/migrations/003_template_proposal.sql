-- Template change proposals the agent sends and the owner accepts or declines one line at a time.
-- Nothing in `workout_template` changes until a line is ACCEPTED; DECLINED lines suppress re-proposals.
CREATE TABLE template_proposal (
	id VARCHAR PRIMARY KEY,
	batch_id VARCHAR NOT NULL,
	template_id VARCHAR NOT NULL,
	exercise VARCHAR NOT NULL,
	changes_json JSON NOT NULL,
	position INTEGER,
	summary TEXT NOT NULL,
	status VARCHAR NOT NULL DEFAULT 'PENDING' CHECK (status IN ('PENDING', 'ACCEPTED', 'DECLINED')),
	created_at DATETIME NOT NULL,
	decided_at DATETIME
);
CREATE INDEX ix_template_proposal_batch ON template_proposal (batch_id);
CREATE INDEX ix_template_proposal_exercise ON template_proposal (template_id, exercise, status);
