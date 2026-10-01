# OpenClaw activation handoff

Prepared, **not activated**. No OpenClaw/NemoClaw install, runtime config changes,
Telegram sends or calendar publication were performed for this phase.

## Readiness

Read-only inspection: macOS arm64, Docker server ready, Node 25.2.1,
OpenClaw/NemoClaw absent. Current OpenClaw docs require Node **24.16+ or 26.1+**
(recommend 26); Node 25 is outside documented support. Install/choose supported
Node only after owner approval. Do not change another project's runtime.

Architecture: OpenClaw scheduler → exact Python CLI argv → private SQLite domain
transaction/outbox → OpenClaw Telegram message CLI → persisted delivery receipt.
No separate Python agent runtime or sleep/poll daemon. JSON callbacks use
`--no-deliver` so scheduler does not also announce raw JSON or duplicate messages.

## Approval boundaries

1. Install/runtime configuration approval: dedicated **gymclaw** profile only.
2. Telegram activation approval: private verified owner ID, bot token configured
   locally; proactive messages enabled only with `--allow-messages`.
3. Calendar-write approval: separately inspect plan, then `calendar publish
   --allow-writes`. Watcher currently **never publishes** queued calendar changes.

Do not paste bot tokens, API keys or OAuth credentials into chat/Git. Bot token
belongs to local OpenClaw config or regular private `tokenFile` (0600, not symlink).
Profile and Telegram numeric user ID are routing identifiers, not secrets.

## Direct OpenClaw path (after approval)

Use dedicated profile instead of daily-driver state. Commands below are a handoff,
not a script to run unattended. Recheck installed CLI `--help`, docs and version
before activation; contracts below are researched/fixture-tested, not live-tested.

1. Choose supported Node, install reviewed OpenClaw release. Registry currently
   reports `2026.9.7`; verify maintained release before choosing. No install was run.
2. Run `openclaw --profile gymclaw onboard` locally. Choose provider/model and enter
   auth locally. NVIDIA direct provider uses `NVIDIA_API_KEY`; NemoClaw instead
   uses `NVIDIA_INFERENCE_API_KEY`. Do not put keys in command arguments/history.
3. Pin workspace to this repository's **absolute** `openclaw/` path:

   ```text
   openclaw --profile gymclaw config set agents.defaults.workspace '"ABSOLUTE_REPO_PATH/openclaw"'
   openclaw --profile gymclaw config set agents.defaults.skipBootstrap true
   ```

   On multi-agent installs, pin per-agent workspace under `agents.entries.ID.workspace`
   instead. Prefer sole dedicated agent for MVP; avoid modifying another profile.
4. Configure Telegram locally via supported setup UI or `tokenFile` setting.
   If ID unknown, use default pairing, DM bot, read your numeric ID in pairing reply.
   Approve only owner; then replace pairing with explicit numeric allowlist:

   ```text
   openclaw --profile gymclaw config set channels.telegram.dmPolicy allowlist
   openclaw --profile gymclaw config set channels.telegram.allowFrom '["OWNER_ID"]'
   openclaw --profile gymclaw config set channels.telegram.groupPolicy disabled
   openclaw --profile gymclaw config set channels.telegram.configWrites false
   openclaw --profile gymclaw config set commands.ownerAllowFrom '["telegram:OWNER_ID"]'
   ```

   Set allowFrom before switching policy if installed version rejects transient
   empty allowlist. Set account ownership binding to actual agent ID; current
   OpenClaw requires resolvable owner even for sole Telegram account. For `main`:

   ```json
   {"agentId":"main","match":{"channel":"telegram","accountId":"default"}}
   ```

   Merge into dedicated profile's `bindings`, never replace unrelated bindings.
   Do not run `getUpdates` alongside Gateway polling. No public/group access.
5. Keep Gateway loopback-only; choose unused port (e.g. 18790) for this profile.
   First use foreground Gateway; service installation is separate approval.
   Verify `channels status --probe`, agent/workspace ownership and `skills check`.
6. Configure exec access deliberately. Workspace is **not** filesystem isolation.
   Allowlisting generic Python gives broad code execution; do not call it a sandbox.
   Prefer scoped OS user/sandbox with repo/private DB available. Command payloads
   run on Gateway host as operator-authored admin work and **bypass agent exec
   approvals**; review exact absolute argv with `runtime plan` first.
7. From repo root, upgrade schema and preview executable jobs without network:

   ```bash
   .venv/bin/python -m gymclaw.cli db init
   .venv/bin/python -m gymclaw.cli runtime plan --telegram-id OWNER_ID
   ```

   Callbacks pin absolute Python/DB/cwd paths. Existing OAuth-bound DB supplies
   dedicated calendar ID; no `.env` read by wrapper or callbacks. Never mix demo
   DB/provider and live calendar. Path changes require explicit job review.
