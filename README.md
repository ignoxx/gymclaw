# GymClaw

A Telegram agent that plans your training week and tells you what to do next at the gym.

![GymClaw dashboard](assets/dashboard-demo.png)

The dashboard shows a synthetic demo. Telegram is the daily interface.

[One week with GymClaw (60 s)](videos/gymclaw-story-60s.mp4) · [Feature tour (60 s)](videos/gymclaw-tour-60s.mp4) · [Motion reel (15 s)](videos/gymclaw-reel-15s.mp4)

Telegram screens and data in the videos are simulated. See [video notes](videos/README.md).

## What runs over time

- Plans workouts around your calendar, travel time and recovery days.
- Polls gym attendance every 15 minutes within your training window.
- Guides warm-ups and sets, with exercise images and rest reminders.
- Saves progress in SQLite and adapts future workouts.
- Replans around calendar edits, travel and illness.

OpenClaw handles conversation and scheduled jobs inside NemoClaw. Python tools validate actions and persist state. Current inference uses OpenRouter/GLM.

## Try it

Python 3.12 or newer. The demo needs no credentials.

```bash
python3 -m venv .venv
source .venv/bin/activate
pip install -e '.[dev]'
python -m gymclaw.dashboard
```

Open http://127.0.0.1:8765. Click exercise thumbnails to preview poses.

The demo runs real planning and workout code in a disposable DB. It opens no private DB, calls no live API and sends no messages.

For your existing local NemoClaw installation:

```bash
python -m gymclaw.dashboard --live
```

Open http://127.0.0.1:8766. This reads saved calendar, attendance, workout and runtime state from the authoritative sandbox. Use the refresh button after actions in Telegram. It reads SQLite in read-only mode and starts no new poller. Private views stay on localhost.

## Current status

Live calendar reads, scheduled Telegram test delivery and MySports polling work. Native vision and resumable onboarding are implemented. Calendar writes stay off during testing.

Real workout, rest reminder and restart acceptance still need owner verification. Google Popular Times is not connected. Reported counts can lag and are not occupancy percentages. VPS deployment and complete backup/restore remain unfinished.

Exercise art comes from [Workout Guide](https://github.com/bryllim/workout-guide), Bryl Lim and Everkinetic under [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/). GymClaw bundles all 302 first-pose SVG/PNG pairs. See [asset notice](gymclaw/assets/workout-guide/NOTICE.md).

## Setup and code

[Agent setup](openclaw/README.md) · [CLI reference](docs/technical-reference.md) · [Onboarding](docs/onboarding.md) · [Crowd data](docs/crowd.md) · [Inference](docs/inference-runtime.md) · [Spec](SPEC.md) · [Security](SECURITY.md)

Run domain tests with `pytest -q`. Keep credentials, gym identifiers, plans and DB backups out of Git. License for GymClaw's own code is not yet selected.
