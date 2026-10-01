"""Explicit runtime authority and event-based proactive message outbox."""
from alembic import op
import sqlalchemy as sa

revision = "ab928e603fd0"
down_revision = "6a40f1a22b01"
branch_labels = None
depends_on = None


def upgrade():
    with op.batch_alter_table("notification_delivery", naming_convention={"ck": "ck_delivery_status"}) as batch:
        batch.alter_column("notification_job_id", existing_type=sa.String(), nullable=True)
        batch.add_column(sa.Column("agent_event_id", sa.String(), nullable=True))
        batch.create_foreign_key("fk_delivery_event", "agent_event", ["agent_event_id"], ["id"])
        batch.create_unique_constraint("uq_delivery_event", ["agent_event_id"])
        batch.create_check_constraint("ck_delivery_owner", "(notification_job_id IS NOT NULL AND agent_event_id IS NULL) OR (notification_job_id IS NULL AND agent_event_id IS NOT NULL)")
    op.create_table("runtime_settings",
        sa.Column("id", sa.Integer(), primary_key=True),
        sa.Column("profile", sa.String(), nullable=False),
        sa.Column("recipient", sa.String(), nullable=False),
        sa.Column("project_root", sa.String(), nullable=False),
        sa.Column("python_path", sa.String(), nullable=False),
        sa.Column("template_id", sa.String(), sa.ForeignKey("workout_template.id"), nullable=True),
        sa.Column("enabled", sa.Boolean(), nullable=False),
        sa.Column("calendar_writes_enabled", sa.Boolean(), nullable=False),
        sa.Column("crowd_polling_enabled", sa.Boolean(), nullable=False),
        sa.CheckConstraint("id = 1"))


def downgrade():
    if op.get_bind().execute(sa.text("SELECT count(*) FROM notification_delivery WHERE agent_event_id IS NOT NULL")).scalar():
        raise ValueError("Cannot downgrade while event delivery history exists")
    op.drop_table("runtime_settings")
    with op.batch_alter_table("notification_delivery") as batch:
        batch.drop_constraint("ck_delivery_owner", type_="check")
        batch.drop_constraint("uq_delivery_event", type_="unique")
        batch.drop_constraint("fk_delivery_event", type_="foreignkey")
        batch.drop_column("agent_event_id")
        batch.alter_column("notification_job_id", existing_type=sa.String(), nullable=False)
