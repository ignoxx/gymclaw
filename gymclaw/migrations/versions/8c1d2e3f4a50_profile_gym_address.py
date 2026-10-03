"""Gym address for calendar event locations (travel time / time to leave in calendar apps)."""
from alembic import op
import sqlalchemy as sa

revision = "8c1d2e3f4a50"
down_revision = "7b3e9d2c4f10"
branch_labels = None
depends_on = None


def upgrade():
    with op.batch_alter_table("user_profile") as batch:
        batch.add_column(sa.Column("gym_address", sa.String(), nullable=True))


def downgrade():
    with op.batch_alter_table("user_profile") as batch:
        batch.drop_column("gym_address")
