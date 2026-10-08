# Failure details

Local hook-only plugin for the dedicated GymClaw Gateway. Rewrites OpenClaw's
`Agent couldn't generate a response` Telegram fallback with a sanitized cause,
output/reasoning usage when available, a short run reference, and retry warning.
Normal replies and other channels are unchanged. No extra messages or retries.

Classifies output exhaustion, context overflow, provider quota/auth, timeout,
and upstream failure. Unknown causes remain explicitly unknown. Raw errors,
prompts, tool arguments/results are never included in messages. Diagnostic state
is held in memory for up to 10 minutes; overlapping runs sharing one session
fall back to unknown rather than misattribute evidence. This does not diagnose
Telegram delivery failures or failures where the Gateway cannot send any reply.

## Activation

[deploy/entrypoint.sh](../../../deploy/entrypoint.sh) installs and configures it on every start. To undo, disable the plugin entry and restart the Gateway.

## Tests

```bash
node --test openclaw/plugins/failure-details/runtime.test.mjs
```

Tests are synthetic: no model calls, Telegram messages or domain mutations.
