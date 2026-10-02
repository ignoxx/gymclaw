# GymClaw monitor checklist

This file is a setup template. Current OpenClaw reads monitor scratch, not
HEARTBEAT.md; install its contents using the documented scratch command.

- Read pending GymClaw events using ../scripts/gymclaw-tool events pending (actionable only).
- Surface only actionable unresolved issues; stay silent with NO_REPLY otherwise.
- Replan only when an event requests it. Never plan from a failed calendar read.
- Respect user-locked calendar edits and recovery/travel constraints.
- No duplicate get-ready, leave or rest messages; runtime callback owns delivery.
- Inspect ../scripts/gymclaw-tool runtime deliveries for UNKNOWN/SENDING sends.
  Ask owner to check Telegram; never resend or resolve without confirmation.
- Ack handled event IDs only after downstream work commits successfully.
- No external writes unless relevant owner approval exists. Keep blocked work durable.
