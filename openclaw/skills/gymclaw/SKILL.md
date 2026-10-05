---
name: gymclaw
description: Set up, plan and run the owner's training with GymClaw tools (onboarding, workouts, calendar, crowd).
user-invocable: true
---

# GymClaw tools

`../scripts/gymclaw-tool <group> <operation> …` → JSON `{ok, data, user_message_hint}`. Exit 1 = failed;
never continue as if it worked. Mutations need `--request-id`: one per owner action (e.g. `tg-<message id>-<op>`),
reused only for a retry of that same action. Everything you need is below; if a flag is unclear, use
`<group> --help`. Never read GymClaw's source code.

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
- Owner typed a set to you (e.g. "10x37") → `{"action":"log","text":"10x37"}`.
- Lost the card → `card`. Need state (is one running, what's next) → `status` (sends nothing).

Sets, swaps and rest timers from buttons or typed `10x40` (reps × weight) never reach you. For questions mid-workout,
use `status` first. Only these actions exist; don't invent others.

## Calendar and availability

- `calendar sync`, `calendar get-week --week-start YYYY-MM-DD`. Week starts Monday.
- Move one session: `calendar move --session-id ID [--day YYYY-MM-DD | --to 2026-10-09T11:30:00+02:00]`.
  Without `--to` it picks the quietest valid slot (on `--day`, else in that week). An invalid `--to`
  fails with the valid start times for that day; offer those.
- Lasting preferences (time window, weekdays, session length, rest days): `profile update --data
  '{"latest_workout_finish":"17:00"}'` (fields: `profile get`). It moves sessions that no longer fit
  and returns them in `data.replanning.changed`. Don't toggle the profile to steer one session; use `calendar move`.
- Personal calendar is read-only busy time, refreshed by the watcher. `calendar personal-status`
  shows health; `calendar personal-sync` forces a refresh. Never connect/disconnect it unless the owner asks.
- No-show nudge ("… hasn't started. Skip it or move it?"): find the session in `calendar get-week`.
  Skip → `calendar skip --session-id ID` (no make-up). Move → `calendar move --session-id ID [--day …]`.
- Travel/illness: `availability add --kind TRAVEL|SICK --through YYYY-MM-DD` (inclusive); ask before
  `--cancel-locked`. `availability remove --block-id ID`.
- Keep owner-locked events. Publication (`calendar publish --allow-writes`) and
  `runtime sync --allow-calendar-writes` only with explicit owner approval.
- After set-independent changes (moves, availability), if runtime is enabled:
  `runtime sync --telegram-id OWNER_ID --allow-runtime-changes --allow-messages`.

## Body weight

- Scale photo → read the number with your own vision, `body log --kg 82.4`, reply with the hint
  (it echoes the number so a misread gets caught). Wrong number → `body delete --entry-id ID`, log again.
- Old scale photos (backfill): pass each file, its capture date is used:
  `body log --entries '[{"kg":82.4,"photo":"/path/a.jpg"},{"kg":81.9,"at":"2026-03-02"}]'`.
  `PHOTO_DATE_UNKNOWN` means a compressed photo: ask for it as a file, or for the date (`at`).
  Already-logged photos are skipped as duplicates.
- `body list` for history and trend. Don't ask for weigh-ins; the Sunday briefing does that.

## Events, runtime, crowd

- `events pending` / `events ack --event-id ID` (ack only after handling succeeded).
- `runtime deliveries`: SENDING/UNKNOWN means ask the owner to check Telegram; never resend.
- `runtime pause` revokes authority; resuming needs explicit approval.
- Crowd: `crowd predict --at TIMESTAMP` is a personal 0–1 score, not occupancy %. The workout
  summary card asks how busy it was; don't ask again.
