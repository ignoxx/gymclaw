# diff-replay

Runs the same owner operations through the Rust tools (`gymclaw call …`) and the Python CLI (the
oracle) on two fresh copies of a real database, then compares the results and the affected tables.

```sh
scripts/diff-replay ~/gymclaw-backups/<backup>.db [--python ~/Projects/gymclaw/.venv/bin/python] [--keep]
```

- The backup is only read; each side works on its own copy in a temp dir, deleted afterwards
  (`--keep` keeps it and prints the path).
- Fixed `--now` (2026-10-12 21:00 Berlin) and fixed request ids. The Rust copy gets `gymclaw migrate`
  first, like a deploy.
- No network: only local operations (no calendar sync or publish, no crowd poll, no model).
- Normalized: new UUIDs and the sha256 calendar event ids derived from them become `<new:N>`, the
  `ok`/empty envelope fields are dropped, and so are `created_at`/`updated_at`/`handled_at`. Key
  order is ignored. Table rows are compared as multisets.
- Operations: get week, move a session, add a session, skip, set workout, plan week, profile get and
  update, availability add/list/remove, crowd status and forecast, events pending, then start a
  workout, log two sets and finish.
- The script hardcodes two session ids from the 2026-10-12 backup (`PULL`, `LEGS`). Update them when
  you replay a newer backup.

## Last result (backup gymclaw-20261012-190132.db, 2026-10-14)

Every table is identical after the replay: `planned_session`, `availability_block`, `user_profile`,
`workout_session`, `workout_exercise`, `set_log`, `notification_job`, `exercise_progression` and
`crowd_feedback`. That covers the add-session fix from PR #29.
Results are identical for get week, move, add, skip, set workout, profile get, and availability
add/list/remove.

Differences, all intended (the tool contract differs by design):

| Where | Difference |
|---|---|
| plan week | Rust `plan_week` also queues the Google writes for the new sessions, like Python's `calendar plan-week`. The script uses Python's `schedule plan-week`, because `calendar plan-week` needs the calendar provider. That gives 3 extra `calendar_write` rows and 3 `calendar.write_queued` events on the Rust side. |
| plan week | `choices[*]` timestamps: Rust `+00:00`, Python pydantic `Z`; same instants. One score differs in the last float digit (summation order). |
| profile update | Python replans and returns `replanning`; the Rust tool only updates the profile and tells the agent to call `replan_week`. On this data the Python replan changed nothing (warnings only). See PARITY.md. |
| crowd status | The Rust tool adds `polling` health next to the source list. |
| crowd forecast | Rust returns `{forecast, prediction}`; Python `predict` returns the bare prediction. The prediction values are identical. |
| events pending | `{"events": []}` vs a bare `[]`. |
| workout start / log / finish | Rust returns `{ack, card}` (the card text; Telegram sends the card). Python `coach` returns the full card objects (buttons, photo, chat_id, react). Card texts are identical. |

Bugs found: none.
