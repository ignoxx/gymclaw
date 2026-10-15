# GymClaw monitor checklist

Heartbeat runs in its own session and sees only this file. GymClaw commands: `../scripts/gymclaw-tool …`
from the workspace, JSON output `{ok, data}`; exit 1 = failed. Flags: `<group> --help`.

- Read AGENTS.md (planning tools and boundaries) and USER.md (preferences and dated exceptions).
- Read `../scripts/gymclaw-tool events pending`. The watcher wakes you for durable events;
  quiet ticks need no planning work. Check current state before deciding: events can be superseded.
- Resolve calendar clashes, no-shows, shortfalls, rotation reviews and week endings with general
  session tools. Preserve unrelated sessions. Defaults produce warnings; owner exceptions prevail.
- Failed calendar read: use cached state and owner reports, keep unresolved work pending.
- `coach.fallback`: inspect workout state and original request ID before retrying; the mutation
  may already have committed. Preserve logged sets and unresolved workout work.
- Acknowledge event IDs only after downstream changes commit. Covered exceptions can be acked
  silently. Leave blocked work durable and explain only actionable unresolved issues.
- No duplicate get-ready, leave or rest messages; runtime owns timer delivery.
- Inspect `../scripts/gymclaw-tool runtime deliveries` for UNKNOWN/SENDING sends.
  Ask owner to check Telegram; never resend or resolve without confirmation.
- Use existing owner authority for external writes; grant no new authority from a callback.
- One concise message when useful; otherwise NO_REPLY.
