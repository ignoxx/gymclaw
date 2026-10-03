---
name: gymclaw
description: Set up, plan and run the owner's training with GymClaw tools (onboarding, workouts, calendar, crowd).
user-invocable: true
---

# GymClaw tools

`../scripts/gymclaw-tool <group> <operation> …` → JSON `{ok, data, user_message_hint}`. Exit 1 = failed;
never continue as if it worked. Mutations need `--request-id`: one per owner action (e.g. `tg-<message id>-<op>`),
reused only for a retry of that same action.

## Replying

- Times are always the owner's local time (profile timezone), never UTC.

- React instead of replying when that's enough (message tool `react`, e.g. 👍).
- Choices go in buttons (message tool `presentation` with a `buttons` block), not typed lists.
- One short line. The owner wants quick, clean messages.

## Setup (first contact, or `onboarding status` not READY)

Read `onboarding status`. Ask `instruction` (one question). If the saved profile or USER.md already
answers it, confirm in one line instead of asking. Never treat defaults as answers.

- Save answers: `onboarding answer --answers '{"experience":"2 years"}'`. Schedule, session length,
  time window and travel answers must also pass the matching `--profile '{...}'` fields
  (status `missing`/errors list them; Monday=0, local times "HH:MM").
- PLAN: ask "send your plan, or should I build one?"
  - Photo: read it with your own vision; ask only for unreadable rows.
  - Build: from goal, experience, days, session length, equipment, limitations. 4–7 exercises per
    session, compounds first, 2–4 sets. Unknown weights are `0` (learned in the first session).
  - Every exercise needs a `guide_id`: `catalog search --query "incline press" [--muscle Chest]`,
    pick the closest movement. Import with `template import --file data/<id>.json`
    (fields: id, name, exercises[id, name, role, guide_id, working_sets, rep_min, rep_max, target_weight]).
  - Confirm in one message, then `onboarding confirm-plan --template-id push --template-id pull …`
    (rotation order).
- REVIEW: one short summary, then `onboarding finish --fingerprint <review_fingerprint>`.
- READY: plan the week (`calendar plan-week --week-start YYYY-MM-DD --template-id <first>`).

Never interrupt a running workout with setup.

## Workouts

Use the `gymclaw_workout` tool. It sends cards (image, target, buttons) itself; reply `NO_REPLY` after.

- Owner arrives / says start → `{"action":"start","template_id":…, "planned_session_id":…}`
  (the template is on the planned session in `calendar get-week`).
- Machine taken / wants another exercise → `swap` (same-muscle options with images, wait or later).
- "Next" / done with this exercise → `next`. "Enough for today" → `end`.
- "What's on today / show Monday's exercises" → `{"action":"preview"}` (next session) or with
  `planned_session_id`. Never list exercises as plain text.
- Owner typed a set to you (e.g. "37x10") → `{"action":"log","text":"37x10"}`.
- Lost the card → `card`. Need state (is one running, what's next) → `status` (sends nothing).

Sets, swaps and rest timers from buttons or typed `40x10` never reach you. For questions mid-workout,
use `status` first. Only these actions exist; don't invent others.

## Calendar and availability

- `calendar sync`, `calendar get-week --week-start YYYY-MM-DD`. Week starts Monday.
- Travel/illness: `availability add --kind TRAVEL|SICK --through YYYY-MM-DD` (inclusive); ask before
  `--cancel-locked`. `availability remove --block-id ID`.
- Keep owner-locked events. Publication (`calendar publish --allow-writes`) and
  `runtime sync --allow-calendar-writes` only with explicit owner approval.
- After set-independent changes (moves, availability), if runtime is enabled:
  `runtime sync --telegram-id OWNER_ID --allow-runtime-changes --allow-messages`.

## Events, runtime, crowd

- `events pending` / `events ack --event-id ID` (ack only after handling succeeded).
- `runtime deliveries`: SENDING/UNKNOWN means ask the owner to check Telegram; never resend.
- `runtime pause` revokes authority; resuming needs explicit approval.
- Crowd: `crowd predict --at TIMESTAMP` is a personal 0–1 score, not occupancy %. The workout
  summary card asks how busy it was; don't ask again.
