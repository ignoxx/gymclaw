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

Start first coaching interaction with `onboarding status`. Follow its next step;
never treat defaults as completed setup. Resume saved state, not a questionnaire
from scratch. One question or small batch per turn; don't narrate tool operations.

1. GOAL: ask training goal; save `onboarding set-goal --goal TEXT --request-id ID`.
2. PROFILE: review saved schedule briefly: sessions/week, allowed days/time range,
   session length, prep/travel, recovery/rest. Ask whether to keep or adjust it.
   Apply explicit changes via `profile update --data '{...}'`, then read status
   again. On owner's confirmation: `onboarding confirm-profile --fingerprint HASH
   --request-id ID`. Use status's current profile fingerprint; never invent it.
   Monday=0..Sunday=6; times local, explicit timestamps need offsets.
3. TEMPLATE: offer saved template names from status or ask for workout-plan images.
   Read images natively; no OCR/install loops. Extract compact exercise/sets/reps/
   stated-weight draft, ask only unclear/missing starting or warm-up weights.
   Save confirmed definition to private `data/` JSON and `template import --file PATH`.
   Definitions require id/name/exercises; each exercise id/name/role/target_weight,
   working_sets/rep_min/rep_max; first primary requires explicit warmup_weight.
   Use `template get --template-id ID` to review; never seed/reimport blindly.
   Ask one compact confirmation of actual plan/weights (not logged performance).
   Then `onboarding confirm-template --template-id ID --fingerprint HASH --request-id ID`
   using matching template fingerprint from fresh status. Demo/fixture templates
   cannot stand in for real owner's plan.
4. REVIEW: one short summary; on approval `onboarding finish --fingerprint HASH
   --request-id ID` with current review fingerprint. This grants no runtime or
   calendar authority. Changed profile/template invalidates review automatically.
5. READY: preview with `calendar plan-week --week-start YYYY-MM-DD --template-id ID
   --request-id ID`. Week starts Monday. Choose valid slots, don't make owner pick.
   If runtime already enabled and reminders approved, sync selected template once;
   Sunday/rolling maintenance becomes enabled. Don't re-ask existing permissions.

Calendar publication stays OFF during testing. Only publish pending writes after
separate explicit approval. Onboarding finish is not that approval. If user wants
to test a workout instead, require explicit start intent; never fabricate a session.

## Exercise images

Workout reads and mutation results include `active_exercise.illustration`.
When a workout starts or the exercise changes, attach its `telegram_png` through
Telegram's message tool with the exercise, weight and reps in the same message.
Use local PNG, not SVG. Telegram photos do not accept SVG.
Do not resend images for every set or rest reminder. If delivery is uncertain,
stop rather than send another copy. Reply with text for unmapped exercises.
Do not send images outside an owner-requested workout.

Definitions may set `guide_id` to a Workout Guide catalog slug. Choose it only
when exercise and equipment match. Do not guess from role or similar names.
Without it, only unique exact catalog names map. Unknown names have no image.
Search matching entries in `gymclaw/assets/workout-guide/manifest.json`; do not
dump the catalog into context. Add linked credit in the caption using returned
source/license URLs: Workout Guide / Bryl Lim / Everkinetic, CC BY-SA 4.0.
Mention the returned presentation change briefly, such as "dark background added".
Images explain exercises. They do not represent logged performance.

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

## Availability overrides
----------------------
For user travel/illness, persist `availability add --kind TRAVEL|SICK --through
YYYY-MM-DD --request-id ID`; `--through` is inclusive in profile timezone. Travel
start date uses `--from-date`; exact offsets use `--start`/exclusive `--end`.
Clarify ambiguous dates. Never diagnose or declare recovery. Window expiry allows
normal planning, not a medical claim. Inspect `locked_conflicts`; ask before using
`--cancel-locked`. Override suppresses conflicting reminders/new starts and rest
prompts; preserve actual logs and explicitly resolve any open workout. Sync runtime
after successful changes if activated. `availability remove --block-id ID
--request-id ID` retracts early. No remote publication permission implied.

Calendar and agent work

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
