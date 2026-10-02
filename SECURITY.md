# Security

GymClaw is a single-owner prototype, not an audited multi-user service.

- Keep OpenClaw Gateway private. Require a Telegram user-ID allowlist; disable groups.
- Keep tokens, OAuth files, gym identifiers and runtime state out of Git and chat.
- SQLite stores OAuth credentials and workout data. Restrict file access; protect and encrypt backups.
- Calendar publication requires explicit approval, separately from reminders.
- A failed model reply may follow successful tool actions. Check saved state before retrying; never blindly replay writes.
- Public source does not grant access to a deployed bot. Configure your own credentials and owner permissions.

Report suspected vulnerabilities privately to repository owner through GitHub private
vulnerability reporting, if enabled. Otherwise request a private contact channel
without posting exploit details. Never attach tokens, databases, chat transcripts
or private configuration to public issues. If a credential leaks, revoke/rotate it;
deleting a Git file does not remove it from history.
