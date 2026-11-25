# gymclaw

[![Tests](https://github.com/ignoxx/gymclaw/actions/workflows/tests.yml/badge.svg)](https://github.com/ignoxx/gymclaw/actions/workflows/tests.yml)
[![License: BSL 1.1](https://img.shields.io/badge/license-BSL%201.1-blue)](LICENSE)

An AI gym coach that adapts to you.

![gymclaw: the last workout app you'll ever need](assets/hero.png)

> **Work in progress.** I use it every day, and it has rough edges. Expect breaking changes.

## What it does

- Plans the week into the quietest gym slots that fit my calendar and rest days, and replans when
  meetings, trips or sick days get in the way, or when I ask in chat.
- Reminds me when to get ready and when to leave, with the current crowd.
- Coaches the workout set by set: exercise picture, target, one-tap or typed sets (`10x80`), rest
  timer, swaps to a same-muscle exercise.
- Raises targets when I hit all my reps, and sends a weekly recap every Sunday.
- Tracks body weight from scale photos.

<p align="center">
  <img src="assets/screens/chat.png" width="30%" alt="The chat mid-workout: exercise card and rest timer">
  <img src="assets/screens/live.png" width="30%" alt="The app mid-workout: rest timer and set logger">
  <img src="assets/screens/progress.png" width="30%" alt="Progress: sessions per week and estimated 1RM">
</p>

## How it works

One Rust binary over one SQLite file ([architecture](docs/architecture.md)). An LLM agent (via
OpenRouter) handles the conversation and the decisions, using typed tools that own all state and
validate every action. A model-free coach handles taps and sets during a workout, and an in-process
scheduler runs the calendar feed refresh, crowd polling, reminders and the Sunday plan.

## Running your own

You need a Telegram bot and an OpenRouter key. Calendars (a read-only workout feed to subscribe to,
and your own calendar as busy time) and gym check-in counts ([crowd data](docs/crowd.md)) are optional. Then
`docker compose up -d` and follow [self-hosting](docs/self-hosting.md).

## License

[Business Source License 1.1](LICENSE): free to clone, self-host and modify for noncommercial use;
commercial use needs a license from the author. Each release becomes Apache 2.0 four years after
it is published.
