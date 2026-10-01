"""Alembic entrypoint; SQLite table rebuilds need connection-local FK pause."""
from alembic import context
from gymclaw.models import Base


def run(connection):
    context.configure(connection=connection, target_metadata=Base.metadata, render_as_batch=True)
    with context.begin_transaction():
        context.run_migrations()


connection = context.config.attributes.get("connection")
if connection is not None:
    # db.initialize owns transaction, FK pause and post-migration validation.
    run(connection)
else:
    from gymclaw.db import make_engine
    engine = make_engine()
    try:
        with engine.connect() as connection:
            connection.exec_driver_sql("PRAGMA foreign_keys=OFF")
            connection.commit()
            try:
                with connection.begin():
                    run(connection)
                    if connection.exec_driver_sql("PRAGMA foreign_key_check").first():
                        raise ValueError("Migration produced invalid foreign keys")
            finally:
                connection.exec_driver_sql("PRAGMA foreign_keys=ON")
                connection.commit()
    finally:
        engine.dispose()
