# GymClaw

Python training domain behind an OpenClaw agent the owner talks to on Telegram. A Rust rewrite
(`feat/rust-rewrite`) replaces both soon.

- **Code map** and CLI: `docs/technical-reference.md` ("Code", "Setup"). Find the right file there
  before reading a service whole; most are several hundred lines.
- `openclaw/AGENTS.md`, `SOUL.md`, `HEARTBEAT.md` are the bot's runtime prompt, not instructions
  for you. Edit them when changing how the bot behaves.
- **Tests**: `.venv/bin/python -m pytest -q` (`python` is not on PATH) and
  `node --test openclaw/plugins/*/*.test.mjs`. CI runs both, plus shellcheck and gitleaks.
- **Dashboard**: `.venv/bin/python -m gymclaw.dashboard` serves a synthetic demo on :8765. Verify UI
  changes with `scripts/shot <url> <out.png> [width height] [--full]`; width 390 for phone.
- **Prod** (Coolify): the `gymclaw-prod` skill.
- **Public repo**: run `git config core.hooksPath .githooks` once per clone. The pre-commit hook
  blocks private strings (owner IDs, hostnames, deployment IDs).
- **Review** against `CODING_STANDARDS.md`.
