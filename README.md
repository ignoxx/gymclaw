# GymClaw

Autonomous training agent. Product source of truth: [SPEC.md](SPEC.md).

## Implemented slice

- SQLite schema for all 10 spec domain entities; versioned Alembic initialization.
- Validated, persistent single-user profile.
- Deterministic weekly candidate planner: prep/travel conflict checks, travel blocks,
  local-date recovery across week boundaries, weekly caps, weighted scores, DST.
- Persistent tentative plans and durable planning events; retry-safe request IDs.
- JSON CLI, independent of OpenClaw and external APIs.

This is **not yet the complete MVP**. Calendar writes/sync/reconciliation, notifications,
workout execution, crowd learning and OpenClaw/Telegram integration remain unimplemented.
Planning creates local tentative records, **not Google Calendar events**.

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

## Code

- `gymclaw/models/__init__.py`: SQLAlchemy schema and UTC timestamp type.
- `gymclaw/migrations/`: Alembic revisions.
- `gymclaw/services/profile.py`: validated profile operations.
- `gymclaw/services/planning.py`: pure domain types and candidate/week planner.
- `gymclaw/services/scheduling.py`: SQLite planning transaction.
- `gymclaw/cli.py`: JSON adapter.

Services accept explicit time and typed inputs, need no agent runtime. CLI owns
transactions; service callers must commit/rollback. Use `alembic revision --autogenerate`
for subsequent schema changes, review generated migration, then `alembic upgrade head`.
