# Onboarding

Setup is a short interview in Telegram, one question at a time. Nothing is assumed: defaults never
count as answers, and GymClaw works for any owner, schedule and gym.

1. **Interview:** goal, training experience, days per week and which days, session length, time
   window, gym address with prep and travel time, equipment (full gym / basic / home), injuries or exercises to avoid.
   Answers that shape scheduling also write the matching profile fields.
2. **Plan:** the owner sends a photo of their plan, or GymClaw builds one from the interview. Every
   exercise is mapped to an illustrated catalog entry (`catalog search`). Unknown weights start at 0
   and are learned from the first session. Several templates form a split that rotates over
   sessions in order (e.g. Push → Pull → Legs).
3. **Review:** one short summary; the owner confirms.
4. **Ready:** later profile or plan edits don't restart setup.

```text
onboarding status
onboarding answer --answers '{"experience":"2 years"}' --request-id ID
onboarding answer --answers '{"schedule":"3x Mon/Wed/Fri"}' \
  --profile '{"weekly_target_sessions":3,"weekdays_allowed":[0,2,4]}' --request-id ID
onboarding confirm-plan --template-id push --template-id pull --template-id legs --request-id ID
onboarding finish --fingerprint REVIEW_FINGERPRINT --request-id ID
```

Profile-backed questions and their fields:

| Question | Profile fields |
| --- | --- |
| schedule | `weekly_target_sessions`, `weekdays_allowed` (Monday=0) |
| session_length | `preferred_workout_minutes` |
| time_window | `earliest_workout_start`, `latest_workout_finish` (local "HH:MM") |
| travel | `gym_address`, `prep_minutes`, `commute_to_gym_minutes` |

Mutations are request-idempotent and survive chat resets. Finishing setup grants no runtime or
calendar authority. Template import fails with `ILLUSTRATION_REQUIRED` until every exercise has a
`guide_id`. Migration `7b3e9d2c4f10` keeps an existing goal and template, and asks for the rest.
