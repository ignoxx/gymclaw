# GymClaw

One Rust binary: Telegram bot, agent loop, scheduler and training domain over one SQLite DB.

- **Code map**: `docs/rust-runtime.md` ("Layout"). CLI and local runs: `docs/technical-reference.md`.
- The bot's system prompt is built in `src/agent/context.rs`; its tools are in `src/tools/`. Before
  proposing a fix to the bot's behaviour, read "Owner complaints get the general capability" in
  `CODING_STANDARDS.md`.
- **Tests**: `cargo test` and `cargo clippy --all-targets -- -D warnings`. CI runs both, plus
  shellcheck and gitleaks.
- **Dashboard**: `cargo run -- dashboard` serves `data/gymclaw.db` on :8766 (`GYMCLAW_DB=…` for
  another copy). Verify UI changes with `scripts/shot <url> <out.png> [width height] [--full]`;
  width 390 for phone; `--click`/`--fill`/`--text` steps drive dialogs and forms (see its header).
- **Public repo**: run `git config core.hooksPath .githooks` once per clone. The pre-commit hook
  blocks private strings (owner IDs, hostnames, deployment IDs).
- **Review** against `CODING_STANDARDS.md`.
