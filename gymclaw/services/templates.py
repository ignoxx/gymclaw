"""Validated template import. Definitions are stored in SQLite, snapshotted at start."""
from typing import Annotated

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator
from sqlalchemy.orm import Session

from gymclaw.models import WorkoutTemplate
from gymclaw.services.errors import DomainError

PositiveInt = Annotated[int, Field(gt=0, strict=True)]
Weight = Annotated[float, Field(ge=0, allow_inf_nan=False)]
Identifier = Annotated[str, Field(min_length=1, max_length=100)]


class ExerciseSpec(BaseModel):
    model_config = ConfigDict(extra="forbid")
    id: Identifier
    name: Identifier
    role: Identifier
    guide_id: Identifier | None = None
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

    @field_validator("guide_id")
    @classmethod
    def known_guide(cls, value: str | None) -> str | None:
        from gymclaw.services.illustrations import catalog
        if value is not None and value not in catalog():
            raise ValueError("guide_id must match Workout Guide catalog")
        return value

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


def require_illustrations(template: Template) -> Template:
    """Owner templates (CLI import) need a catalog guide_id on every exercise, so every card has an image."""
    missing = [spec.name for spec in template.exercises + template.alternatives if spec.guide_id is None]
    if missing:
        raise DomainError("ILLUSTRATION_REQUIRED", "Set guide_id (closest match from `catalog search`) for: " + ", ".join(missing))
    return template


def import_template(db: Session, template: Template) -> dict:
    row = db.get(WorkoutTemplate, template.id)
    if row is None:
        row = WorkoutTemplate(id=template.id)
        db.add(row)
    row.definition_json = template.model_dump(mode="json")
    db.flush()
    return row.definition_json


def remember_swap(db: Session, template_id: str, original_id: str, replacement: ExerciseSpec) -> bool:
    """Owner swapped an exercise mid-workout: the replacement takes its place in the saved template, and
    the original stays as an alternative so it's one swap away. False if the template can't take it."""
    row = db.get(WorkoutTemplate, template_id)
    if row is None:
        return False
    definition = row.definition_json
    index = next((i for i, e in enumerate(definition["exercises"]) if e["id"] == original_id), None)
    if index is None:
        return False
    original = definition["exercises"][index]
    keep = {k: original[k] for k in ("role", "primary", "working_sets", "rep_min", "rep_max", "rest_seconds", "priority", "requires_completed")}
    new = replacement.model_dump(mode="json") | keep | {"substitutes": [original_id]}
    alternatives = [a for a in definition.get("alternatives", []) if a["id"] not in {replacement.id, original_id}]
    alternatives.append(original | {"substitutes": [], "requires_completed": []})
    exercises = [e | {"requires_completed": [replacement.id if r == original_id else r for r in e["requires_completed"]],
        "substitutes": [s for s in e["substitutes"] if s != replacement.id]} for e in definition["exercises"]]
    exercises[index] = new
    try:
        template = Template.model_validate(definition | {"exercises": exercises, "alternatives": alternatives})
    except ValueError:
        return False
    import_template(db, template)
    return True


def get_template(db: Session, template_id: str) -> Template:
    row = db.get(WorkoutTemplate, template_id)
    if row is None:
        raise ValueError(f"Unknown workout template: {template_id}")
    return Template.model_validate(row.definition_json)


def edit_exercise(db: Session, template_id: str, exercise: str, *, changes: dict | None = None, add: bool = False, remove: bool = False, position: int | None = None) -> dict:
    """Change one exercise of a saved plan without re-importing it. `exercise` is an ID, guide_id or
    name. Edit: `changes` are ExerciseSpec fields (e.g. target_weight, working_sets, rep_min); a new
    target_weight also resets the learned next weight. Add: `exercise` is a catalog guide_id or name,
    `changes` override the defaults, `position` is its 0-based place (default last). Remove: drops it
    and any dependency on it. Upcoming planned sessions pick the change up via `refresh_plans`."""
    from gymclaw.models import ExerciseProgression
    from gymclaw.services.illustrations import catalog, search
    definition = get_template(db, template_id).model_dump(mode="json")
    rows = definition["exercises"]
    key = exercise.strip().casefold()
    index = next((i for i, e in enumerate(rows) if key in {e["id"].casefold(), e["name"].casefold(), (e.get("guide_id") or "").casefold()}), None)
    changes = changes or {}
    if add:
        item = catalog().get(key) or next((i for i in catalog().values() if i["name"].casefold() == key), None)
        if item is None:
            options = ", ".join(f"{o['guide_id']} ({o['name']})" for o in search(exercise, limit=5))
            raise DomainError("EXERCISE_UNKNOWN", f"No exact match for '{exercise}'. Pass one guide_id: {options or 'try catalog search'}")
        if index is not None:
            raise DomainError("EXERCISE_EXISTS", f"{rows[index]['name']} is already in {definition['name']}; edit it instead")
        spec = {"id": item["slug"], "name": item["name"], "role": "accessory", "guide_id": item["slug"], "target_weight": 0} | changes
        rows.insert(len(rows) if position is None else position, spec)
    else:
        if index is None:
            names = ", ".join(e["name"] for e in rows)
            raise DomainError("EXERCISE_NOT_FOUND", f"'{exercise}' isn't in {definition['name']} ({names})")
        spec = rows[index]
        if remove:
            rows.pop(index)
            for row in rows:
                row["requires_completed"] = [r for r in row["requires_completed"] if r != spec["id"]]
        else:
            rows[index] = spec = spec | changes
            if position is not None:
                rows.insert(position, rows.pop(index))
            progression = db.get(ExerciseProgression, spec["id"])
            if "target_weight" in changes and progression is not None:
                progression.next_weight = changes["target_weight"]
    return import_template(db, Template.model_validate(definition))
