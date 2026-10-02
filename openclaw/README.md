# OpenClaw activation handoff

Local NemoClaw sandbox installed; owner confirmed Telegram/OpenRouter chat works.
GymClaw domain/skill connected. Private proactive delivery approved; watcher active
and scheduled setup message verified with persisted receipt. Real workout/rest
acceptance still pending onboarding. Dedicated-calendar publication approved and enabled. Original host
DB/credential files remain untouched.

## Readiness

Initial read-only inspection: macOS arm64, Docker server ready, Node 25.2.1,
OpenClaw/NemoClaw absent. Current OpenClaw docs require Node **24.16+ or 26.1+**
(recommend 26); Node 25 is outside documented support. Install/choose supported
Node only after owner approval. Do not change another project's runtime.

Verified local setup: isolated Node 26.10.0, Colima 0.10.3, NemoClaw v0.0.124,
managed OpenClaw 2026.7.1, Python 3.13.5. Adapter uses `cron` command, supported by
bundled release (including command argv, declaration keys and env). Newer
`automations` command name is absent; no runtime upgrade required. Read-only cron
listing/profile routing verified. Scheduled watcher execution and one-shot Telegram
delivery verified; setup tests are explicit messages, not fabricated workout logs.

Code/Linux venv installed under `/sandbox/.openclaw/workspace/gymclaw`, inside
manifest's backed-up workspace. All 145 tests pass on macOS and Linux there.
Recreate venv when Python or CPU architecture changes; do not move macOS venv to VPS. Repo wrapper selects managed config
path by sandbox location, without reading credentials; declared callbacks retain
that path through `--command-env`. Secrets themselves never enter job argv/env.
Owner-approved opaque host DB copy is now under that repo's `data/` (directory
0700, DB 0600); app migrations succeeded, original host file unchanged. Owner key
files were never inspected or copied. Workspace selected via NemoClaw config API,
Gateway restart healthy, `gymclaw` skill eligible/model-visible; owner DM tool access
confirmed. Scoped dedicated-calendar GET/OAuth-refresh policy applied and live app
read-only sync verified. Owner subsequently granted ongoing writes to the dedicated
GymClaw calendar: persisted runtime authority enabled, scoped event POST/PATCH/DELETE
network preset applied, Google access role verified as owner. Other calendars remain
outside this write preset. Crowd polling is also active.

Installed CLI may prefix JSON with startup logs and wrap job creation as
`{created, updated, job}`; adapter handles both while rejecting trailing garbage.
Cron child inherits `OPENCLAW_GATEWAY_URL`, which refuses implicit config auth.
For this exact managed config path only, adapter drops inherited override and uses
sandbox's config route. No tokens inserted into argv or timer env. Device admin
scope was explicitly approved locally by owner. Ambiguous first test was confirmed
by owner and marked sent, never retried; distinct second test recorded API receipt.
No diagnostic child output remains enabled.

Sandbox DB is now authoritative. Original host DB is an untouched pre-deployment
copy, not synchronized. Run live tools through `nemoclaw gymclaw exec`, not host
CLI against original DB. For VPS, back up active sandbox state, securely provision
credentials and rebuild platform-specific env; stop local poller/timers before
starting VPS instance. No automatic login-start service configured yet.

Architecture: OpenClaw scheduler → exact Python CLI argv → private SQLite domain
transaction/outbox → OpenClaw Telegram message CLI → persisted delivery receipt.
No separate Python agent runtime or sleep/poll daemon. JSON callbacks use
`--no-deliver` so scheduler does not also announce raw JSON or duplicate messages.

## Approval boundaries

1. Install/runtime configuration approval: dedicated **gymclaw** profile only.
2. Telegram activation approval: private verified owner ID, bot token configured
   locally; proactive messages enabled only with `--allow-messages`.
3. Calendar-write approval: separately inspect plan, then `calendar publish
   --allow-writes` for one publication. For continuing autonomy, explicitly grant
   `runtime sync --allow-calendar-writes` with normal runtime/message flags.
   Watcher/Sunday publication defaults off; revoke using `runtime revoke-calendar-writes`.

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
   openclaw --profile gymclaw config set channels.telegram.allowFrom '["OWNER_ID"]'
   openclaw --profile gymclaw config set channels.telegram.dmPolicy allowlist
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
   jobs. Add confirmed `--template-id ID` to enable rolling maintenance and Sunday
   19:00 local weekly audit/planning/briefing. Add `--with-crowd-poll` for approved
   public readings every 15m during configured workout hours. Template/authority
   persist in SQLite; later timer sync retains them. No model calls for these jobs.
   After each logged set, agent invokes same sync immediately. Stable
   declaration keys + DB-path namespace recover lost creation responses. Sync
   cancels old timers without touching foreign jobs. Disabled/drifted jobs need
   operator review, not silent re-enablement. Future notifications fire at true
   due time; overdue rest jobs catch up, prep/leave/start expire after 15 min.
9. Set heartbeat to 30m with private explicit owner target. Current OpenClaw no
   longer reads HEARTBEAT.md: copy checklist to system-owned monitor scratch:

   ```text
   openclaw --profile gymclaw cron list --all --json
   openclaw --profile gymclaw cron scratch HEARTBEAT_JOB_ID --file ABSOLUTE_REPO_PATH/openclaw/HEARTBEAT.md
   ```

   This heartbeat contract targets newer docs. Verify `cron scratch --help` against
   installed release before using it; bundled release heartbeat is not yet activated.
   Choose correct heartbeat job ID from listing, not a fabricated ID. With
   `lightContext: true`, full workspace/skill is absent; leave false unless
   scratch includes all required tool instructions. Heartbeat is not a rest timer.

Update USER.md activation line only after approvals, so agent can distinguish
permission from setup instructions. Weekly workflow is fixture-tested; live Telegram,
real onboarding and publication acceptance still need verification. Do not claim full
MVP yet. Activated callbacks use actual time; artificial `--now` is local-only.

`runtime pause` disables local callback/message authority and revokes calendar writes.
Callbacks cannot reactivate it. `runtime resume --allow-runtime-changes --allow-messages`
requires explicit approval and does not restore calendar writes. These change only
SQLite authority; no global config edits or remote job deletion. Stop dedicated Gateway
when desired. In-flight external calls may already complete; pause cannot unsend them.

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

## Inference stream failures

Pinned local NemoClaw adapter's 30s total response cap caused `LLM request failed`
after successful tools. Deadline-only 300s workaround applied; auth/network guards
unchanged. See [`docs/inference-runtime.md`](../docs/inference-runtime.md) for
version-guarded patch, local streaming regression and scoped adapter restart.

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
DB commits output before sending; no message request under SQLite write lock.
Calendar consequence acknowledgements and weekly briefings use same outbox. New calendar
ack supersedes older still-pending ack; SENT/UNKNOWN history is preserved. Local-only
weekly previews do not enqueue Telegram sends. Quiet watcher ticks do not wake model.
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
