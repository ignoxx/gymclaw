# GymClaw coach

Model-free workout flow in Telegram. Logging a set takes about a second and never wakes the agent.

- **Cards:** each exercise gets an illustration (first set only), the target, and buttons:
  `✅ 40 kg × 10` (repeat last set), `🔄 Swap`, `⏭ Next exercise`.
- **Typed sets:** `40x10`, `12x40kg`, `9 reps at 80` are claimed in `before_dispatch`, before the agent
  sees them (`inbound_claim` only fires for plugin-bound chats). The previous card gets `✅ 40 kg × 10`
  and the next card follows. Order is guessed from units, then from the expected weight.
- **Rest:** the next card arrives right away with `⏱ Rest 1:25 · next up` on top, edited every 5 s.
  At zero the plugin closes the rest and sends a fresh card with buttons (edits don't notify). The
  runtime cron ping is only a fallback 30 s later, e.g. after a gateway restart mid-rest.
- **Swap** (only before the first set of an exercise): up to two same-muscle alternatives with
  images, plus `⏳ I'll wait` and `↪ Do it later`. "Later" keeps the workout on the same muscle group.
- **Agent tool:** `gymclaw_workout` (status, start, log, card, swap, later, next, end), so chat requests produce
  the same cards.

All workout logic lives in Python (`gymclaw/services/coach.py`, `gymclaw-tool coach …`). This plugin
only moves messages. Button data is `gc:<action>:<exercise-ref>…`; old buttons answer "⌛ Old button."

Telegram send/edit/react come from the installed OpenClaw Telegram runtime (`telegram.mjs`), which
is not a public SDK path. Reviewed against OpenClaw 2026.7.1. If an upgrade moves it, the plugin
fails loudly on first use.

## Activation

```bash
openclaw plugins install --link /sandbox/.openclaw/workspace/gymclaw/openclaw/plugins/coach
```

Hash-checked `config.patch` (preserve existing entries):

```json
{
  "plugins": {
    "allow": ["…existing…", "gymclaw-coach"],
    "entries": {
      "gymclaw-coach": {
        "enabled": true,
        "config": { "ownerId": "OWNER_TELEGRAM_ID", "tool": "/sandbox/.openclaw/workspace/gymclaw/scripts/gymclaw-tool" }
      }
    }
  },
  "tools": { "alsoAllow": ["gymclaw_workout"] },
  "channels": {
    "telegram": {
      "capabilities": { "inlineButtons": "dm" },
      "actions": { "reactions": true },
      "reactionLevel": "extensive",
      "streaming": { "mode": "off" }
    }
  }
}
```

Restart the Gateway, then `openclaw plugins inspect gymclaw-coach --runtime --json`. To undo,
disable the entry and restart. Workout data is untouched.

## Tests

```bash
node --test openclaw/plugins/coach/runtime.test.mjs
```

Fakes only: no Telegram, no CLI, no model.
