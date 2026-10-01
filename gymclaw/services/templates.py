"""Validated template import. Definitions are stored in SQLite, snapshotted at start."""
from typing import Annotated

from pydantic import BaseModel, ConfigDict, Field, model_validator
from sqlalchemy.orm import Session

from gymclaw.models import WorkoutTemplate

PositiveInt = Annotated[int, Field(gt=0, strict=True)]
Weight = Annotated[float, Field(ge=0, allow_inf_nan=False)]
Identifier = Annotated[str, Field(min_length=1, max_length=100)]


class ExerciseSpec(BaseModel):
    model_config = ConfigDict(extra="forbid")
    id: Identifier
    name: Identifier
    role: Identifier
    primary: bool = False
    working_sets: PositiveInt = 3
    rep_min: PositiveInt = 8
    rep_max: PositiveInt = 10
    target_weight: Weight
    increment: Annotated[float, Field(gt=0, allow_inf_nan=False)] = 2.5
    rest_seconds: PositiveInt | None = None
    warmup_weight: Weight | None = None
    warmup_reps: PositiveInt = 8
    priority: Annotated[int, Field(ge=0, strict=True)] = 0
    requires_completed: list[Identifier] = []
    substitutes: list[Identifier] = []

    @model_validator(mode="after")
    def valid(self):
        if self.rep_min > self.rep_max:
            raise ValueError("Rep minimum exceeds maximum")
        return self


class Template(BaseModel):
    model_config = ConfigDict(extra="forbid")
    id: Identifier
    name: Identifier
    exercises: Annotated[list[ExerciseSpec], Field(min_length=1)]
    alternatives: list[ExerciseSpec] = []
    set_duration_seconds: PositiveInt = 40
    transition_seconds: Annotated[int, Field(ge=0, strict=True)] = 60
    source: str = "user"

    @model_validator(mode="after")
    def valid(self):
        all_specs = self.exercises + self.alternatives
        specs = {s.id: s for s in all_specs}
        if len(specs) != len(all_specs):
            raise ValueError("Exercise IDs must be unique within template")
        seen = set()
        for spec in self.exercises:
            if not set(spec.requires_completed) <= seen:
                raise ValueError("Dependencies must reference earlier planned exercises")
            seen.add(spec.id)
        planned_ids = {s.id for s in self.exercises}
        for alternative in self.alternatives:
            if not set(alternative.requires_completed) <= planned_ids:
                raise ValueError("Alternative dependencies must reference planned exercises")
        for spec in all_specs:
            for substitute in spec.substitutes:
                alternative = next((s for s in self.alternatives if s.id == substitute), None)
                if alternative is None or alternative.role != spec.role or substitute == spec.id:
                    raise ValueError("Substitutes must reference same-role alternatives")
        first_primary = next((s for s in self.exercises if s.primary), None)
        if first_primary and first_primary.warmup_weight is None:
            raise ValueError("First primary exercise requires explicit warmup_weight")
        return self


def import_template(db: Session, template: Template) -> dict:
    row = db.get(WorkoutTemplate, template.id)
    if row is None:
        row = WorkoutTemplate(id=template.id)
        db.add(row)
    row.definition_json = template.model_dump(mode="json")
    db.flush()
    return row.definition_json


def get_template(db: Session, template_id: str) -> Template:
    row = db.get(WorkoutTemplate, template_id)
    if row is None:
        raise ValueError(f"Unknown workout template: {template_id}")
    return Template.model_validate(row.definition_json)
