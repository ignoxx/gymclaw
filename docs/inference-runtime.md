# Local inference deadline workaround

Pinned NemoClaw v0.0.124 (`6f3cced`) applies a **30-second total upstream
response deadline**, including actively streaming responses. A longer GLM response
is cut off after tool actions may already have succeeded.

Observed failure: assistant error `response truncated: upstream read error`;
OpenRouter adapter telemetry `status=504`, `durationMs=30003`. This was not exercise
catalog seeding, context overflow, or an invalid key.

Our local workaround changes only upstream deadline to **300 seconds**. Body
limits, request-body deadline, authentication, network policy and finite timeout
remain unchanged. No inference/model fallback or automatic action replay added.

## Native vision metadata

Public GLM-5.3-flash catalog advertises image inputs and 1M context, but pinned
NemoClaw initially declared text-only/131k. Dedicated Gateway model metadata was
corrected through supported `config.patch` API: `input: [text, image]`,
`contextWindow: 1048576`. Auth, route, max output and calendar authority unchanged.
Synthetic image digits were recognized natively, with no tools/OCR or fitness writes.

Context correction: the generated agent catalog and Telegram session still reported
131072 tokens despite the main model override. Set dedicated Gateway
`agents.defaults.contextTokens: 1048576` through hash-checked `config.patch`, then
refresh the catalog with `openclaw models list --json`. Verified model catalog and
live Gateway defaults both report 1048576; existing session usage metadata may
retain the previous limit until its next agent turn. No session reset required.

Review these metadata overrides after NemoClaw rebuild/upgrade. Agent policy forbids
unrequested OCR/install loops; resend attachments created under text-only metadata.

## Output budget and transparent failures

The generated NemoClaw model entry capped output at 4096 tokens. GLM reasoning
consumes that same budget: multiple observed failures used all 4096 tokens on
reasoning, returned no visible answer, and stopped with `length`. This is separate
from input context capacity. OpenRouter's live GLM-5.3-flash catalog advertises
completion capacity above 16384 tokens.

Dedicated Gateway model `maxTokens` and
`agents.defaults.models["inference/z-ai/glm-5.3-flash"].params.maxTokens` now both
use **16384**. This remains a finite cost/latency guardrail, not a guarantee that
reasoning always completes. The 300-second upstream deadline remains unchanged.
To revert the budget, patch both fields back to 4096, refresh the model catalog,
and reload the dedicated Gateway.

Installed local [failure-details plugin](../openclaw/plugins/failure-details/README.md)
rewrites generic Telegram failures with sanitized cause, usage, run reference,
and action/retry warning. Six hooks verified loaded without permission warnings;
five focused synthetic tests pass on host and sandbox. No failed actions replayed
or artificial Telegram errors sent. Review budget and plugin activation after a
NemoClaw rebuild/upgrade.

## Apply / revert

These scripts target the exact pinned installation and refuse other revisions.
Host adapter is shared by OpenRouter sandboxes; only `gymclaw` exists here. Review
scope before using them on any other installation. No credential files are opened
by scripts; restart delegates opaque authorization-state handling to native lifecycle.

```bash
python3 scripts/patch-nemoclaw-timeout --source "$HOME/.nemoclaw/source"
node scripts/test-nemoclaw-timeout.cjs "$HOME/.nemoclaw/source"
node scripts/restart-nemoclaw-adapter.cjs "$HOME/.nemoclaw/source"
```

To revert only our two deadline declarations:

```bash
python3 scripts/patch-nemoclaw-timeout --source "$HOME/.nemoclaw/source" --revert
node scripts/restart-nemoclaw-adapter.cjs "$HOME/.nemoclaw/source"
```

Use isolated GymClaw Node runtime on macOS. Reapply/review workaround after upgrades;
no automatic patch of unknown versions. Fixtures use localhost only: 31-second
stream must finish; explicit short deadline must still return redacted 504.

After a failed model reply, verify saved state before continuing. Never rerun a
mutation blindly. `replayInvalid=true` after exec means unsafe to auto-replay, not
necessarily unsuccessful completion; abandoned/incomplete terminal states still
require investigation. One artificial repeated-read probe ended `incomplete_turn`;
subsequent single read completed. Timeout fix does not claim to cure every model
or agent-harness failure.