8. Only after explicit proactive-message approval:

   ```bash
   .venv/bin/python -m gymclaw.cli runtime sync --telegram-id OWNER_ID \
     --allow-runtime-changes --allow-messages
   ```

   Installs cheap 60s calendar watcher and exact one-shot pending reminders/rest
   jobs. After each logged set, agent invokes same sync immediately. Stable
   declaration keys + DB-path namespace recover lost creation responses. Sync
   cancels old timers without touching foreign jobs. Disabled/drifted jobs need
   operator review, not silent re-enablement. Future notifications fire at true
   due time; overdue rest jobs catch up, prep/leave/start expire after 15 min.
9. Set heartbeat to 30m with private explicit owner target. Current OpenClaw no
   longer reads HEARTBEAT.md: copy checklist to system-owned monitor scratch:

   ```text
   openclaw --profile gymclaw automations list --all --json
   openclaw --profile gymclaw automations scratch HEARTBEAT_JOB_ID --file ABSOLUTE_REPO_PATH/openclaw/HEARTBEAT.md
   ```

   Choose correct heartbeat job ID from listing, not a fabricated ID. With
   `lightContext: true`, full workspace/skill is absent; leave false unless
   scratch includes all required tool instructions. Heartbeat is not a rest timer.

Update USER.md activation line only after approvals, so agent can distinguish
permission from setup instructions. Weekly autonomous publication/briefing and
live Telegram acceptance still need completion; do not claim full MVP yet.

## NemoClaw preferred path

Official quickstart supports macOS with Docker Desktop/Colima. Owner must choose
provider, sandbox name, messaging and policy before installation. Installation,
image downloads, sandbox creation and credential entry wait for approval.

NemoClaw hosts OpenClaw **inside sandbox**. Host macOS venv cannot run inside Linux
container. Copy/mount repository into sandbox, create Linux Python 3.12+ environment,
install GymClaw, mount private SQLite persistently, then run `runtime plan` **inside
that same Gateway environment**. Do not enable host-path callbacks in container.
Confirm persistence and API access before activation; do not guess bind/network
policy syntax for an uninstalled version.

Required outbound hosts depend on chosen inference: `www.googleapis.com`,
`oauth2.googleapis.com`, `api.telegram.org`, `www.mysports.com`, and NVIDIA route
if selected. Use documented least-privilege policy, not unrestricted network.
OAuth localhost callback requires operator-host flow; token state may need secure
transfer/mount. Do not print/copy secrets through chat. Direct OpenClaw fallback
keeps same domain/skill/outbox if container integration becomes blocker.

## Delivery recovery

`runtime fire --job-id ID --telegram-id OWNER_ID` without `--allow-messages` is
local-only: advances due state and persists delivery. Ordinary `notifications due`
is legacy local-only debug dispatch, **not** activated delivery path.

```text
runtime deliveries
runtime deliver --delivery-id ID --allow-messages
```

PENDING retries recover domain-commit/send gaps. SENT has confirmed receipt.
SENDING (process crash) and UNKNOWN (lost/error response) **never auto-retry**:
Telegram send has no exactly-once key. Inspect actual DM, stop/check original
sender process, then explicitly resolve:

```text
runtime resolve --delivery-id ID --outcome sent|not-sent --confirm-sender-stopped
```

`not-sent` makes pending again; stale plan/advanced workout suppresses delivery.
DB commits output before sending; no network request under SQLite write lock.
Errors withhold child output to avoid credential leakage.

## Acceptance when owner returns

- Verified private bot/owner can invoke profile/current through skill.
- Start appropriate workout, log explicit warm-up and working set.
- Rest automation persists and sends exactly next-step prompt with receipt.
- Early next set cancels old timer; calendar move replaces reminders.
- Preview first real week and separately approve calendar publication.
- Verify Gateway restart recovery, disabled-job visibility and blocked send recovery.

## Official references

- https://docs.openclaw.ai/start/getting-started
- https://docs.openclaw.ai/cli/cron
- https://docs.openclaw.ai/automation/cron-jobs/payloads
- https://docs.openclaw.ai/channels/telegram/setup
- https://docs.openclaw.ai/channels/telegram/access-control
- https://docs.openclaw.ai/tools/exec
- https://docs.openclaw.ai/tools/skills
- https://docs.openclaw.ai/gateway/heartbeat
- https://docs.openclaw.ai/concepts/agent-workspace
- https://docs.nvidia.com/nemoclaw/latest/user-guide/openclaw/get-started/quickstart

Synthetic tests model documented/source-inspected JSON contracts. They do not
prove installed-version behavior or real Telegram delivery.
