-- The agent-writable layer above the core (#92): playbooks, its own data collections and triggers,
-- each versioned so any change can be rolled back, plus the capability-gap log.
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

-- Every state an extension was ever in, newest version last; a rollback appends a copy of an old one.
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

-- Rows of the agent's own collections (creatine log, shoulder pain): written by data_add, read by sql_query.
CREATE TABLE agent_data (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	collection VARCHAR NOT NULL,
	at DATETIME NOT NULL,
	data_json JSON NOT NULL,
	created_at DATETIME NOT NULL
);
CREATE INDEX ix_agent_data_collection_at ON agent_data (collection, at);

-- Requests no tool could handle, for the developer to review; never acted on automatically.
CREATE TABLE capability_gap (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	at DATETIME NOT NULL,
	source VARCHAR NOT NULL CHECK (source IN ('agent', 'step_cap', 'deadline', 'repeated_call')),
	asked TEXT NOT NULL,
	tried TEXT NOT NULL,
	why TEXT NOT NULL,
	run_id VARCHAR
);
