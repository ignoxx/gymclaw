"""Bounded SQL reads on a separate, read-only SQLite connection."""
from contextlib import closing
from pathlib import Path
import sqlite3
import math
from time import monotonic

from gymclaw.services.errors import DomainError


def query(engine, statement: str, *, limit: int = 200) -> dict:
    if not 1 <= limit <= 1000:
        raise ValueError("--limit must be between 1 and 1000")
    database = engine.url.database
    if not database or database == ":memory:":
        raise ValueError("SQL reads require a file-backed database")
    # mode=ro cannot be disabled by SQL. The authorizer additionally prevents ATTACH,
    # PRAGMA changes, transactions and extension loading, including inside a WITH query.
    try:
        with closing(sqlite3.connect(Path(database).resolve().as_uri() + "?mode=ro", uri=True)) as connection:
            connection.execute("PRAGMA query_only=ON")
            connection.setlimit(sqlite3.SQLITE_LIMIT_LENGTH, 1_000_000)
            allowed = {sqlite3.SQLITE_SELECT, sqlite3.SQLITE_READ, sqlite3.SQLITE_FUNCTION, sqlite3.SQLITE_RECURSIVE}
            connection.set_authorizer(lambda action, arg1, arg2, *_: sqlite3.SQLITE_OK
                if action in allowed and not (action == sqlite3.SQLITE_FUNCTION and arg2 == "load_extension") else sqlite3.SQLITE_DENY)
            deadline = monotonic() + 2
            connection.set_progress_handler(lambda: int(monotonic() > deadline), 1000)
            cursor = connection.execute(statement)
            if cursor.description is None:
                raise DomainError("SQL_QUERY_FAILED", "Expected a query returning rows")
            rows = cursor.fetchmany(limit + 1)
            def cell(value):
                if isinstance(value, bytes):
                    return {"hex": value.hex()}
                if isinstance(value, float) and not math.isfinite(value):
                    return str(value)
                return value
            return {"columns": [column[0] for column in cursor.description], "rows": [[cell(value) for value in row] for row in rows[:limit]], "truncated": len(rows) > limit}
    except sqlite3.Error as error:
        raise DomainError("SQL_QUERY_FAILED", str(error)) from error
