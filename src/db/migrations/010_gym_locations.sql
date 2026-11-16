-- Saved training places ("home: dumbbells up to 24 kg, pull-up bar"), so a workout can be rebuilt for
-- the equipment there by name. equipment_json: catalog equipment names; notes: the owner's words.
CREATE TABLE gym_location (
	name VARCHAR NOT NULL,
	equipment_json JSON NOT NULL,
	notes TEXT,
	updated_at DATETIME NOT NULL,
	PRIMARY KEY (name)
);
