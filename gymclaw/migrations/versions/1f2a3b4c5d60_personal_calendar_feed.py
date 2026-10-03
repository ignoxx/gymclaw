"""Read-only personal ICS feed and its expanded busy intervals."""
from alembic import op
import sqlalchemy as sa

revision = "1f2a3b4c5d60"
down_revision = "8c1d2e3f4a50"
branch_labels = None
depends_on = None


def upgrade():
    op.create_table("personal_calendar_state",
        sa.Column("id", sa.Integer(), primary_key=True),
        sa.Column("url", sa.String(), nullable=False),
        sa.Column("fetched_at", sa.DateTime(), nullable=True),
        sa.Column("last_success_at", sa.DateTime(), nullable=True),
        sa.Column("last_error_code", sa.String(), nullable=True),
        sa.CheckConstraint("id = 1"))
    op.create_table("personal_busy_interval",
        sa.Column("id", sa.Integer(), primary_key=True, autoincrement=True),
        sa.Column("start_at", sa.DateTime(), nullable=False),
        sa.Column("end_at", sa.DateTime(), nullable=False),
        sa.CheckConstraint("end_at > start_at"))
    op.create_index("ix_personal_busy_interval_start_at", "personal_busy_interval", ["start_at"])


def downgrade():
    op.drop_index("ix_personal_busy_interval_start_at", table_name="personal_busy_interval")
    op.drop_table("personal_busy_interval")
    op.drop_table("personal_calendar_state")
