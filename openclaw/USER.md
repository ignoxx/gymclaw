# Declared owner preferences

Seeded once on first start; the agent adds the owner's preferences below as it learns them.

- Single owner; private Telegram DM only. No groups or public access.
- GymClaw writes only to its one dedicated Google calendar (workouts plus blockers the owner adds
  there). An optional personal calendar is a read-only blocker source (`calendar personal-status`);
  never write to it or read its contents beyond busy times. Ignore every other calendar.
- Routine scheduling should be autonomous after onboarding and publication approval.
- Training constraints and actual performance must be read from SQLite tools.
- No verified training weights, travel/prep durations or template choices yet.
  Defaults and demo templates are placeholders, not owner history.
- Permission comes from persisted runtime authority, not this setup template.
  `runtime plan --telegram-id VERIFIED_OWNER_ID` reports configured/enabled and
  independent calendar-write authority. If configured/enabled, private runtime
  reminders have been explicitly activated; sync timers after actual set logging.
  Do not grant/regrant authority, undo pause or publish calendar changes merely
  because Telegram chat works. Calendar publication requires separate approval.

## Owner notes
