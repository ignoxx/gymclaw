"""Reduce lowest-priority volume first; preserve first primary and prerequisites."""
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import Profile
from gymclaw.services.templates import Template


def compress_template(template: Template, profile: Profile, budget_seconds: int) -> dict:
    counts = {e.id: e.working_sets for e in template.exercises}
    first_primary = next((e.id for e in template.exercises if e.primary), None)
    protected = {first_primary} if first_primary else set()
    for spec in reversed(template.exercises):
        if spec.id in protected:
            protected.update(spec.requires_completed)

    def duration():
        kept = [e for e in template.exercises if counts[e.id]]
        seconds = template.set_duration_seconds if first_primary and counts[first_primary] else 0
        for index, spec in enumerate(kept):
            rest = spec.rest_seconds or (profile.default_compound_rest_seconds if spec.primary else profile.default_accessory_rest_seconds)
            seconds += counts[spec.id] * template.set_duration_seconds + max(0, counts[spec.id] - 1) * rest
            if index < len(kept) - 1:
                seconds += rest + template.transition_seconds
        return seconds

    while duration() > budget_seconds:
        kept = [e for e in template.exercises if counts[e.id]]
        choices = [e for e in kept if not (e.id in protected and counts[e.id] == 1) and not (len(kept) == 1 and counts[e.id] == 1)]
        if not choices:
            raise DomainError("WORKOUT_SLOT_TOO_SHORT", "Edited slot cannot fit required warm-up and primary movement; event kept, explicit adjustment needed")
        lowest = min(choices, key=lambda e: (e.priority, -template.exercises.index(e)))
        counts[lowest.id] -= 1
        # Removing a prerequisite cannot silently create an invalid execution order.
        for spec in template.exercises:
            if any(counts[required] == 0 for required in spec.requires_completed):
                counts[spec.id] = 0
    return {"template": template.model_dump(mode="json"), "sets": counts, "budget_seconds": budget_seconds, "estimated_duration_seconds": duration(), "reduced": [{"exercise_id": e.id, "from_sets": e.working_sets, "to_sets": counts[e.id]} for e in template.exercises if counts[e.id] != e.working_sets]}
