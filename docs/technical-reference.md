# Technical reference

Detailed CLI and setup reference. Run commands from the repository root.

## Setup

Python 3.12+ required. Initial tests run on Python 3.14.

```bash
python3 -m venv .venv
source .venv/bin/activate
pip install -e '.[dev]'
python -m gymclaw.cli db init
pytest -q
```

DB defaults to `data/gymclaw.db`. Override with `GYMCLAW_DB_URL` or global
`--db-url`. SQLite only. `.env` is not automatically loaded; export variables in your
shell (`set -a; source .env; set +a`). `.env.example` is a template; do not overwrite
an existing configured `.env`. Credentials and DB files are ignored.

```bash
python -m gymclaw.cli profile get
python -m gymclaw.cli profile update --data '{"prep_minutes": 15, "commute_to_gym_minutes": 20}'
python -m gymclaw.cli schedule plan-week \
  --week-start 2026-10-12 --now 2026-10-11T19:00:00+02:00 \
  --fixture config/demo_calendar.json --request-id demo-week-1 --json
```

`planning plan-week` aliases `schedule plan-week`. All commands return JSON envelopes,
including validation errors; errors exit 1. Run `db init` before domain commands.
`--week-start` must be Monday. `--now` requires timezone; omit for current time.

Fixtures explicitly contain demo free/busy and crowd predictions, never live observations.
Fixture inputs and planning results are recorded in SQLite's agent event log. Fixture
availability applies to that invocation only. Ongoing availability comes from persisted
calendar/provider state. No external API behavior is simulated as real.

## Onboarding

Setup is a short interview in Telegram, one question at a time. Nothing is assumed: defaults never
count as answers, and GymClaw works for any owner, schedule and gym.

1. **Interview:** goal, training experience, days per week and which days, session length, time
   window, gym address with prep and travel time, equipment (full gym / basic / home), injuries or exercises to avoid.
   Answers that shape scheduling also write the matching profile fields.
2. **Plan:** the owner sends a photo of their plan, or GymClaw builds one from the interview. Every
   exercise is mapped to an illustrated catalog entry (`catalog search`). Unknown weights start at 0
   and are learned from the first session. Several templates form a split that rotates over
   sessions in order (e.g. Push → Pull → Legs).
3. **Review:** one short summary; the owner confirms.
4. **Ready:** later profile or plan edits don't restart setup.

```text
onboarding status
onboarding answer --answers '{"experience":"2 years"}' --request-id ID
onboarding answer --answers '{"schedule":"3x Mon/Wed/Fri"}' \
  --profile '{"weekly_target_sessions":3,"weekdays_allowed":[0,2,4]}' --request-id ID
onboarding confirm-plan --template-id push --template-id pull --template-id legs --request-id ID
onboarding finish --fingerprint REVIEW_FINGERPRINT --request-id ID
```

Profile-backed questions and their fields:

| Question | Profile fields |
| --- | --- |
| schedule | `weekly_target_sessions`, `weekdays_allowed` (Monday=0) |
| session_length | `preferred_workout_minutes` |
| time_window | `earliest_workout_start`, `latest_workout_finish` (local "HH:MM") |
| travel | `gym_address`, `prep_minutes`, `commute_to_gym_minutes` |

Mutations are request-idempotent and survive chat resets. Finishing setup grants no runtime or
calendar authority. Template import fails with `ILLUSTRATION_REQUIRED` until every exercise has a
`guide_id`. Migration `7b3e9d2c4f10` keeps an existing goal and template, and asks for the rest.

## Planner decisions

- Allowed weekdays: integers Monday=0 through Sunday=6.
- Configurable durations default to 70 preferred / 45 minimum minutes;
  local bounds default to 07:00–22:00. Prep/commute default to 15/20/20 minutes.
  These unspecified onboarding defaults are placeholders, not learned preferences.
- Generate 15-minute candidates at preferred and minimum duration.
- Workout fits local start/finish bounds; entire prep/travel block must be free.
  Intervals are half-open, so touching appointments are valid.
- At least one full local rest day by default, including sessions in adjacent weeks.
  Recovery is hard validation; `recovery_weight` is reserved for later soft scoring.
- Maximize achievable target session count first, then total score. Best candidate per
  local day + exhaustive weekday combinations avoids greedy recovery traps.
  Equal scores pick earliest date/start, preferred duration first.
