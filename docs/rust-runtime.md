# How GymClaw is built

One binary owns Telegram, the agent loop, the scheduler, the outbox and the training domain, all
over one SQLite DB (`data/gymclaw.db`).

```
gymclaw serve        # bot: Telegram long-poll, agent, coach, scheduler, outbox
gymclaw call <tool> '<json>'|- [--now …] [--request-id …]   # any agent tool, for ops and testing
gymclaw tools        # tool names, descriptions, JSON schemas
gymclaw migrate      # schema (baseline = last alembic head of the old Python schema, then src/db/migrations)
gymclaw backup [dest]
gymclaw calendar-auth  # Google OAuth by pasting the redirect URL back
gymclaw dashboard    # read-only dashboard
```

## Why one binary

GymClaw used to be a Python package behind OpenClaw, with tools exposed through a shell. On
2026-10-12 the agent was asked to move one session and leave the rest alone. The shell boundary
let it read `--help`, grep source and run ad-hoc Python on the prod DB, and it told the owner
something the state contradicted. Now the model gets typed tools only, a small context, and a way
past missing verbs that doesn't involve the source code (see "Agent"). Behavior, messages, error
codes and DB schema were kept, so existing data and habits carried over.

## Layout

| Path | What |
| --- | --- |
| `src/main.rs`, `src/config.rs` | CLI and environment |
| `src/db/` | schema baseline, migrations, typed rows (`models.rs`) |
| `src/time.rs`, `src/error.rs` | timestamps (naive UTC in the DB), `CODE: message` errors |
| `src/domain/` | planning, scheduling, calendar sync/writes, crowd model, workout, coach, progression, onboarding, body, ticks |
| `src/tools/` | the tool registry: `plan.rs` (week, profile, calendar, crowd), `train.rs` (workout, templates, onboarding, body), `meta.rs` (SQL, notes), `ops.rs` (runtime, deliveries) |
| `src/providers/` | Google Calendar + OAuth, ICS feed, crowd sources, photo decoding/EXIF |
| `src/agent/` | OpenRouter client, per-turn context, media, failure handling, turn log |
| `src/runtime/` | `serve`: bot loop, scheduler, outbox |
| `src/telegram/` | Bot API client, coach card renderer, Markdown |
| `src/web/` | the dashboard |

Rows go through `db::models` (`get/select/first/insert/update`). Status columns are enums.
Mutations run in `domain::core::mutate(db, operation, request_id, intent, now, …)`, which makes
retries with the same request ID return the first result.

## Tools

One struct per tool implementing `tools::Tool` (`NAME`, `DESCRIPTION`, `run`). Args derive
`Deserialize + JsonSchema`; their doc comments become the schema the model reads. The same tool
serves the agent and `gymclaw call`, so anything the agent can do can be reproduced by hand.

Tools include general `session_create`/`session_update` (exact time, duration, template, status),
read-only `sql_query`, `get_week`, `workout`, and profile/notes tools. Session edits lock the session
and return rule warnings; no other sessions change. Mutations that touch other sessions return
`side_effects`, so a reply can't contradict the state. Tools that show something in Telegram push
`Effect::Coach(CoachView)`; the runtime renders it after the transaction commits.

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
- The agent owns planning decisions and exceptions. `note_add {kind: owner}` records standing
  preferences and date-bounded exceptions; notes and lessons appear in every prompt. Background
  events are observations, never fresh owner permission.
- One turn ends with one reply: text (+ optional buttons) or a reaction or nothing. Tool errors go
  back to the model. Step cap (12 calls) and an identical-call breaker; on giving up, one sanitized
  line to the owner.
- Models via OpenRouter: `GYMCLAW_MODEL` (chat, default GLM), `GYMCLAW_MODEL_FAST`,
  `GYMCLAW_MODEL_VISION`; `GYMCLAW_PROVIDER` pins the provider (`reasoning.effort: minimal`).

## Telegram

- DMs with the owner only (`GYMCLAW_TELEGRAM_USER_ID`); everything else is ignored.
- 👀 ack reaction on receipt, removed when the reply is sent; typing indicator while working.
- Two button kinds: `gc:<action>:<ref>` → coach, no model. Agent buttons → `btn:<id>` with the
  value in `agent_button`; a tap is fed to the agent as the owner's text.
- Typed sets (`10x40`, `13 reps`) → coach first; unparsed text and failed validations → agent
  with error context. Failed button operations also fall back to the agent with the card context.
- Coach card: one live card per exercise with image, target and quick buttons, edited in place;
  the rest countdown edits every 5 s.
- Photos/documents in (HEIC included, via libheif ≥ 1.17) → vision model; EXIF capture date for
  scale backfill.
- `/new` clears the chat window; `/status` shows runtime health without the model.
- Every outbound message (agent, coach, reminders, briefings) goes into `chat_message`, so the agent
  sees what the owner sees.
- Per-owner serialized turns; messages arriving during a turn are collected into the next one.
- Turn log: JSONL events in `data/turns/YYYY-MM-DD.jsonl`, secrets redacted
  ([format](../src/agent/turnlog.rs)).

## Scheduler and outbox

- In-process: `ticks::watch` every 60 s (calendar sync, personal feed, reminders), `ticks::weekly`
  Sunday from 19:00 profile-local, `ticks::poll_crowd` every 15 min when enabled, `ticks::publish`
  after ticks when calendar writes are enabled. Conflicts, no-shows, missed slots, stale workouts
  and week rollover create durable decision events rather than automatic replans or fixed briefings.
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
rules); the compiler covers shape. `scripts/diff-replay` compares Rust against the last Python
release on a copy of a real backup. Never test against the production DB; use copies in `/tmp`.
