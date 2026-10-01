"""SQLite connections and versioned schema initialization."""
import os
from pathlib import Path

from alembic import command
from alembic.config import Config
from sqlalchemy import create_engine, event


def make_engine(url: str | None = None):
    url = url or os.environ.get("GYMCLAW_DB_URL", "sqlite:///data/gymclaw.db")
    if not url.startswith("sqlite:"):
        raise ValueError("GymClaw requires SQLite")
    from sqlalchemy.engine import make_url
    database = make_url(url).database
    if database and database != ":memory:":
        Path(database).parent.mkdir(parents=True, exist_ok=True)
    engine = create_engine(url)

    @event.listens_for(engine, "connect")
    def configure(connection, _):
        connection.execute("PRAGMA foreign_keys=ON")
        connection.execute("PRAGMA busy_timeout=5000")

    return engine


def initialize(engine):
    config = Config()
    config.set_main_option("script_location", str(Path(__file__).parent / "migrations"))
    with engine.connect() as connection:
        # SQLite batch migrations rebuild referenced tables. Disable FK enforcement
        # only on this connection, outside any transaction; verify before commit.
        connection.exec_driver_sql("PRAGMA foreign_keys=OFF")
        connection.commit()
        try:
            with connection.begin():
                config.attributes["connection"] = connection
                command.upgrade(config, "head")
                if connection.exec_driver_sql("PRAGMA foreign_key_check").first():
                    raise ValueError("Migration produced invalid foreign keys")
        finally:
            connection.exec_driver_sql("PRAGMA foreign_keys=ON")
            connection.commit()
