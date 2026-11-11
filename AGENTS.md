# GymClaw

One Rust binary: Telegram bot, agent loop, scheduler and training domain over one SQLite DB.

- **Code map**: `docs/architecture.md` ("Layout"). Schema: `src/db/schema.sql` (generated; a test
  rewrites it when a migration changes it). Self-hosting and local runs: `docs/self-hosting.md`.
- The bot's system prompt is built in `src/agent/context.rs`; its tools are in `src/tools/`. Before
  proposing a fix to the bot's behaviour, read `CODING_STANDARDS.md` ("The agent is the brain",
  "Owner complaints get the general capability").
- **Build**: the default `heic` feature needs system libheif ≥ 1.17; without it, pass
  `--no-default-features` to cargo. CI builds the default features.
- **Tests** live inline in `#[cfg(test)]` modules (no `tests/` dir); shared fixtures in
  `src/db/test.rs`. CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, shellcheck and gitleaks.
- **Dashboard**: `cargo run -- dashboard` serves `data/gymclaw.db` on :8766 (`GYMCLAW_DB=…` for
  another copy). Debug builds read `web/` from disk, so a refresh shows edits. Verify UI changes with
  `scripts/shot <url> <out.png> [width height] [--full]`; width 390 for phone;
  `--click`/`--fill`/`--text` steps drive dialogs and forms (see its header).
- **Public repo**: run `git config core.hooksPath .githooks` once per clone. The pre-commit hook
  blocks private strings (owner IDs, hostnames, deployment IDs).
- **Review** against `CODING_STANDARDS.md`.

## Glossary (owner's word → code)

- **owner**: the one Telegram user (`GYMCLAW_TELEGRAM_USER_ID`). **bot**: the whole binary
  (`gymclaw serve`). **agent**: the LLM loop in `src/agent/`. **coach**: the model-free workout
  loop (taps, typed sets, rest timer) in `src/domain/coach.rs` + `src/telegram/coach.rs`.
- **session**: a planned slot, `planned_session`. **workout**: what's actually done and logged,
  `workout_session` (+ `workout_exercise`, `set_log`).
- **workout plan / template**: one workout's exercises and targets, `workout_template`. **the plan**:
  the owner's words, `onboarding_state.plan_text`; its templates in order (`split_json`) are the
  default **rotation** for new sessions.
- **event**: an `agent_event` row; decision events wake the agent (`src/domain/events.rs`). **notes**:
  `agent_note`, owner or learned, in every prompt.
- **delivery / outbox**: every proactive message is a `notification_delivery` row
  (`src/runtime/outbox.rs`); timed reminders are `notification_job` rows.
