"""Preserve unknown crowd normalization/freshness; freeze feedback features."""
from alembic import op
import sqlalchemy as sa

revision = "6a40f1a22b01"
down_revision = "9e3c170a6210"
branch_labels = None
depends_on = None


def upgrade():
    with op.batch_alter_table("crowd_observation") as batch:
        batch.alter_column("normalized_value", existing_type=sa.Float(), nullable=True)
        batch.alter_column("freshness_seconds", existing_type=sa.Integer(), nullable=True)
    op.add_column("crowd_feedback", sa.Column("features_json", sa.JSON(), nullable=False, server_default="{}"))
    op.create_table("crowd_source_state",
        sa.Column("source", sa.String(), primary_key=True),
        sa.Column("provider_id", sa.String(), nullable=False),
        sa.Column("last_success_at", sa.DateTime(), nullable=True),
        sa.Column("last_failure_at", sa.DateTime(), nullable=True),
        sa.Column("last_error_code", sa.String(), nullable=True),
        sa.Column("last_change_at", sa.DateTime(), nullable=True),
        sa.Column("last_raw_value", sa.Double(), nullable=True),
        sa.Column("consecutive_failures", sa.Integer(), nullable=False),
        sa.Column("reliability", sa.Double(), nullable=False),
        sa.Column("evidence_count", sa.Integer(), nullable=False))


def downgrade():
    # Do not coerce unknown values into invented occupancy/freshness on downgrade.
    connection = op.get_bind()
    unknown = connection.execute(sa.text("SELECT count(*) FROM crowd_observation WHERE normalized_value IS NULL OR freshness_seconds IS NULL")).scalar()
    if unknown:
        raise ValueError("Cannot downgrade while observations contain unknown normalization/freshness")
    op.drop_table("crowd_source_state")
    op.drop_column("crowd_feedback", "features_json")
    with op.batch_alter_table("crowd_observation") as batch:
        batch.alter_column("normalized_value", existing_type=sa.Float(), nullable=False)
        batch.alter_column("freshness_seconds", existing_type=sa.Integer(), nullable=False)
