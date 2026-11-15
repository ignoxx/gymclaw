# How GymClaw is built

One binary owns Telegram, the agent loop, the scheduler, the outbox and the training domain, all
over one SQLite DB (`data/gymclaw.db`).

```
gymclaw serve        # bot: Telegram long-poll, agent, coach, scheduler, outbox
gymclaw call <tool> '<json>'|- [--now …] [--request-id …]   # any agent tool, for ops and testing
gymclaw tools        # tool names, descriptions, JSON schemas
gymclaw migrate      # schema: src/db/baseline.sql, then src/db/migrations (result: src/db/schema.sql)
gymclaw backup [dest]
gymclaw export [path]  # every logged set as CSV in Strong's format (also the dashboard's /export.csv)
gymclaw illustrations missing  # exercises with no picture of their own, the work list for scripts/new-illustration
gymclaw calendar-auth  # Google OAuth by pasting the redirect URL back
gymclaw dashboard    # dashboard; writes only the agent's notes, its extensions' on/off switch and plan targets
GYMCLAW_DB=copy.db gymclaw replay --at <local time> [--model …]   # re-run the agent on the chat's last owner message
gymclaw rewind <run> --db copy.db --turns <dir>   # undo a turn's tool calls on a DB copy (turn-log changesets)
gymclaw backfill --db copy.db --turns <dir> --out <file> [--git .]   # index past turns with outcome labels
```

## Why one binary

The model gets typed tools only and a small context. An earlier build exposed tools through a shell;
the agent read source and ran ad-hoc queries on prod, then told the owner things the state contradicted.

## Layout

