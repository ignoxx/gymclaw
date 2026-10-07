"""Mark workouts started from the reminder's Start button: real arrival times for habit learning."""
from alembic import op
import sqlalchemy as sa

revision = "3d5e7f9a1b20"
down_revision = "2b7c4d9e1a30"
branch_labels = None
depends_on = None


def upgrade():
    with op.batch_alter_table("workout_session") as batch:
        batch.add_column(sa.Column("started_by_button", sa.Boolean(), nullable=False, server_default=sa.false()))


def downgrade():
    with op.batch_alter_table("workout_session") as batch:
        batch.drop_column("started_by_button")
