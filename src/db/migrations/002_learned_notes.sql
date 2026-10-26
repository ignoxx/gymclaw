-- The second kind of note is now what the agent learned from the owner's behaviour (weekly review),
-- not tool workarounds. SQLite can't alter a CHECK, so rebuild the table; ids are kept.
CREATE TABLE agent_note_new (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	kind VARCHAR NOT NULL CHECK (kind IN ('owner', 'learned')),
	text TEXT NOT NULL,
	created_at DATETIME NOT NULL,
	updated_at DATETIME NOT NULL
);
INSERT INTO agent_note_new (id, kind, text, created_at, updated_at)
	SELECT id, CASE kind WHEN 'lesson' THEN 'learned' ELSE kind END, text, created_at, updated_at FROM agent_note;
DROP TABLE agent_note;
ALTER TABLE agent_note_new RENAME TO agent_note;