| Path | What |
| --- | --- |
| `src/main.rs`, `src/config.rs` | CLI and environment |
| `src/db/` | schema (`schema.sql`, generated), baseline and migrations, typed rows (`models.rs`), test fixtures (`test.rs`) |
| `src/time.rs`, `src/error.rs` | timestamps (naive UTC in the DB), `CODE: message` errors |
| `src/domain/` | planning, scheduling, calendar sync/writes, crowd model, workout, coach, progression, effort and lift trends (`trends.rs`), goals, activities outside the gym, onboarding, body, ticks, extensions (the agent's own layer) |
| `src/tools/` | the tool registry: `plan.rs` (week, profile, calendar, crowd), `train.rs` (workout, templates, onboarding, body), `progress.rs` (goals, activities), `exercises.rs` (history import, rebuild for today's equipment), `meta.rs` (SQL, notes), `ops.rs` (runtime, deliveries), `extensions.rs` (playbooks, collections, triggers, capability gaps) |
| `src/providers/` | Google Calendar + OAuth, ICS feed, crowd sources, photo decoding/EXIF |
| `src/agent/` | OpenRouter client, per-turn context, media, failure handling, turn log |
| `src/runtime/` | `serve`: bot loop, scheduler, outbox |
| `src/telegram/` | Bot API client, coach card renderer, Markdown |
| `src/web/` | the dashboard |
| `assets/workout-guide/` | exercise catalog and pictures; missing ones are drawn with `scripts/new-illustration` ([illustrations.md](illustrations.md)) |

Rows go through `db::models` (`get/select/first/insert/update`). Status columns are enums.
Mutations run in `domain::core::mutate(db, operation, request_id, intent, now, …)`, which makes
retries with the same request ID return the first result.

## Tools

One struct per tool implementing `tools::Tool` (`NAME`, `DESCRIPTION`, `run`). Args derive
`Deserialize + JsonSchema`; their doc comments become the schema the model reads. The same tool
serves the agent and `gymclaw call`, so anything the agent can do can be reproduced by hand.

Tools include general `session_create`/`session_update` (exact time, duration, template, status),
read-only `sql_query`, `get_week`, `workout`, and profile/notes tools. Session edits lock the session
and return rule warnings; no other sessions change. Tools that show something in Telegram push
`Effect::Coach(CoachView)`; the runtime renders it after the transaction commits.

Exercises come from the catalog (`domain::illustrations`), which also reads other apps' names
("Bench Press (Barbell)", "RDL"); one saved without a picture gets a warning with its closest matches,
never a refusal. `history_import` writes another app's history (Strong/Hevy CSV, or workouts the agent
read from screenshots) as finished workouts, skipping any already logged, and sets next weights from
it (`domain::import`). `workout_rebuild` swaps what doesn't fit the equipment at hand for the closest
same-muscle exercise that does, with places saved by name in `gym_location` (`domain::equipment`).

The runtime derives `request_id` (`tg-<message id>-<call index>`, `tg-cb:<callback id>`), so the
model never has to. Timestamps without an offset are the owner's local time.

## Agent ("adapt to me")

- System prompt: short. Who the owner is, voice, the few rules that protect data and trust, and
  the owner's notes. No tool manual: tool descriptions carry that.
- Context per turn: system prompt + state snapshot (local time, profile, this week's sessions,
  running workout, open events) + notes + the last ~30 `chat_message` rows. Target < 15k tokens.
- Open-ended questions use `sql_query`: a dedicated read-only SQLite connection, a 200-row cap,
  schema discovery and a concise schema description in the tool docs. OAuth credentials and the
  private feed URL stay excluded. There is no raw SQL write tool; session edits reconcile calendar,
  allocation and reminder state through the validated domain layer.
- Effort and trends (`domain::trends`) come from logged reps and weight only, never rest time: reps
  against the range, drop-off across sets at one weight, chosen weight against the target, estimated
  1-rep max (Epley) against the recent best. Effort sizes the default jump after a workout (easy: two
  increments, grind: hold). Trends stay out of the standing context: the Sunday wake carries stalls,
  easy streaks and hard sets per muscle against the 4-week average, and the `lift_history` view
  answers "how's my bench going?" through `sql_query`.
- Goals (`goal` table, `goal` tool) are the owner's words plus an optional measurable target; code
  computes progress by kind (lift → recent best 1-rep max, body weight, cardio minutes, sessions).
  The "Now" snapshot only names the active goals; progress goes into the Sunday recap and wake, where
  the agent keeps each goal's block (`block_text`, its 4–6 week plan in words) current.
- Activities (`activity` table, `activity` tool) are training outside the gym, one-off or weekly.
  Code stores them, lists them in `get_week` and the Sunday wake, and counts their minutes toward
  cardio goals; what one means for the plan is the agent's call.
- The agent owns planning decisions and exceptions. `note_add {kind: owner}` records standing
  preferences and date-bounded exceptions; owner and learned notes appear in every prompt. Background
  events are observations, never fresh owner permission.
- One turn ends with one reply: text (+ optional buttons) or a reaction or nothing. Tool errors go
  back to the model. Step cap (12 calls) and an identical-call breaker; on giving up, one sanitized
  line to the owner.
- Guards in `src/agent/mod.rs` (look here before adding another): `STEP_CAP` and `DEADLINE` end a
  turn; an identical-call breaker forces a text reply; a rejected reply counts as a step; tool calls
  written as text in the reply are caught and never reach the owner; `NO_REPLY`/empty text is
  silence, and on background turns so is an explicit note-to-self; failures reach the owner as one
  plain sentence from `failure.rs` that never claims nothing was saved. Fix a misbehaving turn at its
  source (event, prompt, tool result) rather than adding a filter, and pin the fix with a test next
  to the existing ones.
- Extensions (`domain::extensions`, #92): the layer the agent writes at runtime, built on the tools
  above and never around them. A **playbook** is named steps for a request that keeps coming back; a
  **collection** is the agent's own log (`agent_data` rows via `data_add`, read with `sql_query`); a
  **trigger** is a daily/weekly local time or a finished workout that raises an `extension.trigger`
  decision event carrying the agent's own instruction, so it wakes through the same path as every other
  background event and may stay silent. Triggers run no logic and get no owner permission: the
  `extension.trigger` wake is a background turn, where `runtime_resume` (and with it calendar writes)
  refuses. One wake per trigger per slot, slots older than an hour are skipped, at most 8 trigger wakes a
  day. Every change is a new `extension_version` row and can be restored; enabled extensions appear in
  the prompt as one line each, the owner lists and switches them with `/extensions` or the dashboard's
  Memory tab. A request no tool covers goes to `capability_gap` (the `capability_gap` tool, plus a row
  for every turn that hit the step cap, the deadline or the identical-call breaker), listed for review
  and never acted on by code.
- Models via OpenRouter: `GYMCLAW_MODEL` (chat, default Claude Haiku 5.5; GLM 5.3 Flash misread owners and claimed edits it never saved in replays), `GYMCLAW_MODEL_FAST`,
  `GYMCLAW_MODEL_VISION`, `GYMCLAW_MODEL_AUDIO` (voice notes, default Gemini 3.5 Flash Lite);
  `GYMCLAW_PROVIDER` pins the provider (`reasoning.effort: minimal`).

## Telegram

- DMs with the owner only (`GYMCLAW_TELEGRAM_USER_ID`); everything else is ignored.
- 👀 ack reaction on receipt, removed when the reply is sent; typing indicator while working.
- Two button kinds: `gc:<action>:<ref>` → coach, no model. Agent buttons → `btn:<id>` with the
  value in `agent_button`; a tap is fed to the agent as the owner's text.
- Formatting is the Markdown subset in `telegram::markdown` (bold, italic, strike, code, `> ` and
  collapsed `>> ` quotes). Records get a confetti effect, a week on target a 🔥 one.
- A set is reps × weight, a time or a distance (the exercise's `measure`). Weight can be `added` to
  bodyweight or `assisted` (counterweight: less is progress), reps can be `per_side`, and exercises
  sharing a `group` run as a superset: no rest until the round is done. Rest is the owner's one
  default unless the exercise sets `rest_seconds`.
- Typed sets (`10x40`, `+10x8`, `8/side`, `2 km`, `25 min`) → coach first; unparsed text and failed validations → agent
  with error context. Failed button operations also fall back to the agent with the card context.
- Coach card: one live card per exercise with image, target and quick buttons, edited in place;
  the rest countdown edits every 5 s.
- Photos/documents in (HEIC included, via libheif ≥ 1.17) → vision model; EXIF capture date for
  scale backfill. A CSV document (a Strong or Hevy export) is saved to `data/imports/` and the agent
  gets its path plus a summary (`domain::import::summary`: workouts, dates, each exercise name and
  its match) to import with `history_import`.
- Voice notes and audio files → `GYMCLAW_MODEL_AUDIO` (Telegram's OGG/Opus sent as is), shown back
  as "🎙 “…”", then routed exactly like typed text (set parser first). Adds the transcription call,
  typically 1–3 s, before the coach or the agent sees it.
- `/status` shows runtime health and `/extensions` lists and switches the agent's extensions, both
  without the model; there is no `/new`, the chat window is always the latest messages. Startup sets the
  Telegram "/" menu to exactly these (`bot::COMMANDS`).
- Every outbound message (agent, coach, reminders, briefings) goes into `chat_message`. The agent's
  history holds only the conversation (owner, agent, background prompts); code-sent messages from the
  last 24 h still in the chat are listed in its "Now" snapshot. A planned session keeps one message in the chat: get-ready, leave, start
  and no-show each replace the previous one, and its buttons go once the session can't be started.
- Per-owner serialized turns; messages arriving during a turn are collected into the next one.
- Turn log: JSONL events in `data/turns/YYYY-MM-DD.jsonl`, secrets redacted
  ([format](../src/agent/turnlog.rs)): each model step's exact request and response, the release
  that ran it, the rows each tool call changed, and outcome labels (failures, the owner's 👍/👎 on a
  bot message). Copying, querying, rewinding and backfilling: [turns.md](turns.md).

## Scheduler and outbox

- In-process: `ticks::watch` every 60 s (calendar sync, personal feed, reminders), `ticks::weekly`
  Sunday from 23:00 profile-local, `ticks::poll_crowd` every 15 min when enabled, `ticks::publish`
  after ticks when calendar writes are enabled, `extensions::fire_due` every tick for the agent's
  triggers. A no-show is sent as the session's last reminder
  (Start, plus Skip/Move handed to the agent). Conflicts, missed slots, stale workouts
  and week rollover create durable decision events rather than automatic replans. The two fixed
  messages are numbers only: the workout summary card and the Sunday recap (`weekly::recap`), both
  built on `domain::highlights`. The agent adds its take after the recap only; the summary card is
  the whole wrap-up.
  The bot checks this inbox every minute in its serialized agent worker, gated by runtime authority;
  successful turns acknowledge events and failed turns remain pending. Unchanged conflicts dedupe
  across restarts and acknowledgement. Default drafts for empty weeks, rotation of unlocked
  templates, crowd forecast refresh, reminders and calendar publication stay deterministic.
  `serve` touches `data/heartbeat` every minute for
  the container health check.
- One-off jobs live in `notification_job` (GET_READY, LEAVE, SESSION_START, REST). One loop sleeps
  until the next `due_at` (or a wake-up after any write) and fires them. REST jobs fire 30 s late as
  a fallback; the coach's live countdown normally handles them.
- Outbox = `notification_delivery`: commit PENDING → claim SENDING → send → SENT with the Telegram
  message id; any send error → UNKNOWN, never retried automatically, because Telegram has no
  idempotent send. The owner checks and the agent calls `delivery_resolve`.
- `runtime_settings` gates it all: `enabled` for sends and ticks, plus separate flags for
  calendar writes and crowd polling. The row is created by the first `runtime_resume`.

## Testing

`cargo test` and `cargo clippy --all-targets -- -D warnings`. Tests pin down behavior that's easy
to get subtly wrong (planner choices, set parsing, progression math, rest timing, stale-delivery
rules); the compiler covers shape. Shared fixtures live in `src/db/test.rs`. Never test against the
production DB; use copies in `/tmp`.
