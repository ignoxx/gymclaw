# One reply

Local hook-only plugin: at most one owner message per agent run.

Models narrate between tool calls ("The planner picked 14:00, fixing to 12:30:"). OpenClaw delivers
that leftover text as extra Telegram replies even after the `message` tool already answered and the
run ended with `NO_REPLY`, so one request could get three replies.

A run counts as answered after a successful `message` send, a `gymclaw_workout` action that sends a
card (anything but `status`), or its first delivered reply payload. Later `reply_payload_sending`
payloads of that run are cancelled. Payloads without a `runId` (reminders, outbox deliveries,
recovered replays) are never touched. State is in memory for 10 minutes.

## Tests

```bash
node --test openclaw/plugins/one-reply/runtime.test.mjs
```