- Configurable adherence/preferred-time/uncertainty/duration weights in `PlannerConfig`;
  profile controls crowd/calendar weights. No data → null crowd, zero confidence.
- Existing active/completed sessions count toward weekly target/max and recovery;
  user-locked sessions are untouched. This operation fills gaps, not full replanning.
- Each plan returns scores, minimum feasibility and target shortfall. Impossible
  minimum is reported, never met by silently relaxing constraints.
- Successful request ID replay returns original result. Reuse with changed week/fixture
  fails. Retry after profile/calendar changes requires a new request ID.

## Google Calendar setup

Use one dedicated Google calendar for GymClaw workouts. Blockers you add there count, and so
does an optional read-only personal calendar (see below). Add Google account to Apple
Calendar and enable this calendar there; dragging/resizing/deleting then acts as user intent.

1. Enable Google Calendar API in your Google Cloud project.
2. Configure OAuth consent; add your Google account as test user while app is in testing.
3. Create **Desktop app** OAuth client; keep downloaded JSON outside repo:
   `~/.config/gymclaw/google-client.json` (or pass `--client-file`).
4. Set `GOOGLE_CALENDAR_ID` in local `.env` to dedicated calendar ID, not `primary`.
5. Run from repo root:

```bash
source .venv/bin/activate
set -a; source .env; set +a
python -m gymclaw.cli db init
python -m gymclaw.cli calendar auth
python -m gymclaw.cli calendar sync
```

Auth opens browser and waits up to 180 seconds on a loopback callback. It does not modify
calendar events. OAuth tokens/refresh state live in SQLite, never in CLI output. DB is
set to mode `0600`; tokens are plaintext local secrets, so keep DB/backups private. Client
JSON stays outside repo. Reauthorize on `CALENDAR_AUTH_REQUIRED` (testing-mode tokens may
expire). Browser/client failures return sanitized `CALENDAR_AUTH_FAILED`.

**OAuth scope covers events on calendars you own, not one specific calendar.** GymClaw
requests `calendar.events.owned`, not full calendar/ACL permissions. App pins requests
to one explicit ID and binds DB to that ID/provider; mismatched IDs fail before API calls.
There is no silent primary-calendar fallback. Use a separate DB for demo providers.

### Personal calendar (read-only)

Your everyday calendar can block workout slots without GymClaw getting write access.
It is read as an ICS feed. GymClaw only ever GETs it and keeps just busy start/end
times, no titles or notes.

1. In Apple Calendar: right-click the calendar → **Share Calendar…** → tick
   **Public Calendar** → copy the `webcal://pNN-caldav.icloud.com/published/2/…` link.
2. From the repo root on the host, run `scripts/connect-personal-calendar` and paste the
   link at the hidden prompt. It resolves iCloud's redirect, applies a GET-only policy for
   that one host (`config/gymclaw-personal-calendar-policy.example.yaml`), hands the link
   to the sandbox as a temp file (never argv), then fetches once and replans.
   `calendar personal-status` shows health afterwards.

