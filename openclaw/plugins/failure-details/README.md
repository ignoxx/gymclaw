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

Linked installation in the managed sandbox:

```bash
openclaw plugins install --link /sandbox/.openclaw/workspace/gymclaw/openclaw/plugins/failure-details
```

Use hash-checked Gateway `config.patch` to add `gymclaw-failure-details` to the
existing `plugins.allow` list (preserve other entries), and set:

```json
{
  "plugins": {
    "entries": {
      "gymclaw-failure-details": {
        "enabled": true,
        "hooks": { "allowConversationAccess": true }
      }
    }
  }
}
```

Conversation permission is required for `llm_input`, `llm_output`, and `agent_end`
even though the plugin consumes only run/usage/error metadata, not prompt text.
Restart through `nemoclaw gymclaw gateway restart`, then verify
`openclaw plugins inspect gymclaw-failure-details --runtime --json`: six typed
hooks, no blocked-hook diagnostics. Disable the plugin entry and restart to undo;
no transcript edits or deletion needed.

## Tests

```bash
node --test openclaw/plugins/failure-details/runtime.test.mjs
```

Tests are synthetic: no model calls, Telegram messages or domain mutations.
