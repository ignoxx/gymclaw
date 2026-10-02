"""Onboarding interview answers and the ordered workout split (rotation)."""
import json

from alembic import op
import sqlalchemy as sa

revision = "7b3e9d2c4f10"
down_revision = "d9012ca783be"
branch_labels = None
depends_on = None


def upgrade():
    with op.batch_alter_table("onboarding_state") as batch:
        batch.add_column(sa.Column("interview_json", sa.JSON(), nullable=False, server_default="{}"))
        batch.add_column(sa.Column("split_json", sa.JSON(), nullable=False, server_default="[]"))
    # Keep what the owner already told us; everything else is asked in the interview.
    connection = op.get_bind()
    row = connection.execute(sa.text("SELECT goal, template_id FROM onboarding_state WHERE id = 1")).first()
    if row:
        connection.execute(sa.text("UPDATE onboarding_state SET interview_json = :interview, split_json = :split WHERE id = 1"),
            {"interview": json.dumps({"goal": row.goal} if row.goal else {}), "split": json.dumps([row.template_id] if row.template_id else [])})


def downgrade():
    with op.batch_alter_table("onboarding_state") as batch:
        batch.drop_column("split_json")
        batch.drop_column("interview_json")
