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
    for sql in ("", "-- just a comment", "DELETE FROM sample", "WITH x AS (SELECT 1) UPDATE sample SET value=0", "PRAGMA query_only=OFF",
                f"ATTACH DATABASE '{tmp_path / 'other.db'}' AS other", "SELECT load_extension('nope')", "SELECT 1; DELETE FROM sample"):
        with pytest.raises(DomainError):
            query(engine, sql)
    assert query(engine, "SELECT sum(value) FROM sample")["rows"] == [[6]]
    assert not (tmp_path / "other.db").exists()
    engine.dispose()


def test_sql_cli_and_schema_are_json_and_read_only(tmp_path, capsys):
    import json
    from gymclaw.cli import main
    url = f"sqlite:///{tmp_path / 'cli.db'}"
    assert main(['--db-url', url, 'db', 'init']) == 0
    capsys.readouterr()
    assert main(['--db-url', url, 'sql', "SELECT name FROM sqlite_schema WHERE type='table'", '--limit', '1']) == 0
    data = json.loads(capsys.readouterr().out)['data']
    assert data['columns'] == ['name'] and len(data['rows']) == 1 and data['truncated']
    assert main(['--db-url', url, 'sql', "SELECT x'0011', 1e999"]) == 0
    assert json.loads(capsys.readouterr().out)['data']['rows'] == [[{'hex': '0011'}, 'inf']]
    assert main(['--db-url', url, 'sql', 'DROP TABLE planned_session']) == 1
    assert json.loads(capsys.readouterr().out)['error']['code'] == 'SQL_QUERY_FAILED'
