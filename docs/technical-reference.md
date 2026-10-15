# Technical reference

How GymClaw behaves, and how to drive it by hand. Architecture: [rust-runtime.md](rust-runtime.md).

## Running it locally

One binary, `gymclaw`. Rust stable; HEIC photo support (default feature `heic`) needs system
libheif ≥ 1.17 (Debian: `libheif-dev` from bookworm-backports; macOS: `brew install libheif`).

```bash
cargo build --release
gymclaw migrate                  # create or upgrade data/gymclaw.db
gymclaw tools                    # every agent tool with its JSON schema
gymclaw call profile_get
gymclaw call session_update '{"session_id": "…", "fields": {"start": "2026-10-14T18:00"}}'
gymclaw serve                    # the bot
```

The DB is `GYMCLAW_DB`, default `data/gymclaw.db` under `GYMCLAW_DATA_DIR` (default `data`).
SQLite only. `.env` is not loaded automatically; export it (`set -a; source .env; set +a`).
Credentials and DB files are git-ignored.

`gymclaw call <tool> '<json>'` runs one tool exactly as the agent would and prints its JSON
result; errors exit non-zero with a code (`SESSION_NOT_FOUND`, …). `-` reads the arguments from
stdin, so secrets stay out of argv. `--now` (ISO 8601 with offset) pretends it is another time;
`--request-id` makes a mutation idempotent: a retry with the same key returns the first result,
the same key with different intent fails. Timestamps without an offset are the owner's local time.

Schema: `gymclaw migrate` (the container entrypoint runs it on every start; `serve` does not). The baseline is the
last alembic head of the old Python schema, so existing DBs upgrade in place; later changes are
numbered SQL files in `src/db/migrations/`.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Tests use in-memory or temporary DBs and fixture providers; no test touches the network.
`scripts/diff-replay` replays owner operations against a copy of a real backup through both the
Rust tools and the last Python release, and diffs the results ([how](../scripts/diff-replay.md)).
Two ignored tests check against a real DB copy: `GYMCLAW_COMPAT_DB=/tmp/copy.db cargo test -- --ignored`.

## Onboarding

Setup is a short interview in Telegram, one question at a time. Nothing is assumed: defaults never
count as answers, and GymClaw works for any owner, schedule and gym.

1. **Interview:** goal, training experience, days per week and which days, session length, time
   window, gym address with prep and travel time, equipment (full gym / basic / home), injuries or exercises to avoid.
   Answers that shape scheduling also write the matching profile fields.
2. **Plan:** the owner sends a photo of their plan, or GymClaw builds one from the interview. Every
   exercise is mapped to an illustrated catalog entry (`catalog_search`). Unknown weights start at 0
   and are learned from the first session. Several templates form a split that rotates over
   sessions in order (e.g. Push → Pull → Legs); each session gets the one trained longest ago, so a
   skipped or swapped-out workout comes next instead of repeating the last.
3. **Review:** one short summary; the owner confirms.
4. **Ready:** later profile or plan edits don't restart setup.

```bash
gymclaw call onboarding_status
gymclaw call onboarding_answer '{"answers": {"experience": "2 years"}}'
gymclaw call onboarding_answer '{"answers": {"schedule": "3x Mon/Wed/Fri"},
  "profile": {"weekly_target_sessions": 3, "weekdays_allowed": [0, 2, 4]}}'
gymclaw call onboarding_confirm_plan '{"template_ids": ["push", "pull", "legs"]}'
gymclaw call onboarding_finish '{"fingerprint": "REVIEW_FINGERPRINT"}'
```

Profile-backed questions and their fields:

| Question | Profile fields |
| --- | --- |
| schedule | `weekly_target_sessions`, `weekdays_allowed` (Monday=0) |
| session_length | `preferred_workout_minutes` |
| time_window | `earliest_workout_start`, `latest_workout_finish` (local "HH:MM") |
| travel | `gym_address`, `prep_minutes`, `commute_to_gym_minutes` |

