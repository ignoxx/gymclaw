# GymClaw

Autonomous training agent. Product source of truth: [SPEC.md](SPEC.md).

## Implemented slice

- SQLite schema for all 10 spec domain entities; versioned Alembic initialization.
- Validated, persistent single-user profile.
- Deterministic weekly candidate planner: prep/travel conflict checks, travel blocks,
  local-date recovery across week boundaries, weekly caps, weighted scores, DST.
- Persistent tentative plans and durable planning events; retry-safe request IDs.
- Persisted workout templates/snapshots and guarded workout execution.
- Explicit first-primary warm-up, set parser/logging, durable rest outbox, dynamic ETA.
- Busy-machine reorder/defer/retry, same-role substitution, explicit skips.
- Double progression, persisted next weights, completion audit and replan events.
- JSON CLI, independent of OpenClaw and external APIs.

This is **not yet the complete MVP**. Calendar writes/sync/reconciliation, external
notification delivery, crowd learning and OpenClaw/Telegram integration remain unimplemented.
Planning creates local tentative records, **not Google Calendar events**. Rest dispatch
advances local state and emits instructions; it does **not** send Telegram messages.

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
`--db-url`. SQLite only. `.env` is a template, not automatically loaded;
export environment variables in your shell. Credentials and DB files are ignored.

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
availability applies to that invocation only; real ongoing availability will come from
persisted calendar/provider state. No external API behavior is simulated as real.

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
  --text '50x8' --set-type WARMUP --now 2026-10-12T20:00:40+02:00 --request-id demo-warmup
python -m gymclaw.cli workout log-set --workout-id "$WORKOUT_ID" \
  --text '80x9' --now 2026-10-12T20:01:20+02:00 --request-id demo-bench-1
python -m gymclaw.cli notifications pending
python -m gymclaw.cli notifications due --now 2026-10-12T20:03:50+02:00
python -m gymclaw.cli workout current --workout-id "$WORKOUT_ID"
```

Continue logging sets, or use:

- `workout machine-busy --workout-id ... --request-id ...`: defer current movement;
  choose next compatible planned movement, never silently drop deferred work.
- `workout machine-free --workout-id ... --exercise-id ... --request-id ...`: retry
  deferred equipment. Exercise IDs here are runtime UUIDs from queue, not catalog IDs.
- `workout substitute --workout-id ... --exercise-id ... --substitute-id db-fly
  --request-id ...`: explicit same-role fixture alternative; only remaining volume.
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
Only one unfinished workout is allowed.

Warm-up never counts toward volume/progression. Working logs while warm-up is pending
fail with `WARMUP_REQUIRED`. Set parsing accepts `80x9`, `80kg x9`, `9 reps at 80`;
ambiguous input fails with a concise question. Structured `--weight`/`--reps`/`--rir`
are also supported. Logging early cancels stale rest jobs. `notifications due` is an
idempotent local dispatcher; external automation IDs are reserved for later integration.
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
- `gymclaw/cli.py`: JSON adapter.

Services accept explicit time and typed inputs, need no agent runtime. CLI owns
transactions; service callers must commit/rollback. Use `alembic revision --autogenerate`
for subsequent schema changes, review generated migration, then `alembic upgrade head`.
Run `db init` again to upgrade existing DBs; migration tests preserve legacy workout logs.
SQLite migrations pause FK enforcement only on migration connection and validate links
before commit. Old workouts without snapshots remain intact, but are read-only for new
execution services. Local development/tests use isolated temporary DBs.
