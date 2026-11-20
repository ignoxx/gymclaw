-- Remove historical Alembic marker table; PRAGMA user_version is the sole migration counter.
DROP TABLE IF EXISTS alembic_version;
