"""Resumable owner-confirmed onboarding, independent of runtime authority."""
from alembic import op
import sqlalchemy as sa

revision = "d9012ca783be"
down_revision = "c60eb28084e2"
branch_labels = None
depends_on = None


def upgrade():
    op.create_table("onboarding_state",
        sa.Column("id", sa.Integer(), primary_key=True),
        sa.Column("goal", sa.String(), nullable=True),
        sa.Column("profile_fingerprint", sa.String(), nullable=True),
        sa.Column("template_id", sa.String(), sa.ForeignKey("workout_template.id"), nullable=True),
        sa.Column("template_fingerprint", sa.String(), nullable=True),
        sa.Column("completed_fingerprint", sa.String(), nullable=True),
        sa.CheckConstraint("id = 1"))


def downgrade():
    op.drop_table("onboarding_state")
