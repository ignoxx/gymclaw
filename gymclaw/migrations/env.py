from alembic import context
from gymclaw.models import Base

connection = context.config.attributes.get("connection")
if connection is None:
    from gymclaw.db import make_engine
    with make_engine().connect() as connection:
        context.configure(connection=connection, target_metadata=Base.metadata, render_as_batch=True)
        with context.begin_transaction():
            context.run_migrations()
else:
    context.configure(connection=connection, target_metadata=Base.metadata, render_as_batch=True)
    with context.begin_transaction():
        context.run_migrations()
