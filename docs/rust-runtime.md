# GymClaw in Rust: design and porting rules

One binary replaces OpenClaw and the Python domain. It owns Telegram, the agent loop, the
scheduler, the outbox and the training domain, all over the existing `data/gymclaw.db`.

```
gymclaw serve        # bot: Telegram long-poll, agent, coach, scheduler, outbox
gymclaw call <tool> '<json>' [--now …] [--request-id …]   # any agent tool, for ops and testing
gymclaw tools        # tool names, descriptions, JSON schemas
gymclaw migrate      # schema (baseline = last alembic head, then src/db/migrations)
gymclaw backup [dest]
gymclaw dashboard    # read-only dashboard
```

## Why

On 2026-10-12 the agent was asked to move one session and leave the rest alone. The shell-based
tool boundary let it read `--help`, grep source and run ad-hoc Python on the prod DB, and it told
the owner something the state contradicted. The rewrite gives the model typed tools only, a small
context, and a way past missing verbs that doesn't involve the source code (see "Agent").

## Layout and ownership

| Path | What | Owner |
| --- | --- | --- |
| `src/db/`, `src/time.rs`, `src/error.rs`, `src/config.rs`, `src/main.rs`, `src/lib.rs` | rows, timestamps, errors, CLI | lead |
| `src/tools/mod.rs`, `src/tools/meta.rs` | tool registry, SQL escape hatch, notes | lead |
| `src/domain/{core,view,profile,templates,set_parser,illustrations,progression}.rs` | shared domain leaves | lead |
| `src/domain/{planning,scheduling,replanning,calendar,calendar_writes,calendar_busy,notifications,availability,weekly,habits,compression,personal_calendar,crowd,events,ticks}.rs`, `src/tools/plan.rs` | planning side | lead |
| `src/domain/{workout,adaptation,coach,audit,backfill,preview,body,onboarding}.rs`, `src/tools/train.rs` | training side | WP-train |
| `src/providers/` | Google Calendar + OAuth, ICS feed, crowd sources, photo decoding/EXIF | WP-providers |
| `src/runtime/`, `src/telegram/`, `src/agent/`, `src/web/`, `src/tools/ops.rs`, `deploy/`, `docker-compose.yaml` | serve, dashboard, deploy | WP-runtime |

Only edit files you own. Functions other areas call are already declared with their signature and a
`todo!()` body ("contract"); the owner fills the body and keeps the signature. If you need a new
cross-area function, declare it in your own file, or ask the lead. `Cargo.toml`: add the crates
you need; the lead resolves merge conflicts there.

## Porting rules

- **Python is the spec.** Port behavior faithfully, including messages, error codes, JSON field
  names and event types (`agent_event.type`, payload keys). The owner's habits and the dashboard
  depend on them. Improve structure, not behavior, unless this doc says otherwise.
- **Same DB.** Rows go through `crate::db::models` (`get/select/first/insert/update/delete`). No
  autoflush: after changing a loaded row, call `.update(db)`. Timestamps are `Ts` (naive UTC text
  in the DB, Python `isoformat()` in JSON). New ids: `db::new_id()` (uuid4).
- **Types over strings.** Status columns are enums (`SessionStatus`, `WorkoutStatus`, …); match on
  them. Parse JSON into structs where the shape is fixed (templates, coach views).
- **Errors.** `crate::bail!("CODE", "message")` for domain failures; keep Python's codes and texts.
- **Mutations** use `domain::core::mutate(db, operation, request_id, intent, now, || …)` exactly
  where Python used `mutate`, so retries stay idempotent.
- **Tests only when they earn it.** The compiler covers shape. Port a Python test when it pins
  down real behavior that is easy to get subtly wrong (planner choices, set parsing, progression
  math, rest timing, stale-delivery rules). Keep them short and literal. No smoke tests.
- Comments: short doc comment on public functions saying how they are used. Keep them true.
- `cargo clippy --all-targets -- -D warnings` and `cargo test` pass before you hand back.
- Never touch production or the real DB. Test against copies (`/tmp/...`) only.

## Tools

