"""Body weight log (scale photos and typed weigh-ins)."""
from alembic import op
import sqlalchemy as sa

revision = "2b7c4d9e1a30"
down_revision = "1f2a3b4c5d60"
branch_labels = None
depends_on = None


def upgrade():
    op.create_table("body_weight",
        sa.Column("id", sa.String(), primary_key=True),
        sa.Column("measured_at", sa.DateTime(), nullable=False, unique=True),
        sa.Column("kg", sa.Double(), nullable=False),
        sa.Column("created_at", sa.DateTime(), nullable=False),
        sa.CheckConstraint("kg >= 20 AND kg <= 400"))


def downgrade():
    op.drop_table("body_weight")
