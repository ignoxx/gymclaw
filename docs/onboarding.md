# Short, resumable setup

Telegram flow: goal → schedule review → workout-plan/weights review → final confirmation.
One question or small batch at a time. Images use native vision; unreadable values
stay unknown. No OCR/install loop, demo weights, invented sets or publication consent.

`onboarding status` returns next step, saved profile draft, template choices and review
fingerprints. Defaults are never automatically confirmed. Existing choices are reused.

JSON CLI operations:
- `set-goal --goal TEXT --request-id ID`
- `confirm-profile --fingerprint HASH --request-id ID`
- `confirm-template --template-id ID --fingerprint HASH --request-id ID`
- `finish --fingerprint HASH --request-id ID`

Read profile/status, show concise review, wait for owner confirmation. Apply requested
profile changes with `profile update`, then refresh fingerprint. Read `template get`
before reviewing exercises, sets, reps and starting/warm-up weights. Matching template
fingerprint comes from status's template choices. Finish uses review fingerprint.
Never expose hashes to owner as questionnaire answers; agent handles them.

Mutations are request-idempotent; retries use same authenticated user-action ID.
Changed profile/template invalidates confirmation; setup survives chat resets/restarts.
Completion grants no calendar/runtime authority and creates no workout logs. Calendar
writes stay off during testing. Use active sandbox tools, not original host DB.

Migration adds one metadata table without replacing saved profiles/templates/workouts.
Opaque SQLite backup created before local activation. Nemo snapshot currently rejects
venv symlinks in backed-up workspace; packaging fix remains separate deployment work.