One struct per tool implementing `tools::Tool` (`NAME`, `DESCRIPTION`, `run(self, &mut Ctx)`),
registered in the owning `tools/*.rs`. The same tool serves the agent and `gymclaw call`.
Args are `#[derive(Deserialize, JsonSchema)]` with doc comments (they become the schema the model
reads). Results are JSON the model reads; mutations return the `mutate` envelope.

The runtime derives `request_id` (`tg-<message id>-<call index>`, `tg-cb:<callback id>`), so the
model never has to. Timestamps without an offset are the owner's local time
(`Ts::parse(text, Some(&profile.tz()))`).

Tool surface = the old CLI + `gymclaw_workout` actions, as intent-level verbs
(`move_session`, `add_session`, `skip_session`, `set_session_workout`, `get_week`, `workout`, …).
Mutations that touch other sessions return `side_effects` so a reply can't contradict the state.
Tools that show something in Telegram push `Effect::Coach(CoachView)`; the runtime renders it after
the transaction commits.

## Agent ("adapt to me")

- System prompt: short. Who the owner is, voice (old `openclaw/SOUL.md`), the few rules that protect
  data and trust, and the owner's notes. No tool manual: tool descriptions carry that.
- Context per turn: system prompt + state snapshot (local time, profile, this week's sessions,
  running workout, open events) + notes + the last ~30 `chat_message` rows. Target < 15k tokens.
- Escape hatch: `sql_query` (read anything but secrets) and `sql_write` (DB snapshot first, audited
  as `agent.sql_write`). When no verb fits, the model does it with SQL instead of telling the owner
  "can't". `note_add {kind: lesson}` lets it write down a workaround for next time. Lessons and owner
  notes are in every prompt, which is how it improves itself.
- One turn ends with one reply: text (+ optional buttons) or a reaction or nothing. Tool errors go
  back to the model. Step cap (12 calls) and an identical-call breaker; on giving up, one sanitized
  line to the owner.
- Models via OpenRouter: `GYMCLAW_MODEL` (chat, default GLM), `GYMCLAW_MODEL_FAST`,
  `GYMCLAW_MODEL_VISION`; `GYMCLAW_PROVIDER` pins the provider like today (`reasoning.effort: minimal`).

## Telegram parity (everything OpenClaw + plugins did)

- DM with the owner only (`GYMCLAW_TELEGRAM_USER_ID`); everything else ignored.
- 👀 ack reaction on receipt, removed when the reply is sent; typing indicator while working.
- Two button kinds: `gc:<action>:<ref>` → coach (`coach::handle_tap`), no model. Agent buttons
  → `btn:<id>` with the value in `agent_button`; a tap is fed to the agent as the owner's text.
- Typed sets (`10x40`, `13 reps`) → `coach::handle_text` first; `handled: false` → agent.
- Coach card renderer: port `openclaw/plugins/coach/runtime.mjs` `createCoach` (live card,
  extras, 5 s rest countdown edits, retire/delete, `refresh`/`keep`/`restore`/`standalone`/swap).
- Photos/documents in (HEIC included) → vision; EXIF capture date for scale backfill.
- `/new` clears the chat window; `/status` shows runtime health.
- Every outbound message (agent, coach, reminders, briefings) goes into `chat_message`, so the agent
  sees what the owner sees.
- Per-owner serialized turns; messages arriving during a turn are collected into the next one.
- Turn log: same JSONL events as `openclaw/plugins/turn-log` in `data/turns/`.

## Scheduler and outbox

- In-process: `ticks::watch` every 60 s, `ticks::weekly` Sunday 19:00 profile-local, `ticks::poll_crowd`
  every 15 min when enabled, `ticks::publish` after ticks when calendar writes are enabled.
- One-off jobs stay in `notification_job` (GET_READY, LEAVE, SESSION_START, REST). One loop sleeps
  until the next `due_at` (or a wake-up after any write) and fires them like `runtime fire` did.
  REST jobs fire 30 s late as a fallback; the coach's live countdown normally handles them.
- Outbox = `notification_delivery`, semantics unchanged: commit PENDING → claim SENDING → send →
  SENT with the Telegram message id; any send error → UNKNOWN, never retried automatically.
- `runtime_settings.enabled` still gates sends; calendar writes and crowd polling keep their flags.
