---
name: gymclaw
description: Plan workouts, guide sets, adapt occupied equipment and manage durable rest/reminder jobs using GymClaw SQLite tools.
user-invocable: true
---

# GymClaw domain tools

Execute `../scripts/gymclaw-tool ...` from workspace. All results are JSON:
`ok`, `data`, `events`, `user_message_hint`. Failure exits 1. Do not proceed
as if a failed operation succeeded. No direct SQLite/OAuth/token-file inspection.
Activated runtime uses actual time; never supply simulated `--now` to live callbacks.

## First use / onboarding

1. `profile get`: inspect persisted constraints; ask only missing/ambiguous choices.
2. `profile update --data '{...}'`: persist confirmed choices. Allowed weekdays
   are Monday=0..Sunday=6. Times are local; all explicit timestamps need offsets.
3. Owner/importer must supply a suitable template. `template get --template-id ID`
   reads it. Demo weights are illustrative, never actual owner training history.
   After owner confirms template and activation, pass `--template-id ID` to runtime
   sync once; selected template persists, Sunday job/rolling maintenance become enabled.
4. `calendar plan-week --week-start YYYY-MM-DD --template-id ID --request-id ID`
   refreshes dedicated calendar and previews a deterministic local plan.
   Week start must be Monday. Do not ask owner to choose among valid routine slots.
5. Inspect `calendar pending-writes`; only publish after separate approval using
   `calendar publish --allow-writes`. Runtime approval does not cover calendar writes.

## Workout

- `workout start --template-id ID --planned-session-id ID --request-id ID`
  means explicit arrival/start intent. Do not auto-start just because reminder fired.
- `workout current --workout-id ID` returns exact exercise, warm-up/working set,
  target weight/reps, deferred queue, rest job and ETA. Read this before logging.
- `workout log-set --workout-id ID --text '80x9' --set-type WARMUP|WORKING --request-id ID`.
  Warm-up is explicit and does not count toward working volume. Never change an
  ambiguous weight/reps interpretation into fabricated performance; ask once.
- Every mutation request ID corresponds to one user action, e.g. authenticated
  Telegram chat/message ID plus operation. Same retry uses same ID. New action
  uses new ID. Do not copy sample request IDs into real calls.
- Logging working sets creates durable local rest job. After activated runtime,
  reconcile timers immediately with `runtime sync --telegram-id OWNER_ID
  --allow-runtime-changes --allow-messages` (only with activation approval).
  Reply with tool hint/ETA, normally "Rest.". Callback sends next-set prompt.
- Do not run `notifications due` or `workout rest-complete` manually in activated
  operation: these are local-only debug tools and would bypass delivery outbox.
- `workout machine-busy --workout-id ID --request-id ID`: reorder before substitution.
  Deferred exercises stay unresolved. Explain next action and revised ETA.
- `workout machine-free --workout-id ID --exercise-id RUNTIME_UUID --request-id ID`.
- `workout substitute --workout-id ID --exercise-id RUNTIME_UUID --substitute-id CATALOG_ID --request-id ID`:
  same-role alternative; preserve remaining volume, no invented equipment status.
- `workout skip-exercise --workout-id ID --exercise-id RUNTIME_UUID --reason TEXT --request-id ID`:
  explicit user intent only. No silently discarded deferred exercise.
- `workout finish --workout-id ID --request-id ID` saves audit/progression only
  after queue resolves. `audit workout --workout-id ID` reads saved summary.

## Calendar and agent work

`calendar sync` detects manual moves/resizes/deletes, updates reminders and repairs
unlocked invalid sessions. `calendar get-week --week-start YYYY-MM-DD` reads state.
Keep locked events even when conflicts need explicit resolution. Never recreate
exact deleted event. No other calendars or silent primary fallback.

`events pending` reads durable work; `events ack --event-id ID` acknowledges only
successfully handled work. Unresolved issues remain pending. No repeated plan
reconsideration without trigger. Watcher runs every 60s, without model calls.

`audit week --week-start YYYY-MM-DD` reads weekly performance. Sunday job audits,
plans next week and sends one briefing through durable outbox. Watcher also marks
elapsed unstarted slots missed (never completed), fills rolling horizon and handles
crowd-label triggers by reconsidering distant tentative slots. Committed/user-locked
sessions stay fixed for soft crowd preference changes.

Calendar acknowledgement/weekly briefing delivery belongs to runtime outbox too;
do not send them again from heartbeat. Continuing publication is separately approved
with `runtime sync --allow-calendar-writes`; never grant merely because Telegram works.
`runtime revoke-calendar-writes` and `runtime pause` are local authority revocation.
Resume needs explicit approval; never undo owner pause from scheduled callback.

`runtime plan --telegram-id OWNER_ID` is offline inspection. `runtime deliveries`
shows delivery receipts; SENT is confirmed, SENDING/UNKNOWN is ambiguous. Never
blindly retry/resolve ambiguous sends; owner must inspect Telegram and confirm
sender process stopped first. Do not duplicate callback-owned notifications.

## Crowd

`crowd poll` reads public MySports reported active count; not guaranteed live occupancy.
`crowd get-source-health` distinguishes retrieval age from unknown backend freshness.
`crowd predict --at TIMESTAMP_WITH_OFFSET` reads label-calibrated personal 0–1 score
and heuristic confidence. Never call score an occupancy percentage or validated accuracy.
No labels may mean unknown score despite available raw count. Google acquisition remains
unconfigured; don't invent Popular Times data or future MySports attendance.

At arrival ask how busy it feels: Empty/Fine/Busy/Packed. Persist with
`crowd record-feedback --workout-id ID --rating busy --request-id USER_ACTION_ID`.
Defaults to actual recorded arrival time, pairs only prior recent observations. Can
also ask after finish; audit then reflects feedback. Never fill feedback without user
answer. Replan event is durable; normal future planning now uses stored labels.

No medical advice.
