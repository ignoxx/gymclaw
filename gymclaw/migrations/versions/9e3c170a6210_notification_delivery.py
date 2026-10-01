"""Durable notification delivery separate from domain job execution."""
from alembic import op
import sqlalchemy as sa

revision = "9e3c170a6210"
down_revision = "0c816b552051"
branch_labels = None
depends_on = None


def upgrade():
    op.create_table("notification_delivery",
        sa.Column("id", sa.String(), primary_key=True),
        sa.Column("notification_job_id", sa.String(), sa.ForeignKey("notification_job.id"), nullable=False, unique=True),
        sa.Column("recipient", sa.String(), nullable=False),
        sa.Column("runtime_profile", sa.String(), nullable=False),
        sa.Column("message", sa.Text(), nullable=False),
        sa.Column("result_json", sa.JSON(), nullable=False),
        sa.Column("status", sa.String(), nullable=False),
        sa.Column("created_at", sa.DateTime(), nullable=False),
        sa.Column("handled_at", sa.DateTime(), nullable=True),
        sa.Column("external_message_id", sa.String(), nullable=True),
        sa.CheckConstraint("status IN ('PENDING','SENDING','SENT','UNKNOWN','CANCELLED')"))


def downgrade():
    op.drop_table("notification_delivery")