Mutations are request-idempotent and survive chat resets. Finishing setup grants no runtime or
calendar authority. `template_import` fails with `ILLUSTRATION_REQUIRED` until every exercise has a
`guide_id`. An existing goal and template are kept; setup asks only for the rest.

## Planner decisions

- Allowed weekdays: integers Monday=0 through Sunday=6.
- Configurable durations default to 70 preferred / 45 minimum minutes;
  local bounds default to 07:00–22:00. Prep/commute default to 15/20/20 minutes.
  These unspecified onboarding defaults are placeholders, not learned preferences.
- Generate 15-minute candidates at preferred and minimum duration.
- Workout fits local start/finish bounds; entire prep/travel block must be free.
  Intervals are half-open, so touching appointments are valid.
- At least one full local rest day by default, including sessions in adjacent weeks.
  These are default-plan rules; exact agent/owner edits may override them with warnings.
- Maximize achievable target session count first, then total score. Best candidate per
  local day + exhaustive weekday combinations avoids greedy recovery traps.
  Equal scores pick earliest date/start, preferred duration first.
- Profile controls crowd/calendar weights. No data → null crowd, zero confidence.
- Existing active/completed sessions count toward weekly target/max and recovery;
  locked sessions keep their time and template. `plan_week` fills gaps; background inspection reports conflicts to the agent.
- Each plan returns scores, minimum feasibility and target shortfall. Impossible
  minimum is reported, never met by silently relaxing constraints.
- Successful request ID replay returns original result. Reuse with changed intent fails.

The agent owns planning decisions, rotation overrides and exceptions. `get_week` reads the plan;
`session_create` and `session_update` edit exact times, duration, template and status. All edits lock
the session and leave other sessions alone; warnings describe rest-day, weekly maximum, window,
calendar and allocation conflicts. Invalid times or unknown templates fail back to the agent.

```bash
gymclaw call session_create '{"fields": {"start": "2026-10-17T10:00", "end": "2026-10-17T11:10", "template_id": "legs"}}'
gymclaw call session_update '{"session_id": "…", "fields": {"template_id": "pull", "status": "COMMITTED"}}'
gymclaw call session_update '{"session_id": "…", "fields": {"status": "CANCELLED"}}'
gymclaw call sql_query '{"sql": "SELECT workout_template_id, status, planned_start_at FROM planned_session ORDER BY planned_start_at DESC LIMIT 20"}'
```

Create requires a start and real template; end defaults to the profile duration, and status defaults
to COMMITTED within 48 hours, otherwise TENTATIVE. Update preserves
omitted fields; moving the start alone preserves duration. Timestamps without offsets use the
owner's timezone. Cancellation queues deletion of the owned calendar event; SKIPPED/MISSED keep
calendar history and cancel pending jobs/writes. Any scheduling status can be set, but this never
fabricates workout or set logs. Running/historical workouts use the workout tools.

Session mutations go through the same allocation, calendar outbox, reminder and idempotency
mechanics as calendar edits. `profile_update` and calendar/feed changes report conflicts without
repairing saved sessions. The agent chooses whether to move/cancel a session, accept an exception
or ask the owner. Standing preferences and dated temporary exceptions go into owner notes,
which appear in every prompt. Default planning still scores quiet slots ahead of learned habits.

`sql_query` uses a separate read-only connection (an isolated query-only copy for in-memory DBs),
returns at most 200 rows, and allows schema discovery via `sqlite_master` and `PRAGMA table_info`.
All application tables are readable except OAuth credentials and the private personal feed URL.
Timestamps are naive UTC text; JSON columns are text. Raw SQL writes are not exposed: domain tools
preserve calendar, allocation, locks and reminder invariants.

## Google Calendar setup

Use one dedicated Google calendar for GymClaw workouts. Blockers you add there count, and so
does an optional read-only personal calendar (see below). Add the Google account to Apple
Calendar and enable this calendar there; dragging/resizing/deleting then acts as user intent.

