"""Calendar sync, OAuth state, notification ownership and write outbox."""
from alembic import op
import sqlalchemy as sa

revision = "0c816b552051"
down_revision = "f546f352a874"
branch_labels = None
depends_on = None


def upgrade():
    op.add_column("planned_session", sa.Column("workout_plan_json", sa.JSON(), nullable=False, server_default="{}"))
    with op.batch_alter_table("notification_job", naming_convention={"ck": "ck_%(table_name)s_legacy"}) as batch:
        batch.alter_column("workout_session_id", existing_type=sa.String(), nullable=True)
        batch.add_column(sa.Column("planned_session_id", sa.String(), nullable=True))
        batch.create_foreign_key("fk_notification_plan", "planned_session", ["planned_session_id"], ["id"])
        batch.create_check_constraint("ck_notification_owner", "(kind = 'REST' AND workout_session_id IS NOT NULL AND planned_session_id IS NULL) OR (kind IN ('GET_READY','LEAVE','SESSION_START') AND planned_session_id IS NOT NULL AND workout_session_id IS NULL)")
    op.create_table("calendar_sync_state",
        sa.Column("id", sa.Integer(), primary_key=True),
        sa.Column("calendar_id", sa.String(), nullable=False),
        sa.Column("timezone", sa.String(), nullable=False),
        sa.Column("sync_token", sa.String(), nullable=True),
        sa.Column("last_synced_at", sa.DateTime(), nullable=True),
        sa.Column("source", sa.String(), nullable=False),
        sa.CheckConstraint("id = 1"))
    op.create_table("calendar_credential",
        sa.Column("id", sa.Integer(), primary_key=True),
        sa.Column("credentials_json", sa.JSON(), nullable=False),
        sa.CheckConstraint("id = 1"))
    op.create_table("calendar_write",
        sa.Column("id", sa.String(), primary_key=True),
        sa.Column("planned_session_id", sa.String(), sa.ForeignKey("planned_session.id"), nullable=False),
        sa.Column("revision", sa.Integer(), nullable=False),
        sa.Column("action", sa.String(), nullable=False),
        sa.Column("event_id", sa.String(), nullable=False),
        sa.Column("expected_etag", sa.String(), nullable=True),
        sa.Column("body_json", sa.JSON(), nullable=False),
        sa.Column("status", sa.String(), nullable=False),
        sa.Column("created_at", sa.DateTime(), nullable=False),
        sa.Column("handled_at", sa.DateTime(), nullable=True),
        sa.UniqueConstraint("planned_session_id", "revision", "action"),
        sa.CheckConstraint("status IN ('PENDING','APPLIED','CONFLICT','CANCELLED')"),
        sa.CheckConstraint("action IN ('CREATE','UPDATE','DELETE')"))


def downgrade():
    op.drop_table("calendar_write")
    op.drop_table("calendar_credential")
    op.drop_table("calendar_sync_state")
    with op.batch_alter_table("notification_job") as batch:
        batch.drop_constraint("ck_notification_owner", type_="check")
        batch.drop_constraint("fk_notification_plan", type_="foreignkey")
        batch.drop_column("planned_session_id")
        batch.alter_column("workout_session_id", existing_type=sa.String(), nullable=False)
    op.drop_column("planned_session", "workout_plan_json")