Google's **Secret address in iCal format** also works with `calendar personal-connect
--url-file PATH`. The host helper and its `/published/**` network policy are for iCloud;
for Google, configure a GET-only policy for the feed's host and path separately.

The 60 s watcher refreshes the feed at most every 5 minutes and the Sunday plan
always does. Busy events block slots; events marked **Free** (`TRANSP:TRANSPARENT`)
and cancelled events don't. A failed fetch records `last_error_code` and keeps the
last known blockers. The link is unauthenticated, so anyone holding it can read the
calendar. It lives only in the DB and is never printed (status shows the host only).
To stop: `calendar personal-disconnect` here, then turn off **Public Calendar** in
Apple Calendar to revoke the link.

### Preview, then explicit publication

Set your real profile constraints and import an appropriate workout template first.
Demo seed weights are illustrative, not your training history or personalized advice.
Use a Monday date for `--week-start`:

```text
python -m gymclaw.cli calendar plan-week --week-start YYYY-MM-DD --template-id TEMPLATE_ID --request-id UNIQUE_ID
python -m gymclaw.cli calendar pending-writes
python -m gymclaw.cli calendar publish --allow-writes
```

`calendar plan-week` syncs first, creates local plans and queues writes. Inspect calendar
ID/times/bodies in `pending-writes` before approving publication. `schedule plan-week`
remains the provider-free local operation; optional `--template-id` adds compression plan.
`calendar get-week --week-start ...` reads local state; `calendar replan --week-start ...`
syncs then repairs it. `planning replan --week-start ...` repairs cached state without API.
`calendar move --session-id ... [--day ... | --to ... | --after ...]` moves one upcoming session to the
best valid slot (at/after `--after` if given) or an exact valid start, without a provider sync, and
returns up to two spread-out `alternatives`. Slot score = crowd first, then a learned time-of-day
habit (`services/habits.py`: arrival times of workouts started with the ▶️ button, and owner moves, decayed over ~6 weeks) at a
lower weight, so a habitual busy hour still loses to a quiet one. `profile update`
repairs upcoming weeks right away, so a narrower time window moves sessions that no longer fit.

Sync accepts manual edits, recalculates prep/leave/finish, replaces local reminder jobs,
locks edited sessions, compresses resized slots and repairs only invalid unlocked sessions.
Locked recovery/blocker conflicts remain visible, not silently reverted. Slots too short
for required movements stay edited and report explicit adjustment needed. Deletes leave
tombstones; exact deleted events/slots are not recreated automatically. Metadata-only
updates may refresh description/revision on locked events; never their chosen times/title.

Upcoming sessions commit within 48 hours; get-ready/leave/start jobs become durable.
`notifications due` emits local instructions only. Prepared OpenClaw watcher syncs every
60 seconds without model calls and reconciles reminders. Publication stays off unless
continuing calendar-write authority is separately granted. No watcher/job is installed
until explicit runtime/message approval.

### Safety and retries

- Full/incremental sync consumes all pages, persists token only after reconciliation;
  HTTP 410 triggers a fresh full sync. Tombstones/history are retained for ownership and
  non-recreation guarantees rather than destructively wiping domain state.
- Busy all-day events use exclusive end date/calendar timezone. Recurring blockers and
  moved/cancelled exceptions expand locally with DST-aware bounds; unsupported recurrence
  fails visibly instead of scheduling over it. Transparent events do not block workouts.
- Fetch failure leaves cached plan/token intact. Stable event IDs and persisted outbox
  prevent duplicate creation after crashes. Lost patch responses are recognized by revision.
- Writes require matching ownership/session ID and ETag. Concurrent edits cause conflict;
  sync before retry, never force overwrite. Publication commits each result separately;
  if request fails, prior applied writes remain recorded, unattempted writes stay queued.
- OAuth, sync tokens and private calendar bodies never appear in tool results/event inbox.
  Stored snapshots omit attendees/descriptions; generated gym write descriptions are public
  to whoever can access that calendar, so contain only session timing/crowd summary.

### Calendar fixtures

Calendar fixtures are distinct from `config/demo_calendar.json` planner scoring inputs.
Format is Google event payloads with explicit demo provider ID:

```json
{"calendar_id":"demo-calendar","events":[],"full":true}
```

```bash
python -m gymclaw.cli --db-url sqlite:///data/calendar-demo.db db init
python -m gymclaw.cli --db-url sqlite:///data/calendar-demo.db calendar sync --fixture PATH.json
```

Fixture commands require explicit global `--db-url`; DB cannot mix fixture and Google
providers. `full: true` represents complete remote snapshot; absent cached events become
cancelled. For an edit-only fixture, use `full: false`. Fixture publication is in-memory
simulation persisted into local snapshots, never a Google call; it does not rewrite input
fixture file. Tests inject providers/transports; live HTTP is blocked in pytest.

```bash
pytest -q gymclaw/tests/test_calendar_reconcile.py gymclaw/tests/test_calendar_cli.py
```

## Workout CLI

Import demo template once; its weights are illustrative, not personalized training advice.
All template/runtime/progression/job state lives in SQLite. Re-importing a template does
not alter a workout already in progress.

```bash
python -m gymclaw.cli template import --file config/exercises.seed.json
WORKOUT_ID=$(python -m gymclaw.cli workout start --template-id upper-a \
  --now 2026-10-12T20:00:00+02:00 --request-id demo-start \
  | python -c 'import json,sys; print(json.load(sys.stdin)["data"]["workout_id"])')
python -m gymclaw.cli workout log-set --workout-id "$WORKOUT_ID" \
  --text '8x50' --set-type WARMUP --now 2026-10-12T20:00:40+02:00 --request-id demo-warmup
python -m gymclaw.cli workout log-set --workout-id "$WORKOUT_ID" \
  --text '9x80' --now 2026-10-12T20:01:20+02:00 --request-id demo-bench-1
python -m gymclaw.cli notifications pending
python -m gymclaw.cli notifications due --now 2026-10-12T20:03:50+02:00
python -m gymclaw.cli workout current --workout-id "$WORKOUT_ID"
```

Typed sets accept either order (`9x80`, `80kgx9`, `9 reps at 80`); unclear order is resolved
toward the last/target weight, then reps-first. Sets display reps first: `9 × 80 kg`. In Telegram, the coach plugin drives the same operations through
`coach text|tap|act|start|card`, which return a reaction and ready-to-send cards
(see [coach plugin](../openclaw/plugins/coach/README.md)). `catalog search --query ... [--muscle ...]`
finds illustrated exercises for `guide_id`; `template import` requires one on every exercise.

Continue logging sets, or use:

- `workout machine-busy --workout-id ... --request-id ...`: defer current movement;
  next comes a pending movement for the same primary muscle, else plan order. Deferred work is never dropped.
- `workout machine-free --workout-id ... --exercise-id ... --request-id ...`: deferred equipment is
  free; it becomes active unless the current exercise already has sets. IDs are runtime UUIDs from queue.
- `workout alternatives --workout-id ... [--exercise-id ...]`: read-only swap options (up to 3): exercises
  swapped away from this slot first (changed your mind), then template alternatives, then same-muscle
  catalog exercises (owner history ranks first). Never one still in the workout or with the same artwork
  (the same movement under another name).
- `coach act --action switch|relabel|rest --request-id ... [--exercise X] [--which Y] [--seconds N] [--remember]`:
  owner-led changes. `switch` makes any named exercise the current one (finishes the started one or
  replaces an unstarted one), `relabel` renames a logged exercise and keeps its sets (also after the
  workout; progression and template follow), `rest` sets rest for the rest of the workout (`--remember`:
  profile default too). `--exercise` is a guide_id or exact name; otherwise EXERCISE_UNKNOWN lists matches.
- `workout substitute --workout-id ... --exercise-id ... --substitute-id db-fly
  --request-id ...`: template alternative or same-muscle catalog slug; only remaining volume.
- `workout next-exercise --workout-id ... --exercise-id ... --request-id ...`: move on; completes
  the exercise with the sets done, or skips it if none. `workout skip-warmup` skips a pending warm-up
  (logging a working set does the same).
- `workout skip-exercise --workout-id ... --exercise-id ... --reason ... --request-id ...`:
  explicit volume reduction. Dependants of skipped movements need explicit resolution.
- `workout finish --workout-id ... --request-id ...`: requires resolved queue; save audit
  and next working weights, mark linked planned session completed, emit replan request.
- `audit workout --workout-id ...`: read saved audit without changing progression.
- `events pending` / `events ack --event-id ...`: inspect/ack durable agent inbox.

Mutation request IDs are mandatory. Exact retry returns original result; changed intent
with same ID fails. Use a unique ID per user action, not per network attempt. Original
request ID is needed to retry `finish`; new finish requests fail once audited.
`--now` is optional for runtime, required timezone when supplied. Mutation times cannot
move backwards. `workout start` expresses arrival intent; it does not invent prep/travel
history. `--planned-session-id` optionally links an existing tentative/committed plan.
Only one unfinished workout is allowed. Linked workouts use snapshotted calendar volume
allocation; compression removes low-priority accessories first, preserves prerequisites and
first primary warm-up, and does not lower the full-volume progression threshold.
Compression uses configured or learned set durations, refreshed again at workout start.

Warm-up never counts toward volume/progression. Working logs while warm-up is pending
fail with `WARMUP_REQUIRED`. Set parsing accepts `9x80`, `9x80kg`, `9 reps at 80`;
ambiguous input fails with a concise question. Structured `--weight`/`--reps`/`--rir`
are also supported. Logging early cancels stale rest jobs. `notifications due` is an
idempotent local debug dispatcher; activated runtime callbacks use separate durable
Telegram delivery outbox and persist actual external automation IDs.
The final set needs no rest timer when no work remains.

ETA counts remaining warm-up/working sets, rests, transitions, deferred/substituted work,
current outstanding rest and elapsed time. Configured estimates are used until completed
sessions provide usable set durations. No equipment wait duration is invented.
Progression requires full prescribed sets at target weight and top reps; partial or
missed targets hold weight. Warm-up and historical logs are not changed by progression.

Acceptance test runs a complete fake workout across separate CLI processes, including
rest dispatch, equipment reordering, substitution, finish, audit, and persisted progression:

```bash
pytest -q gymclaw/tests/test_workout_cli.py
```

## Crowd signals / learning

```text
python -m gymclaw.cli crowd poll
python -m gymclaw.cli crowd test
python -m gymclaw.cli crowd get-source-health
python -m gymclaw.cli crowd record-feedback --workout-id ID --rating busy --request-id UNIQUE_ID
python -m gymclaw.cli crowd predict --at 2026-10-21T20:00:00+02:00
```

`poll` reads the configured source (MySports, any HTTPS JSON/text endpoint, or an owner-provided
command) and stores the count; `test` reads once and stores nothing. DB binds source identity to
prevent silent gym/provider mixing. [Sources, setup and limits](crowd.md).

Save every **reported active count** with local retrieval time. MySports has no source
observation time/cache age: freshness stays null, never "0 seconds/live". Changed
counts give possible change windows, not proven refresh cadence. Count and percentage
remain distinct. `/today` dated percentages and `/historic/week` undated profiles are
not collected or used as measured future attendance. No count-to-percentage conversion.

User ratings map EMPTY/FINE/BUSY/PACKED to personal 0–1 score, not physical occupancy.
Feedback defaults to workout arrival time; `--observed-at` can describe another time
within that visit. Only prior readings retrieved within 30 min pair with label; future
polls are excluded. Exact request retry replays; second distinct label for same visit
fails. Audits reflect labels supplied after completion. Source weights use prediction
error against later feedback, not in-sample fits. Learned discomfort summary needs
at least three matching busy/packed visits.

Before labels, reported count is shown but **not normalized**; forecast may be unknown.
Under 10 paired labels use conservative neighbor heuristic; 10+ use small monotonic
regression. Future slots combine same-weekday/hour locally collected count history
and user slot labels. Repeated polls count as one dated-hour
mean rather than independent days. Confidence is heuristic evidence strength, **not
validated forecast accuracy**. Normal planner/replanner uses these signals; explicit
planning fixtures override them. Locked sessions remain untouched.

The gym's own check-in counts are the only crowd source; Google Popular Times was dropped
(no official API, and polled counts are more direct). Source outages retain observations/model
and reduce confidence.

Explicit crowd fixture format:

```json
{"source":"GYM_API","metric":"reported_active_count","raw_value":7}
```

```text
python -m gymclaw.cli --db-url sqlite:///data/crowd-demo.db crowd poll --fixture PATH.json --now TIMESTAMP_WITH_OFFSET
```

Requires initialized isolated DB. Fixtures cannot enter live Google-bound DB and stay
marked demo in predictions/calendar descriptions. Live reads reject artificial `--now`.
For approved OpenClaw polling, activate with `runtime sync --with-crowd-poll` plus
runtime/message approval flags. Installs cheap 15m command payload; callback skips
outside configured workout hours without HTTP. Not installed automatically.

## OpenClaw / Telegram activation

[Self-hosting](self-hosting.md) covers the Docker deployment, owner-only Telegram
and approval boundaries; [deploy/entrypoint.sh](../deploy/entrypoint.sh) holds the exact
OpenClaw config. No runtime is installed/configured automatically.

```text
python -m gymclaw.cli runtime plan --telegram-id OWNER_ID
python -m gymclaw.cli runtime sync --telegram-id OWNER_ID --allow-runtime-changes --allow-messages
python -m gymclaw.cli runtime deliveries
```

`plan` stays offline. `sync` installs executable OpenClaw callbacks and enables messages;
inspect first and obtain approval. No calendar-write approval implied. Select confirmed
`--template-id ID` to install Sunday 19:00 local audit/planning/briefing and enable rolling
maintenance. `--with-crowd-poll` adds cheap 15m readings in configured workout hours.
After set logging, agent immediately syncs rest jobs. UNKNOWN/SENDING sends never blindly retry; owner
must check actual DM and confirm sender stopped before `runtime resolve`. During active
operation, do not use local-only `notifications due` to bypass delivery outbox.

For continuing autonomous calendar publication, preview pending writes first and obtain
separate approval, then activate `runtime sync --allow-calendar-writes` with normal
runtime/message flags. Watcher/Sunday job subsequently publish only owned queued writes.
`runtime revoke-calendar-writes` removes that authority without network calls.
`runtime pause` blocks callbacks/messages and revokes writes; `runtime resume` requires
explicit runtime/message flags and does not restore write authority. No global config
changes. Activated calls reject simulated `--now`; offline previews/fixtures retain it.
`runtime relocate` rebinds a paused DB to the current checkout and Python after a host
move (see [deploy/README.md](../deploy/README.md)); `runtime sync` then recreates timers.

`audit week --week-start YYYY-MM-DD` returns durable weekly performance totals.
`runtime weekly --template-id ID --telegram-id OWNER_ID` is local planning/briefing preview
(no sends/publication); an explicit isolated calendar fixture is supported. Calendar
read failure prevents rolling changes. Known labels can reconsider distant tentative
slots; committed/user-locked slots do not move for crowd preference alone. Changed
calendar consequences and Sunday briefings use same confirmed-delivery/unknown-send rules.

### Travel / illness overrides

`availability add --kind SICK --through YYYY-MM-DD --request-id ID` pauses from now
through inclusive local date. Travel uses `--kind TRAVEL --from-date YYYY-MM-DD
--through YYYY-MM-DD`; offset timestamps use `--start`/`--end` (exclusive end).
Overrides persist across restarts, participate in prep/travel/recovery planning and
expire automatically. No remote blocker events or fabricated workout logs.

`availability list` inspects state; `availability remove --block-id ID --request-id ID`
retracts early. Exact request retries replay; changed intent rejects reused ID.
Manual calendar locks stay visible, but conflicting reminders/new starts are blocked.
Cancel locked slots only after explicit permission using `--cancel-locked` (records
superseding user intent). Owned calendar deletions remain queued behind publication
approval. Active workout history is preserved; suspended rest guidance does not silently
finish an open workout. Resolve its outstanding exercises explicitly before next start.

## Code

- `gymclaw/models/__init__.py`: SQLAlchemy schema and UTC timestamp type.
- `gymclaw/migrations/`: Alembic revisions.
- `gymclaw/services/profile.py`: validated profile operations.
- `gymclaw/services/planning.py`: pure domain types and candidate/week planner.
- `gymclaw/services/scheduling.py`: SQLite planning transaction.
- `gymclaw/services/templates.py`: validated template definitions/import.
- `gymclaw/services/workout.py`: state machine, logs, rest, ETA, request replay.
- `gymclaw/services/adaptation.py`: equipment queue and substitutions.
- `gymclaw/services/progression.py`, `audit.py`: double progression and completion audit.
- `gymclaw/services/notifications.py`, `events.py`: durable local job/event dispatch.
- `gymclaw/providers/`: typed calendar contract, Google REST/OAuth and demo provider.
- `gymclaw/services/calendar.py`, `calendar_busy.py`: sync/reconciliation and recurrence.
- `gymclaw/services/calendar_writes.py`, `replanning.py`: owned outbox and plan repair.
- `gymclaw/services/weekly.py`: rolling horizon, no-show nudges, missed-slot maintenance, weekly audit/briefing.
- `gymclaw/services/body.py`, `body_cli.py`: weigh-ins, photo dates, trend and briefing line.
- `gymclaw/services/compression.py`: deterministic workout time allocation.
- `gymclaw/providers/openclaw.py`, `services/runtime.py`: scoped scheduler and durable delivery.
- `gymclaw/providers/crowd_sources.py`, `providers/mysports.py`, `providers/crowd.py`, `services/crowd.py`: public signals,
  source health, arrival labels, calibration and planner integration.
- `openclaw/`: repo-local workspace (the agent's tool reference is `AGENTS.md`) and activation handoff.
- `gymclaw/cli.py`, `calendar_cli.py`, `runtime_cli.py`, `crowd_cli.py`: JSON adapters.
- `gymclaw/warm.py`: warm worker behind `scripts/gymclaw-tool`. The container keeps one process with
  GymClaw imported and forks it per call (~1.3s → ~0.1s); without it, calls run in-process.

Services accept explicit time and typed inputs, need no agent runtime. CLI owns
transactions; service callers must commit/rollback. Use `alembic revision --autogenerate`
for subsequent schema changes, review generated migration, then `alembic upgrade head`.
Run `db init` again to upgrade existing DBs; migration tests preserve legacy workout logs.
SQLite migrations pause FK enforcement only on migration connection and validate links
before commit. Old workouts without snapshots remain intact, but are read-only for new
execution services. Local development/tests use isolated temporary DBs.
