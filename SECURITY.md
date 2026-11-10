# Security

GymClaw is a single-owner app, not an audited multi-user service.

- The bot answers only the allowlisted Telegram user ID (`GYMCLAW_TELEGRAM_USER_ID`), in private
  chats. Everything else is ignored.
- The bot container reaches the internet only through an egress proxy with a host allowlist, and
  refuses to start if a direct connection works ([deploy/README.md](deploy/README.md)).
- SQLite holds the Google OAuth token and all chat and workout data. Restrict file access; keep
  backups private and encrypted. Keep tokens, OAuth files and gym identifiers out of Git and chat.
- Calendar writes need their own explicit grant, separate from reminders.
- A failed model reply may follow tool actions that succeeded. Check the saved state before
  retrying; never blindly replay writes.

Report suspected vulnerabilities through GitHub private vulnerability reporting if it is enabled;
otherwise ask the repository owner for a private channel, without posting details. Never attach
tokens, databases, chat transcripts or private config to public issues. If a credential leaks,
revoke it: deleting a file doesn't remove it from Git history.
