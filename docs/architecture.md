# How gymclaw is built

One binary owns Telegram, the agent loop, the scheduler, the outbox and the training domain, all
over one SQLite DB (`data/gymclaw.db`). Every module opens with a `//!` summary; `rg '^//!' src` is
the detailed map. This page holds the map and the rules that span modules.

```
gymclaw serve        # bot: Telegram long-poll, agent, coach, scheduler, outbox
gymclaw call <tool> '<json>'|- [--now …] [--request-id …]   # any agent tool, for ops and testing
gymclaw tools        # tool names, descriptions, JSON schemas
gymclaw migrate      # create or upgrade the DB (src/db/schema.sql for a fresh one)
gymclaw backup [dest]
gymclaw export [path]  # every logged set as CSV in Strong's format (also the dashboard's /export.csv)
gymclaw illustrations missing  # exercises with no picture of their own, the work list for scripts/new-illustration
gymclaw calendar-auth  # Google OAuth by pasting the redirect URL back
gymclaw dashboard    # the Mini App (the bot's "Gym" menu button); its writes are DASHBOARD_TOOLS and live-workout steps in src/web/mod.rs
GYMCLAW_DB=copy.db gymclaw replay --at <local time> [--model …]   # re-run the agent on the chat's last owner message
gymclaw rewind <run> --db copy.db --turns <dir>   # undo a turn's tool calls on a DB copy (turn-log changesets)
gymclaw backfill --db copy.db --turns <dir> --out <file> [--git .]   # index past turns with outcome labels
gymclaw doctor       # check DB, owner id, Telegram token, OpenRouter key and crowd config
gymclaw crowd-test   # test the configured crowd source and print the live count
gymclaw crowd-reset  # wipe crowd history and state before switching crowd sources
```

## Layout

| Path | What |
| --- | --- |
| `src/main.rs`, `src/config.rs` | CLI and environment |
| `src/db/` | schema (`schema.sql`, generated), baseline and migrations, typed rows (`models.rs`), test fixtures (`test.rs`) |
| `src/time.rs`, `src/error.rs` | timestamps, `CODE: message` errors |
| `src/domain/` | planning, scheduling, calendar, crowd, workout, coach, progression, trends, goals, activities, onboarding, body, ticks, extensions |
| `src/tools/` | the tool registry, grouped by area: `plan.rs`, `train.rs`, `progress.rs`, `exercises.rs`, `meta.rs`, `ops.rs`, `extensions.rs` |
| `src/providers/` | Google Calendar + OAuth, ICS feed, crowd sources, photo decoding/EXIF |
| `src/agent/` | OpenRouter client, per-turn context, media, failure handling, turn log |
| `src/runtime/` | `serve`: inbound routing, scheduler, outbox |
| `src/telegram/` | Bot API client, coach card renderer, Markdown |
| `src/web/` | the dashboard |
| `assets/workout-guide/` | exercise catalog and pictures; missing ones are drawn with `scripts/new-illustration` ([illustrations.md](illustrations.md)) |

## Rules that span modules

- **Typed tools only.** An earlier build exposed tools through a shell; the agent read source and ran
  ad-hoc queries on prod, then told the owner things the state contradicted.
- `sql_query` is read-only; the schema it reads is in the system prompt (`src/agent/context.rs`).
  There is no SQL write tool.
- One struct per tool implements `tools::Tool`, serving the agent, `gymclaw call` and the dashboard
  alike, so anything the agent does can be reproduced by hand.
- Mutations run in `domain::core::mutate` with a request ID: a retry returns the first result. The
  runtime derives the IDs (`tg-<message id>-<call index>`, `tg-cb:<callback id>`), never the model.
- A tool shows something in Telegram by pushing `Effect::Coach`, rendered after the commit. Every
  proactive message is a `notification_delivery` row (the outbox); nothing sends directly.
- Timestamps are naive UTC (`Ts`); input without an offset is the owner's local time.
- Env vars are read only in `src/config.rs` (clippy `disallowed_methods`).
- Turn guards (step cap, deadline, identical-call breaker, silence, failure line) live in
  `src/agent/mod.rs`: look there before adding one. Fix a misbehaving turn at its source (event,
  prompt, tool result), not with a filter, and pin it with a test next to the existing ones.
- Background events (ticks, triggers, reflection) are observations, never owner permission: on a
  background turn, `runtime_resume` and calendar writes refuse.
- `runtime_settings` gates sends and ticks, with separate grants for calendar writes and crowd polling.
- Conflicts, missed slots, stale workouts and week rollover become decision events for the agent,
  never automatic replans. Code-sent messages (workout summary card, Sunday recap) are numbers only.
- Reflection: a successful owner turn of 6+ tool calls, or one that hit a guard, raises an
  `agent.reflect` wake once the chat is quiet (at most 3 a day) to save or fix a playbook or note.
  A 👎 on the latest agent reply instead reaches the agent at once as an owner message.
- Schema: `src/db/schema.sql` is generated and creates fresh DBs; a change is a new
  `src/db/migrations/<NNN>_<name>.sql` plus its entry in `MIGRATIONS` (`src/db/mod.rs`).

Telegram behaviour that isn't obvious from one file: DMs with the owner only; a 👀 reaction on
receipt until the reply; `gc:` buttons go to the coach without the model, `btn:<id>` buttons reach
the agent as the owner's text; typed sets go to the coach first and fall back to the agent; one live
card per exercise, edited in place; one message per planned session, each reminder replacing the last.

## Where to look

| Question | File |
| --- | --- |
| What does the model see? | `src/agent/context.rs` |
| Why did a turn do X? | [turns.md](turns.md), `src/agent/turnlog.rs` |
| Where does an inbound message go? | `src/runtime/bot.rs` |
| What runs when? | `src/runtime/scheduler.rs`, `src/domain/ticks.rs` |
| A message is missing or sent twice | `src/runtime/outbox.rs` |
| Taps, typed sets, rest timer | `src/domain/coach/mod.rs`, `src/telegram/coach.rs`, `src/domain/set_parser.rs` |
| Next weights | `src/domain/progression.rs`, `src/domain/audit.rs`, `src/domain/trends.rs` |
| Week planning | `src/domain/planning.rs`, `src/domain/scheduling.rs`, `src/domain/replanning.rs` |
| The agent's own layer (playbooks, triggers) | `src/domain/extensions.rs` |
| Crowd | `src/domain/crowd.rs`, [crowd.md](crowd.md) |
| Schema and migrations | `src/db/mod.rs`, `src/db/schema.sql` |