1. Enable the Google Calendar API in your Google Cloud project.
2. Configure OAuth consent; add your Google account as test user while the app is in testing.
3. Create a **Desktop app** OAuth client and download its JSON to `data/google-client.json`
   (or pass `--client-file`). Keep it out of Git.
4. Set `GOOGLE_CALENDAR_ID` to the dedicated calendar's ID, not `primary`.
5. Authorize, then sync once:

```bash
gymclaw calendar-auth                     # --calendar-id / --client-file to override
gymclaw call calendar_sync
```

`calendar-auth` prints a consent URL. Open it in any browser, approve, and paste back the URL the
browser ends up on: a `127.0.0.1` page that won't load, which is expected. The code in it is
exchanged with PKCE. This works from any terminal, including inside the container
(`docker compose exec -it gymclaw gymclaw calendar-auth`). It does not modify calendar events.

OAuth tokens live in SQLite, never in tool output. The DB is set to mode `0600`; tokens are
plaintext local secrets, so keep DB and backups private. Re-run `calendar-auth` on
`CALENDAR_AUTH_REQUIRED`: Google expires refresh tokens of testing-mode apps after 7 days.

**OAuth scope covers events on calendars you own, not one specific calendar.** GymClaw
requests `calendar.events.owned`, not full calendar/ACL permissions. It pins requests
to one explicit ID and binds the DB to that ID; mismatched IDs fail before API calls.
There is no silent primary-calendar fallback.

### Personal calendar (read-only)

Your everyday calendar can block workout slots without GymClaw getting write access.
It is read as an ICS feed. GymClaw only ever GETs it and keeps just busy start/end
times, no titles or notes.

1. In Apple Calendar: right-click the calendar → **Share Calendar…** → tick
   **Public Calendar** → copy the `webcal://pNN-caldav.icloud.com/published/2/…` link.
2. On the Docker host, run `scripts/connect-personal-calendar` and paste the link at the
   hidden prompt. It resolves iCloud's redirect, checks the feed host is in
   `EGRESS_EXTRA_HOSTS`, and pipes `{"url": …}` into `gymclaw call personal_calendar_connect -`
   (stdin, never argv), which fetches once and reports conflicts. `personal_calendar_status` shows
   health afterwards.

Google's **Secret address in iCal format** works the same way.

The 60 s watcher refreshes the feed at most every 5 minutes and the Sunday plan
always does. Busy events block slots; events marked **Free** (`TRANSP:TRANSPARENT`)
and cancelled events don't. A failed fetch records `last_error_code` and keeps the
last known blockers. The link is unauthenticated, so anyone holding it can read the
calendar. It lives only in the DB and is never printed (status shows the host only).
To stop: `personal_calendar_disconnect`, then turn off **Public Calendar** in
Apple Calendar to revoke the link.

### Sync and publication

Set your real profile constraints and import an appropriate workout template first.
Demo seed weights (`config/exercises.seed.json`) are illustrative, not personalized advice.

