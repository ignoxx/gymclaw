"""Workout execution, snapshots, progression and durable rest outbox."""
from alembic import op
import sqlalchemy as sa

revision = "f546f352a874"
down_revision = "227579a5c228"
branch_labels = None
depends_on = None


def upgrade():
    op.create_table("workout_template",
        sa.Column("id", sa.String(), primary_key=True),
        sa.Column("definition_json", sa.JSON(), nullable=False),
        sa.Column("updated_at", sa.DateTime(), nullable=False))
    with op.batch_alter_table("workout_session") as batch:
        batch.add_column(sa.Column("template_id", sa.String(), nullable=True))
        batch.add_column(sa.Column("template_snapshot", sa.JSON(), nullable=False, server_default="{}"))
        batch.add_column(sa.Column("last_action_at", sa.DateTime(), nullable=True))
        batch.create_unique_constraint("uq_workout_planned_session", ["planned_session_id"])
        batch.create_foreign_key("fk_workout_template", "workout_template", ["template_id"], ["id"])
    with op.batch_alter_table("workout_exercise") as batch:
        batch.add_column(sa.Column("config_json", sa.JSON(), nullable=False, server_default="{}"))
    with op.batch_alter_table("set_log", naming_convention={"ck": "ck_%(table_name)s_legacy"}) as batch:
        batch.create_unique_constraint("uq_exercise_set", ["workout_exercise_id", "set_type", "set_number"])
        batch.create_check_constraint("ck_set_values", "weight >= 0 AND reps > 0 AND set_number > 0")
    op.create_table("exercise_progression",
        sa.Column("exercise_id", sa.String(), primary_key=True),
        sa.Column("next_weight", sa.Double(), nullable=False),
        sa.Column("updated_at", sa.DateTime(), nullable=False),
        sa.Column("source_workout_id", sa.String(), sa.ForeignKey("workout_session.id"), nullable=False))
    op.create_table("notification_job",
        sa.Column("id", sa.String(), primary_key=True),
        sa.Column("kind", sa.String(), nullable=False),
        sa.Column("workout_session_id", sa.String(), sa.ForeignKey("workout_session.id"), nullable=False),
        sa.Column("set_log_id", sa.String(), sa.ForeignKey("set_log.id"), unique=True),
        sa.Column("due_at", sa.DateTime(), nullable=False),
        sa.Column("status", sa.String(), nullable=False),
        sa.Column("payload_json", sa.JSON(), nullable=False),
        sa.Column("external_job_id", sa.String(), nullable=True),
        sa.Column("handled_at", sa.DateTime(), nullable=True),
        sa.CheckConstraint("status IN ('PENDING','CANCELLED','FIRED')"))
    op.create_index("ix_notification_job_due_at", "notification_job", ["due_at"])


def downgrade():
    op.drop_table("notification_job")
    op.drop_table("exercise_progression")
    with op.batch_alter_table("set_log") as batch:
        batch.drop_constraint("uq_exercise_set", type_="unique")
        batch.drop_constraint("ck_set_values", type_="check")
    with op.batch_alter_table("workout_exercise") as batch:
        batch.drop_column("config_json")
    with op.batch_alter_table("workout_session") as batch:
        batch.drop_constraint("fk_workout_template", type_="foreignkey")
        batch.drop_constraint("uq_workout_planned_session", type_="unique")
        batch.drop_column("last_action_at")
        batch.drop_column("template_snapshot")
        batch.drop_column("template_id")
    op.drop_table("workout_template")
