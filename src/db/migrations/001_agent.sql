-- The Rust runtime's own state: chat history the agent sees, its notes, and agent button payloads.
CREATE TABLE chat_message (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	at DATETIME NOT NULL,
	role VARCHAR NOT NULL CHECK (role IN ('owner', 'assistant', 'bot')),
	text TEXT NOT NULL,
	telegram_message_id INTEGER,
	meta_json JSON NOT NULL DEFAULT '{}'
);
CREATE INDEX ix_chat_message_at ON chat_message (at);

-- Owner preferences (was USER.md) and lessons the agent wrote for itself; both go into its prompt.
CREATE TABLE agent_note (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	kind VARCHAR NOT NULL CHECK (kind IN ('owner', 'lesson')),
	text TEXT NOT NULL,
	created_at DATETIME NOT NULL,
	updated_at DATETIME NOT NULL
);

-- Telegram callback data is capped at 64 bytes; agent buttons carry `btn:<id>` and the value lives here.
CREATE TABLE agent_button (
	id VARCHAR PRIMARY KEY,
	label VARCHAR NOT NULL,
	value TEXT NOT NULL,
	created_at DATETIME NOT NULL
);
