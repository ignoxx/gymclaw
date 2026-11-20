-- Secret token for the read-only /calendar/<token>.ics subscription feed.
ALTER TABLE user_profile ADD COLUMN ics_token VARCHAR;
