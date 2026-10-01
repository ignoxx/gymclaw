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
    with engine.begin() as connection:
        config.attributes["connection"] = connection
        command.upgrade(config, "head")
