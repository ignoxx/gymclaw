# GymClaw coach

Model-free workout flow in Telegram. Logging a set takes about a second and never wakes the agent.

- **Cards:** every set card has the illustration, the target and buttons:
  `✅ 10 × 40 kg` (repeat last set, reps first), `🔄 Swap` (before the first set only), `⏭ Skip` before the
  first set / `⏭ Next exercise` after it. With no weight known: `✍️ Reply reps × weight` plus, when history
  has the same movement or a similar exercise (same muscle and equipment), a one-tap guess `✅ 8 × 40 kg?`.
- **Typed sets:** `10x40`, `12x40kg`, `9 reps at 80` are claimed in `before_dispatch`, before the agent
  sees them (`inbound_claim` only fires for plugin-bound chats). The message gets 👍 (its ID comes from
  `message_received`), the set card gets `✅ 10 × 40 kg`. Order comes from units, then the expected weight, then reps-first.
- **Rest** (between sets of one exercise only): a small `⏱ 1:25 until Bench set 2/2` message with
  `⏭ Skip`, edited every 5 s. At zero (or Skip) it is deleted and a fresh set card is sent, which
  notifies the phone. A new exercise starts right away. The runtime cron ping is only a fallback 30 s
  later, e.g. after a gateway restart mid-rest.
- **Swap:** the card's buttons become `↩ Keep <exercise>` / `↪ Do it later` and up to two same-muscle
  alternatives appear below it. Choosing one deletes the options and the old card, and the swap is
  saved in the template (the old exercise stays as an alternative). Keep restores the card.
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
