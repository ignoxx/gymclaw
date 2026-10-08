# Turn log

Local hook-only plugin that appends one JSON line per event to
`$GYMCLAW_TURN_LOG_DIR/YYYY-MM-DD.jsonl` (UTC date; `/opt/gymclaw/data/turns` in Docker). The data
is for a replay/eval set and hard latency numbers, nothing else. It is never sent anywhere.
Without `GYMCLAW_TURN_LOG_DIR` the plugin is off.

Logging can't break or slow a turn: handlers only read and queue a line, writes are async, every
error is swallowed, and if the disk stalls new lines are dropped after 1000 pending.

## Events

Every line has `ts` (ISO) and `ev`. `run` is OpenClaw's runId.

| `ev` | Hook | Fields |
| --- | --- | --- |
| `inbound` | `message_received` | `session`, `channel`, `msg`, `sentAt`, `chars`, `text` |
| `llm_input` | `llm_input` | `run`, `session`, `trigger` (`user`/`cron`/`heartbeat`…), `job`, `provider`, `model`, `history` (messages), `historyChars`, `systemChars`, `systemSha`, `tools`, `toolsChars`, `images`, `chars`, `prompt` |
| `model_call` | `model_call_ended` | `run`, `call`, `ms`, `ttfbMs`, `outcome`, `error`, `reqBytes`, `resBytes`, `budget` |
| `llm_output` | `llm_output` | `run`, `model`, `usage` (attempt total), `stop`, `error`, `budget` |
| `tool` | `after_tool_call` | `run`, `call`, `name`, `ms`, `ok`, `error`, `args`, `result` (`chars`, `sha`, `images`, `preview`) |
| `run_end` | `agent_end` | `run`, `session`, `trigger`, `ok`, `error`, `ms`, `calls` (per model call: `input`, `output`, `reasoning`, `cacheRead`, `cacheWrite`, `stop`, `toolCalls`), `noReply`, `reply` |
| `outbound` | `message_sent` | `session`, `msg`, `ok`, `error`, `text` |
| `fast` | coach plugin | `flow` (`tap`/`text`), `action` or `text`, `msg`, `handled` (text), `ok`, `error`, `cliMs`, `ms` |

Inbound/outbound hooks have no runId, so they join runs by `session` and time: an inbound message
belongs to the next `user` run in its session, an outbound one to the latest run started before it.
Typed sets the coach handled (`fast` with the same `msg`) never belong to a run.
Context size of a call is `input + cacheRead + cacheWrite`.

**Privacy.** Message text, prompts and replies are kept (up to 4000 chars) because they are the
replay set. Never logged: system prompt and history text (sizes and a hash only), secrets
(keys named like token/secret/password/api key/authorization/cookie, plus Bearer, `sk-…` and
Telegram bot token patterns are redacted), inline media (data URLs and long base64 become
`{bytes, sha}`). Tool args are capped at 1000 chars per string, results at a 1000-char preview.
Files are `0600` in a `0700` directory and are in `backup.sh` archives.

## Summary

```bash
node openclaw/plugins/turn-log/summarize.mjs [dir-or-files…] [--since YYYY-MM-DD] [--json]
```

p50/p95 wall time per flow (`model user/workout.log`, `fast tap log`, …), tokens per model turn,
model call latency and first byte, tool latency, model vs fast-path counts, and failures.

## Activation

[deploy/entrypoint.sh](../../../deploy/entrypoint.sh) installs and configures it on every start. To undo, disable the plugin entry and restart the Gateway. Delete the log directory to drop the data.

## Tests

```bash
node --test openclaw/plugins/turn-log/runtime.test.mjs
```