Plans are local until calendar writes are granted. `calendar_pending_writes` lists what would be
written (calendar ID, times, bodies); inspect it before granting. Writes are a separate authority
from reminders: `runtime_resume` with `"calendar_writes": true`, revoked with
`calendar_writes_revoke` (see [Runtime authority](#runtime-authority)). Once granted, the scheduler
publishes owned queued writes after each tick.

Sync accepts manual edits, recalculates prep/leave/finish, replaces local reminder jobs,
locks edited sessions, compresses resized slots and reports conflicts for the agent.
Locked recovery/blocker conflicts remain visible, not silently reverted. Slots too short
for required movements stay edited and report explicit adjustment needed. Deletes leave
tombstones; exact deleted events/slots are not recreated automatically. Metadata-only
updates may refresh description/revision on locked events; never their chosen times/title.

Upcoming sessions commit within 48 hours; get-ready/leave/start jobs become durable.
The watcher syncs every 60 seconds and reconciles reminders. Decision events wake the agent;
quiet ticks do not call the model.

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
  if a request fails, prior applied writes remain recorded, unattempted writes stay queued.
- OAuth, sync tokens and private calendar bodies never appear in tool results/event inbox.
  Stored snapshots omit attendees/descriptions; generated gym write descriptions are public
  to whoever can access that calendar, so contain only session timing/crowd summary.

## Workout coach

In Telegram the coach runs without the model: `gc:<action>:<ref>` button taps and typed sets
(`10x40`, `13 reps`) go straight to the coach, which logs, edits the live card and runs the rest
countdown. Anything it doesn't understand goes to the agent. The agent drives the same state
through the `workout` tool:

| `action` | What it does |
| --- | --- |
| `status` | Read the running workout; sends nothing |
| `preview` | Next (or given) planned session as one image with every exercise |
| `start` | Today's planned session (or `template_id` / `planned_session_id`) |
| `log` | `text` like `10x40` logs a set |
| `card` | Resend the current card |
| `swap` | Up to 3 same-muscle alternatives as buttons (before the first set) |
| `switch` | `exercise`: the owner is doing this now; finishes or replaces the current one |
| `relabel` | `exercise` + `which`: rename a logged exercise, keeping its sets (also after the workout) |
| `rest` | `seconds` between sets for this workout; `remember` makes it the default |
| `later` / `next` | Defer the current exercise / move on |
| `end` | End early, save, show the summary |

`exercise` takes a guide_id or exact name;
on `EXERCISE_UNKNOWN` the error lists matches. `workout_log_past` logs a workout done without the
bot; `workout_audit` reads a finished workout's saved audit.

Typed sets accept either order (`9x80`, `80kgx9`, `9 reps at 80`); unclear order is resolved
toward the last/target weight, then reps-first. Ambiguous input fails with a concise question.
Sets display reps first: `9 × 80 kg`. Swap options: exercises swapped away from this slot first,
then template alternatives, then same-muscle catalog exercises (owner history ranks first); never
one still in the workout or with the same artwork.

Machine busy (`later`) defers the current movement: next comes a pending movement for the same
primary muscle, else plan order. Deferred work is never dropped. Substitution covers only the
remaining volume. Skipping an exercise is an explicit volume reduction; dependants of skipped
movements need explicit resolution. Finishing (after the last exercise, or `end`) saves the audit and next
working weights, marks the linked planned session completed and queues a planning event.

Only one unfinished workout is allowed. Re-importing a template does not alter a workout already
in progress. Linked workouts use snapshotted calendar volume allocation; compression removes
low-priority accessories first, preserves prerequisites and the first primary warm-up, and does
not lower the full-volume progression threshold. Compression uses configured or learned set
durations, refreshed again at workout start.

Warm-up never counts toward volume/progression. Logging a working set skips a pending warm-up.
Logging early cancels stale rest jobs; the final set needs no rest timer.

ETA counts remaining warm-up/working sets, rests, transitions, deferred/substituted work,
current outstanding rest and elapsed time. Configured estimates are used until completed
sessions provide usable set durations. No equipment wait duration is invented.
Progression requires full prescribed sets at target weight and top reps; partial or
missed targets hold weight. Warm-up and historical logs are not changed by progression.

## Crowd signals / learning

```bash
gymclaw call crowd_status
gymclaw call crowd_forecast '{"at": "2026-10-21T20:00"}'
gymclaw call crowd_feedback '{"workout_id": "…", "rating": "BUSY"}'
```

The scheduler polls the configured source (MySports, any HTTPS JSON/text endpoint, or an
owner-provided command) every 15 minutes when crowd polling is granted, and stores the count.
`crowd_status` shows whether readings flow and source health. The DB binds source identity to
prevent silent gym/provider mixing. [Sources, setup and limits](crowd.md).

Save every **reported active count** with local retrieval time. MySports has no source
observation time/cache age: freshness stays null, never "0 seconds/live". Changed
counts give possible change windows, not proven refresh cadence. Count and percentage
remain distinct. Dated percentages and undated "typical week" profiles are
not collected or used as measured future attendance. No count-to-percentage conversion.

User ratings map EMPTY/FINE/BUSY/PACKED to personal 0–1 score, not physical occupancy.
Only prior readings retrieved within 30 min pair with a label; future polls are excluded.
A second distinct label for the same visit fails. Source weights use prediction
error against later feedback, not in-sample fits. Learned discomfort summary needs
at least three matching busy/packed visits.

Before labels, reported count is shown but **not normalized**; forecast may be unknown.
Under 10 paired labels use conservative neighbor heuristic; 10+ use small monotonic
regression. Future slots combine same-weekday/hour locally collected count history
and user slot labels. Repeated polls count as one dated-hour
mean rather than independent days. Confidence is heuristic evidence strength, **not
validated forecast accuracy**. Default planning uses these signals. Locked sessions
remain untouched.

The gym's own check-in counts are the only crowd source; Google Popular Times was dropped
(no official API, and polled counts are more direct). Source outages retain observations/model
and reduce confidence.

## Runtime authority

Reminders, rest timers, the Sunday plan and briefings run in-process in `gymclaw serve`, gated by
one `runtime_settings` row. A fresh DB has none, so nothing is sent until the owner turns it on.
The agent does that when the owner asks in chat ("turn on reminders"), or by hand:

```bash
gymclaw call runtime_status
gymclaw call runtime_resume '{"owner_asked": true, "crowd_polling": true}'
```

`runtime_resume` creates the row on first use (recipient = `GYMCLAW_TELEGRAM_USER_ID`) and enables
sends. `calendar_writes` and `crowd_polling` are separate grants; resuming never restores calendar
writes unless asked. Weekly planning uses the first template of the owner's confirmed split.
`runtime_pause` stops reminders, briefings, planning ticks and calendar writes, and revokes write
authority. `calendar_writes_revoke` removes only write authority, without network calls.

Proactive messages go through an outbox (`deliveries`). A delivery stuck in SENDING or UNKNOWN is
never retried automatically, since Telegram has no exactly-once send. Check the chat, then
`delivery_resolve` with `sent` or `not-sent` (`not-sent` queues it again).

The weekly tick (Sunday from 19:00 profile-local) audits the week, drafts next week only when it
is empty, and queues a week-rollover event for the agent to assess and communicate. Calendar
read failure prevents drafting against stale blockers. Crowd labels queue an agent event; existing
session times are never moved automatically. Default rotation skips locked templates.

Calendar clashes, infeasible weeks, no-shows, missed slots and stale workouts enter a durable event
inbox. The bot processes decision events in the same serialized worker as owner chat, at most one
batch per minute. Successful turns acknowledge their events; failed turns stay pending across
restarts and retry without repeated failure messages. An unchanged conflict is deduplicated even
after acknowledgement. Elapsed unstarted slots may be marked MISSED, but make-up sessions and
closing idle workouts require agent decisions. Crowd forecast refresh and reminder upkeep remain
deterministic. No automatic “Plan updated” briefing is sent.

### Travel / illness overrides

`availability_add` with `kind` `SICK`, `TRAVEL` or `UNAVAILABLE` and either `from_date`/`through`
(inclusive local dates) or `start`/`end` (exclusive end; `start` defaults to now). Overrides
persist across restarts, participate in prep/travel/recovery planning and expire automatically.
No remote blocker events or fabricated workout logs.

`availability_list` inspects state; `availability_remove` retracts early.
Manual calendar locks stay visible, but conflicting reminders/new starts are blocked.
Cancel locked slots only after explicit permission with `cancel_locked: true` (records
superseding user intent). Owned calendar deletions remain queued behind publication
approval. Active workout history is preserved; suspended rest guidance does not silently
finish an open workout.

## Code

See [rust-runtime.md](rust-runtime.md#layout) for the layout. Domain functions take explicit time
and typed inputs and need no agent runtime; tools own the transaction.
