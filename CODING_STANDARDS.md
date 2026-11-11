# Coding standards

Read during review. Each rule is a judgement call; mechanical rules live in CI
(`.github/workflows/tests.yml`) and `.githooks/`.

## Fix the cause, everywhere it occurs

A bug fix names the root cause and removes it at every site that shares it, not only the one in
the report. Search for siblings before approving.

Example: PR #21 stopped dashboard pills from wrapping. The cause was class names built from data
("Empty" → `.empty`) colliding with the page's own classes; PR #23 fixed that a day later.

## Owner complaints get the general capability

When a change answers an owner complaint about the agent ("it couldn't move my workout"), check
that the agent can now handle the whole kind of request, not just the example in the complaint.
A new narrow verb per complaint is a smell. The owner complained about refusals on 2026-10-05,
2026-10-09 and 2026-10-12; each fix added verbs, and the next request still fell through.

## The agent is the brain

The agent makes GymClaw's decisions; code provides mechanics, data and raw actions. Code keeps the
deterministic hot paths: workout taps, set logging, rest timers, reminders, calendar sync,
idempotency, validation, slot timing and crowd. Planning decisions, which workout goes in which
slot, and exceptions belong to the agent. Rules are defaults plus warnings, never refusals or
silent rewrites; when code can't handle something, it hands it to the agent instead of returning an
error. Training preferences belong in the agent's context, not in code: ask "would this rule be
wrong for a different user?", and if yes, it is context.

Example: on 2026-11-10 a "weekly workouts take the week's last slot" rule was built in code and
rejected. PR #80 stores the plan in the owner's words and lets the agent decide.

## No valueless messages

Message the owner only with new, actionable information; silence is the default, and a wake with
nothing changed leaves the model asleep. Fix it at the wake or emit source; an output filter is only
a backstop.

Example: on 2026-10-26 background wakes sent the owner bare "..." messages.
