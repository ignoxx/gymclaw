"""Persistent user travel/illness/unavailability, without private event bodies."""
from alembic import op
import sqlalchemy as sa

revision = "c60eb28084e2"
down_revision = "ab928e603fd0"
branch_labels = None
depends_on = None


def upgrade():
    op.create_table("availability_block",
        sa.Column("id", sa.String(), primary_key=True),
        sa.Column("start_at", sa.DateTime(), nullable=False),
        sa.Column("end_at", sa.DateTime(), nullable=False),
        sa.Column("kind", sa.String(), nullable=False),
        sa.Column("status", sa.String(), nullable=False),
        sa.Column("created_at", sa.DateTime(), nullable=False),
        sa.Column("retracted_at", sa.DateTime(), nullable=True),
        sa.CheckConstraint("end_at > start_at"),
        sa.CheckConstraint("kind IN ('TRAVEL','SICK','UNAVAILABLE')"),
        sa.CheckConstraint("status IN ('ACTIVE','RETRACTED')"))
    op.create_index("ix_availability_block_start_at", "availability_block", ["start_at"])


def downgrade():
    op.drop_index("ix_availability_block_start_at", table_name="availability_block")
    op.drop_table("availability_block")
