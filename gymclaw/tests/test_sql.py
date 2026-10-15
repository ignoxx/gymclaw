import sqlite3

import pytest

from gymclaw.db import make_engine
from gymclaw.services.errors import DomainError
from gymclaw.services.sql import query


def test_bounded_read_and_connection_level_write_protection(tmp_path):
    path = tmp_path / "read.db"
    with sqlite3.connect(path) as db:
        db.execute("CREATE TABLE sample (value INTEGER)")
        db.executemany("INSERT INTO sample VALUES (?)", [(1,), (2,), (3,)])
    engine = make_engine(f"sqlite:///{path}")
    assert query(engine, "WITH x AS (SELECT * FROM sample) SELECT value FROM x ORDER BY value", limit=2) == {
        "columns": ["value"], "rows": [[1], [2]], "truncated": True}
    for sql in ("DELETE FROM sample", "WITH x AS (SELECT 1) UPDATE sample SET value=0", "PRAGMA query_only=OFF",
                f"ATTACH DATABASE '{tmp_path / 'other.db'}' AS other", "SELECT load_extension('nope')", "SELECT 1; DELETE FROM sample"):
        with pytest.raises(DomainError):
            query(engine, sql)
    assert query(engine, "SELECT sum(value) FROM sample")["rows"] == [[6]]
    assert not (tmp_path / "other.db").exists()
    engine.dispose()
