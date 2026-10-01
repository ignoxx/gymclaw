# GymClaw — Implementation Specification

**Status:** Locked MVP spec  
**Target:** Berlin Claw Agent Challenge, submission deadline 2026-10-02  
**Primary interface:** Telegram  
**Agent runtime:** NVIDIA NemoClaw + OpenClaw  
**Timezone:** Europe/Berlin

---

## 1. Product definition

GymClaw is a long-running autonomous training agent that learns how its user actually trains and continuously manages the training lifecycle.

It does **not** wait for the user to manually schedule each workout.

The user provides constraints and preferences during onboarding. GymClaw then:

1. monitors the user's calendar;
2. predicts when the gym will feel least busy;
3. schedules the next valid workout sessions itself;
4. tells the user when to get ready and when to leave;
5. runs each workout set-by-set through Telegram;
6. starts and completes rest timers automatically;
7. adapts the session when equipment is occupied or time changes;
8. records actual performance;
9. learns the user's exercise preferences and crowd tolerance;
10. audits completed workouts;
11. continuously replans future sessions when the calendar, user availability, crowd conditions, or training outcomes change.

Core positioning:

> **GymClaw doesn't give me a workout plan. It learns how I actually train and continuously runs my training life for me.**

The plan is intentionally never final.

---

## 2. Why this qualifies as a long-running agent

GymClaw must demonstrate all of the following:

- persistent state across days and sessions;
- scheduled autonomous work while the user is absent;
- tool use against external systems;
- proactive messages without the user starting a conversation;
- monitoring of changing external state;
- adaptation when previous plans become invalid;
- learning from historical observations;
- unresolved tasks that remain active over time;
- an observe → decide → act → verify → learn loop.

Primary loop:

```text
CALENDAR ──────────────┐
GYM TRAFFIC ───────────┤
WORKOUT HISTORY ───────┤
USER FEEDBACK ─────────┤
                       ▼
                 ┌───────────┐
                 │  OBSERVE  │
                 └─────┬─────┘
                       ▼
                 ┌───────────┐
                 │   LEARN   │
                 └─────┬─────┘
                       ▼
                 ┌───────────┐
                 │   PLAN    │
                 └─────┬─────┘
                       ▼
                 ┌───────────┐
                 │  EXECUTE  │
                 └─────┬─────┘
                       ▼
                 ┌───────────┐
                 │   ADAPT   │
                 └─────┬─────┘
                       │
                       └──────────────→ back to OBSERVE
```

---

## 3. Runtime decision

### Use: NVIDIA NemoClaw + OpenClaw

OpenClaw should be the actual agent runtime, preferably installed/managed through NVIDIA NemoClaw.

Reasons:

- OpenClaw is NemoClaw's default runtime.
- OpenClaw already supports persistent agent workspaces.
- OpenClaw has built-in Telegram support.
- OpenClaw has persistent automations/scheduled jobs.
- OpenClaw supports recurring heartbeats.
- OpenClaw can connect to custom MCP tools.
- NemoClaw provides the NVIDIA-native deployment story for the challenge.
- NVIDIA currently describes OpenClaw as the default tested runtime, while Hermes is supported but does not currently claim full production parity with OpenClaw.

### Do not use Hermes for this MVP

Hermes is a valid alternative, but it gives GymClaw no obvious advantage for this project and introduces unnecessary risk for a one-day implementation.

### Fallback

If NemoClaw installation or sandbox networking becomes a blocker:

1. run OpenClaw directly;
2. keep the exact same GymClaw domain layer;
3. use an NVIDIA Build/Nemotron endpoint as the inference provider if practical;
4. document that the project is OpenClaw-compatible and NemoClaw-ready.

Do not spend the entire build window debugging infrastructure.

Official references:

- NemoClaw OpenClaw quickstart:  
  https://docs.nvidia.com/nemoclaw/latest/user-guide/openclaw/get-started/quickstart
- OpenClaw Telegram:  
  https://docs.openclaw.ai/channels/telegram
- OpenClaw automations:  
  https://docs.openclaw.ai/cron
- OpenClaw heartbeat:  
  https://docs.openclaw.ai/gateway/heartbeat
- OpenClaw MCP:  
  https://docs.openclaw.ai/tools/mcp
- Google Calendar incremental sync:  
  https://developers.google.com/workspace/calendar/api/guides/sync
- Google Calendar push notifications:  
  https://developers.google.com/workspace/calendar/api/guides/push
- Google Calendar extended properties:  
  https://developers.google.com/workspace/calendar/api/guides/extended-properties

---

## 4. Architecture

### High-level architecture

```text
                         ┌──────────────────────┐
                         │      TELEGRAM        │
                         │  primary user UI     │
                         └──────────┬───────────┘
                                    │
                                    ▼
                         ┌──────────────────────┐
                         │      OPENCLAW        │
                         │                      │
                         │ conversation         │
                         │ reasoning            │
                         │ automations          │
                         │ heartbeat            │
                         │ persistent memory    │
                         └──────────┬───────────┘
                                    │ tools
                                    ▼
                  ┌─────────────────────────────────┐
                  │       GYMCLAW DOMAIN LAYER      │
                  │                                 │
                  │ scheduler                       │
                  │ workout state machine           │
                  │ progression                     │
                  │ crowd predictor                 │
                  │ calendar reconciler             │
                  │ notification planner            │
                  │ workout audit                   │
                  └─────┬────────┬────────┬─────────┘
                        │        │        │
             ┌──────────┘        │        └───────────────┐
             ▼                   ▼                        ▼
      ┌────────────┐      ┌────────────┐           ┌────────────┐
      │  SQLite DB │      │  Calendar  │           │ Crowd data │
      │ source of  │      │ Google API │           │ providers  │
      │ truth      │      │            │           │            │
      └────────────┘      └────────────┘           └────────────┘
                                                       │
                                      ┌────────────────┼──────────────┐
                                      ▼                ▼              ▼
                                  Gym API        Google signal    User feedback
```

### Important design principle

**The LLM is not the database and is not the scheduler.**

Use deterministic code for:

- time arithmetic;
- calendar conflict detection;
- recovery-day validation;
- event ownership;
- workout state transitions;
- set/rep logging;
- progression calculations;
- rest timer timestamps;
- notification timestamps;
- crowd observation storage;
- candidate-window scoring.

Use the agent for:

- interpreting free-form user messages;
- resolving ambiguous user intent;
- explaining choices;
- deciding among valid alternatives;
- contextual substitutions;
- summarizing learned patterns;
- communicating naturally;
- handling exceptional cases.

---

## 5. Recommended implementation stack

### Domain application

Use Python 3.12.

Suggested packages:

```text
fastapi
uvicorn
pydantic
sqlalchemy
alembic
google-api-python-client
google-auth
google-auth-oauthlib
httpx
python-dateutil
scikit-learn
pandas
holidays
typer
rich
```

Do not make FastAPI a hard requirement for the first working version. The domain layer should be callable from a CLI.

### Database

SQLite.

File:

```text
data/gymclaw.db
```

SQLite is sufficient for a single-user competition MVP and is easy to inspect during the demo.

### Tool adapter

For the MVP, expose deterministic GymClaw operations through a CLI:

```bash
python -m gymclaw.cli ...
```

OpenClaw can invoke this through its execution tool.

Example:

```bash
python -m gymclaw.cli schedule plan-week --json
python -m gymclaw.cli calendar sync --json
python -m gymclaw.cli workout log-set --weight 80 --reps 9 --json
python -m gymclaw.cli workout machine-busy --json
python -m gymclaw.cli crowd record-feedback --rating busy --json
```

Every command must return machine-readable JSON.

This is intentionally simpler than spending the build window writing an OpenClaw plugin.

### Optional upgrade

If the CLI is stable early enough, expose the same service functions through a local MCP server.

The domain layer must not depend on whether the caller is CLI, MCP, test code, or an HTTP endpoint.

---

## 6. Repository layout

```text
gymclaw/
├── SPEC.md
├── README.md
├── .env.example
├── pyproject.toml
├── alembic.ini
├── data/
│   └── .gitkeep
├── config/
│   ├── exercises.seed.json
│   └── demo_calendar.json
├── openclaw/
│   ├── AGENTS.md
│   ├── HEARTBEAT.md
│   ├── USER.md
│   ├── TOOLS.md
│   └── skills/
│       └── gymclaw/
│           └── SKILL.md
├── gymclaw/
│   ├── __init__.py
│   ├── cli.py
│   ├── config.py
│   ├── db.py
│   ├── models/
│   │   ├── user.py
│   │   ├── calendar.py
│   │   ├── workout.py
│   │   ├── crowd.py
│   │   └── events.py
│   ├── services/
│   │   ├── planning.py
│   │   ├── calendar.py
│   │   ├── workout.py
│   │   ├── progression.py
│   │   ├── crowd.py
│   │   ├── notifications.py
│   │   ├── audit.py
│   │   └── learning.py
│   ├── providers/
│   │   ├── google_calendar.py
│   │   ├── gym_api.py
│   │   ├── google_busyness.py
│   │   └── holidays.py
│   └── tests/
│       ├── test_planning.py
│       ├── test_calendar_reconcile.py
│       ├── test_workout_state.py
│       ├── test_progression.py
│       └── test_crowd.py
└── scripts/
    ├── seed_demo.py
    └── reset_demo.py
```

---

## 7. Data model

Structured state belongs in SQLite.

### 7.1 UserProfile

```text
id
timezone
weekly_min_sessions
weekly_target_sessions
weekly_max_sessions
weekdays_allowed
minimum_full_rest_days_between_sessions
earliest_workout_start
latest_workout_finish
preferred_workout_minutes
minimum_workout_minutes
prep_minutes
commute_to_gym_minutes
commute_home_minutes
warmup_policy
default_compound_rest_seconds
default_accessory_rest_seconds
crowd_preference_weight
calendar_preference_weight
recovery_weight
created_at
updated_at
```

Initial MVP defaults matching the intended user:

```text
timezone: Europe/Berlin
weekly_min_sessions: 2
weekly_target_sessions: 3
weekly_max_sessions: 3
weekdays_allowed: Mon,Tue,Wed,Thu,Fri
minimum_full_rest_days_between_sessions: 1
preferred_workout_minutes: configurable
prep_minutes: configurable
commute_to_gym_minutes: configurable
commute_home_minutes: configurable
warmup_policy: first_primary_exercise_one_warmup_set
```

### 7.2 LearnedPreference

```text
id
key
value_json
confidence
evidence_count
first_observed_at
last_observed_at
source
```

Examples:

```text
prefers_pec_deck_over_db_fly
friday_skip_rate
typical_actual_rest_seconds_bench
crowd_discomfort_threshold
typical_session_duration
best_adherence_weekday
```

### 7.3 CalendarEventSnapshot

```text
calendar_event_id
ical_uid
etag
title
start_at
end_at
status
updated_at_remote
gymclaw_managed
gymclaw_session_id
gymclaw_plan_revision
user_locked
last_seen_at
raw_json
```

### 7.4 PlannedSession

```text
id
week_id
workout_template_id
status
planned_start_at
planned_end_at
prep_start_at
leave_home_at
expected_finish_at
calendar_event_id
crowd_prediction
crowd_confidence
user_locked
source_revision
created_at
updated_at
```

Status values:

```text
TENTATIVE
COMMITTED
STARTED
COMPLETED
CANCELLED
SKIPPED
MISSED
```

### 7.5 WorkoutSession

```text
id
planned_session_id
started_at
arrived_at
completed_at
status
initial_eta
final_eta
actual_duration_seconds
crowd_feedback
waited_for_equipment_count
notes
```

### 7.6 WorkoutExercise

```text
id
workout_session_id
exercise_id
position
status
planned_working_sets
rep_min
rep_max
target_weight
rest_seconds
substituted_from_exercise_id
deferred_reason
```

Status:

```text
PENDING
ACTIVE
DEFERRED
COMPLETED
SKIPPED
SUBSTITUTED
```

### 7.7 SetLog

```text
id
workout_exercise_id
set_number
set_type
weight
reps
rir_optional
logged_at
rest_started_at
rest_due_at
```

`set_type`:

```text
WARMUP
WORKING
```

### 7.8 CrowdObservation

```text
id
observed_at
source
raw_value
normalized_value
freshness_seconds
metadata_json
```

Sources:

```text
GYM_API
GOOGLE
USER
```

### 7.9 CrowdFeedback

```text
id
workout_session_id
observed_at
rating
normalized_score
waited_for_equipment_count
notes
```

Rating:

```text
EMPTY
FINE
BUSY
PACKED
```

### 7.10 AgentEvent

Internal durable event log:

```text
id
type
created_at
payload_json
handled_at
correlation_id
```

Examples:

```text
calendar.gym_event_changed
calendar.gym_event_deleted
calendar.blocker_added
calendar.blocker_removed
workout.set_logged
workout.machine_busy
workout.completed
user.sick
user.travel
crowd.feedback_received
planning.replan_required
```

This table makes the agent's long-running behavior inspectable in the demo.

---

## 8. OpenClaw memory strategy

### SQLite = source of truth

Never depend on free-form memory for:

- exact weights;
- exact reps;
- exact session timestamps;
- recovery constraints;
- calendar IDs;
- timer IDs;
- crowd observations;
- plan ownership;
- progression state.

### OpenClaw USER.md

Store stable declared preferences:

```text
- target gym frequency
- weekend preference
- recovery rule
- prep duration
- commute duration
- workout duration preference
- interface preferences
```

### OpenClaw MEMORY.md

Store high-value qualitative learned facts, for example:

```text
- User usually considers the gym "busy" around X observed people.
- User prefers pec deck to dumbbell flyes.
- Friday evening workouts have poor adherence.
- User tends to need longer rest after heavy bench sets.
```

Only promote a learned fact when evidence is sufficient.

Do not append every set to MEMORY.md.

---

## 9. Calendar architecture

### MVP calendar backend

Use **Google Calendar API** as the writable/syncable backend.

The user can still view and edit that Google Calendar from Apple's Calendar app by enabling the Google calendar on the iPhone.

This avoids building a native EventKit bridge for the hackathon.

### Calendar event ownership

Every GymClaw-created event must contain private extended properties:

```json
{
  "extendedProperties": {
    "private": {
      "gymclawManaged": "true",
      "gymclawSessionId": "<uuid>",
      "gymclawPlanRevision": "7"
    }
  }
}
```

Suggested title:

```text
🏋️ Gym — Upper A
```

Description should include:

```text
GymClaw managed session
Expected crowd: 42% · Fine
Get ready: 19:15
Leave: 19:30
Workout: 20:00–21:10
```

### What the calendar event represents

The calendar event itself represents the **gym workout window**, not preparation or travel.

However, a candidate workout is valid only if the full block fits:

```text
prep
+ commute to gym
+ workout
+ commute home
```

This prevents GymClaw from scheduling a workout that causes travel/prep to overlap another appointment.

### Calendar sync — MVP

Implement incremental Google Calendar sync.

Run it every **60 seconds**.

Flow:

```text
initial sync
    ↓
persist nextSyncToken
    ↓
60 seconds
    ↓
events.list(syncToken=...)
    ↓
receive only changed/deleted events
    ↓
persist new nextSyncToken
    ↓
reconcile changes
```

If the API returns HTTP 410 because the token is invalid, do a fresh full sync and obtain a new token.

### Calendar sync — post-MVP

Replace/augment polling with Google Calendar push notifications.

Do not block submission on webhook deployment.

---

## 10. CRITICAL: reaction to user-edited GymClaw events

A direct user edit to a GymClaw-managed event is **intent**, not an error.

### Rule: calendar edits are a first-class user interface

The user may drag, resize, or delete a GymClaw event inside Apple Calendar.

GymClaw must notice and react.

### Move event

Example:

```text
GymClaw scheduled:
Wed 20:00–21:10

User drags event:
Thu 19:00–20:10
```

Required behavior:

1. calendar sync detects changed start/end;
2. recognize `gymclawManaged=true`;
3. compare with stored snapshot;
4. mark the session `user_locked=true`;
5. treat Thursday 19:00 as an explicit override;
6. recalculate:
   - get-ready time;
   - leave time;
   - arrival;
   - ETA;
7. cancel stale notification jobs;
8. create replacement notification jobs;
9. check the rest of the week's recovery constraints;
10. replan other GymClaw sessions around the manually moved session;
11. do **not** silently move the user's manually edited session back;
12. send a concise Telegram acknowledgement only when the change has downstream consequences.

Example:

```text
I saw you moved Thursday's gym session to 19:00.

I've updated:
• get ready → 18:25
• leave → 18:40

That makes Friday too close, so I moved the remaining session to Monday.
```

### Resize event

If the user shortens:

```text
20:00–21:10
→
20:00–20:45
```

Required behavior:

1. treat the duration change as explicit intent;
2. update available workout duration;
3. automatically compress the workout;
4. preserve highest-priority movements/volume first;
5. notify user of the revised plan.

Example:

```text
You shortened tonight's gym slot to 45 min.

I kept:
• Bench
• Row
• Lateral raise

I reduced accessory volume and moved curls to the next session.
```

### Delete event

Deletion means:

```text
"I cannot/will not do this session."
```

Required behavior:

1. mark session cancelled/skipped;
2. never recreate that exact event automatically;
3. attempt to satisfy weekly minimum elsewhere;
4. if a valid replacement exists, schedule it;
5. if none exists, accept a lower-volume week and explain once.

### Constraint conflicts

Manual calendar changes override soft preferences.

Examples of soft preferences:

```text
quiet gym
preferred workout hour
avoid Friday
avoid weekends
```

Hard constraints should not be silently violated.

Examples:

```text
explicit travel/unavailable block
minimum recovery rule
calendar overlap
```

If a direct user edit conflicts with a hard constraint, keep the edited event and notify the user that another part of the plan must change.

---

## 11. Calendar blockers: travel, holiday, sickness, life events

GymClaw should infer availability from normal calendar events.

### Travel

Example event:

```text
Lisbon
Oct 12–18
```

If represented as an all-day or multi-day busy event, those days become unavailable.

Do not schedule gym sessions inside that period unless explicitly told otherwise.

### Telegram override

User:

```text
I'm in Portugal next week. Don't schedule gym Oct 12–18.
```

Required behavior:

1. record unavailability;
2. optionally create/update an availability/travel calendar block;
3. replan affected gym sessions;
4. resume normal planning after travel.

### Sickness

User:

```text
I'm sick, pause gym until Friday.
```

Required behavior:

1. mark unavailable through Friday;
2. cancel affected sessions;
3. do not give medical guidance;
4. resume planning after the pause;
5. do not automatically force missed volume into a compressed schedule.

---

## 12. Autonomous weekly scheduling

### Weekly target

Initial expected setup:

```text
2–3 workouts per week
target = 3
minimum = 2
Monday–Friday
at least one full gym-free day between sessions
```

### Planning horizon

Continuously evaluate the next 7–10 days.

### Sunday briefing

Every Sunday at approximately 19:00 Europe/Berlin:

1. audit previous week;
2. refresh calendar;
3. refresh crowd model;
4. generate a provisional next-week plan;
5. create/update GymClaw calendar events;
6. send a concise Telegram briefing.

Example:

```text
GYMCLAW · NEXT WEEK

Mon 20:15 · Upper A
Expected crowd: 43% · Fine

Wed 19:45 · Lower A
Expected crowd: 37% · Quiet

Fri ~19:30 · Upper B
Tentative — I'll keep watching this slot.

Last week: 3/3 sessions completed.
```

### Rolling planning

Sunday's plan is not final.

GymClaw must re-evaluate when:

```text
calendar changes
travel appears
user skips session
user reports sickness/tiredness
a session is manually moved/deleted
training duration changes
weekly frequency becomes impossible
```

### Tentative vs committed

Use:

```text
TENTATIVE
COMMITTED
```

Suggested policy:

- distant sessions may remain tentative;
- upcoming session becomes committed when close enough;
- manually edited sessions become user-locked;
- all sessions can still be replanned when necessary except user-locked sessions.

No need for an elaborate commitment algorithm in the first MVP.

---

## 13. Candidate workout scheduling algorithm

### Inputs

For each candidate start time:

```text
calendar free/busy
prep minutes
commute out
workout duration
commute home
weekday allowed
recovery spacing
predicted crowd
crowd confidence
historical adherence
user preferences
```

### Hard validation

Reject candidates that:

```text
overlap busy calendar time after expanding for prep/travel
violate explicit unavailable periods
violate minimum recovery rule
start/end outside allowed bounds
exceed weekly maximum
```

### Candidate scoring

Simple weighted score:

```text
score =
    crowd_score
  + calendar_convenience
  + adherence_score
  + preferred_time_score
  - uncertainty_penalty
```

The exact weights belong in config.

Do not use the LLM to calculate every candidate.

Generate deterministic candidates and scores, then let the agent choose/explain among the top valid candidates if needed.

---

## 14. Gym busyness intelligence

GymClaw has three signal sources.

### Source 1 — gym API

The gym API exposes the number of people currently in the gym, bucketed/cached within the current hour.

Requirements:

- poll the endpoint periodically;
- save every observation with request timestamp;
- do not assume every request is fresh;
- detect when the returned count changes;
- estimate likely cache/update behavior over time;
- track source freshness.

Example:

```text
18:07 → 84
18:17 → 84
18:31 → 91

possible source refresh detected between 18:17 and 18:31
```

### Source 2 — Google Maps busyness signal

Provider interface:

```python
class GoogleBusynessProvider:
    def get_current_busyness(...) -> CrowdObservation
    def get_popular_times(...) -> list[CrowdObservation]
```

The exact acquisition mechanism may be scraping or a third-party provider because public Google Maps/Places interfaces do not necessarily expose every Popular Times field needed.

Do not let this integration block the MVP.

If necessary, build it behind a provider adapter and use seeded/demo observations.

### Source 3 — user ground truth

When the user arrives:

```text
How busy does it actually feel?

[Empty] [Fine] [Busy] [Packed]
```

Optionally:

```text
Did you have to wait for equipment?

[No] [Once] [Multiple times]
```

This feedback is the personalized target.

GymClaw is optimizing:

> **How crowded this gym feels to this user**

not merely:

> number of people in the building.

---

## 15. Crowd predictor

### Features

MVP features:

```text
day_of_week
hour_of_day
is_public_holiday
gym_api_count
gym_api_freshness
google_busyness
historical_mean_for_slot
user_feedback_history
```

Post-MVP:

```text
weather
school/university holidays
special local events
season
month
day-before-holiday
day-after-holiday
```

### Cold start

Before enough user labels exist:

```text
prediction =
weighted gym API
+ weighted Google signal
+ historical slot baseline
```

### Personalized phase

Once user feedback accumulates, learn a mapping from available signals to the user's normalized crowd score.

Do not build a neural network.

Recommended progression:

```text
< 10 labeled visits:
heuristic weighted model

10–30:
simple regression / calibrated model

30+:
optional tree-based regressor
```

For the competition demo, seeded historical data is acceptable if clearly represented as imported historical observations rather than falsely claimed live history.

### Source reliability

Maintain per-source reliability:

```text
gym_api_reliability
google_reliability
```

Compare predictions against later user ground truth and gradually adjust weights.

This gives GymClaw a strong "learns which sensors to trust" story.

---

## 16. Workout execution state machine

```text
SCHEDULED
   ↓
PREPARING
   ↓
COMMUTING
   ↓
ARRIVED
   ↓
SESSION_STARTED
   ↓
EXERCISE_ACTIVE
   ↓
SET_ACTIVE
   ↓
RESTING
   ↓
SET_ACTIVE
   ↓
EXERCISE_COMPLETE
   ↓
NEXT_EXERCISE
   ↓
...
   ↓
WORKOUT_COMPLETE
   ↓
POST_ANALYSIS
   ↓
PLAN_UPDATED
```

The state machine must be deterministic.

The LLM must not invent state transitions.

---

## 17. Warm-up policy

MVP interpretation:

> One warm-up set before the first primary/compound exercise of the session, before working weight.

Example:

```text
Bench Press

Warm-up
50 kg × 8

This does not count toward working volume.
```

Then:

```text
Working set 1/3
80 kg
Target 8–10 reps
```

Make warm-up behavior configurable later.

---

## 18. Telegram UX

Telegram is the primary interface.

### Onboarding

Collect:

```text
weekly target
allowed weekdays
minimum rest days
earliest start
latest finish
preferred workout duration
prep duration
commute duration
exercise/workout template
rest preferences
```

### Start of workout

```text
You're at the gym.

How busy does it feel?

[Empty] [Fine] [Busy] [Packed]

Today's session:
Upper A · ~67 min
Estimated finish: 20:47
```

### Exercise message

```text
BENCH PRESS

Warm-up
50 kg × 8

Last working session:
80 × 10
80 × 9
80 × 8

[LOG SET]
[MACHINE BUSY]
[SWAP]
[SKIP]
```

Exercise imagery can be added from:

https://bryllim.github.io/workout-guide/exercises/

Preserve the repository/assets' required attribution/license obligations.

### Free-form set log

User:

```text
80x9
```

Parse as:

```json
{
  "weight": 80,
  "reps": 9
}
```

Also allow:

```text
80 x 9
80kg x9
9 reps at 80
```

If parsing is ambiguous, ask one concise question.

---

## 19. Rest timer

When a working set is logged:

1. save the set;
2. determine rest duration;
3. save `rest_started_at`;
4. save `rest_due_at`;
5. create an exact one-shot OpenClaw automation;
6. reply immediately:

```text
✓ 80 kg × 9 logged.

Rest.
```

When timer expires:

```text
Rest complete.

Bench Press · Set 2/3
80 kg · target 9+
```

The timer must survive conversation inactivity.

If the user logs another set early, cancel the stale rest timer.

---

## 20. "Holy shit" workout ETA

Continuously estimate finish time.

Inputs:

```text
remaining sets
rest durations
historical set duration
exercise transition duration
deferred exercises
substitutions
current elapsed time
```

After meaningful changes:

```text
Estimated finish: 20:47
2 min ahead of plan
```

If a machine is busy:

```text
Pec deck deferred.
New ETA: 20:52
```

If the calendar event was manually shortened before the workout:

```text
45 min available.
Workout automatically compressed.
ETA: 20:44
```

This feature should be visible in the demo.

---

## 21. Busy equipment behavior

User presses:

```text
MACHINE BUSY
```

or says:

```text
pec deck is occupied
```

Algorithm:

1. mark current exercise temporarily unavailable;
2. see whether another planned exercise can be moved forward safely;
3. prefer reordering before substituting;
4. if reordering is possible:
   - defer current exercise;
   - proceed with next compatible movement;
   - keep unresolved exercise in pending queue;
5. retry later;
6. if still unavailable or workout time is running out:
   - choose substitution.

Example:

```text
Pec deck is busy.

I'm moving lateral raises forward.
I'll retry pec deck after two exercises.
```

Later:

```text
We're back to pec deck.

[FREE NOW]
[STILL BUSY]
```

### Substitution context

Substitution should consider:

```text
target muscle / movement role
volume already performed
remaining volume
user likes/dislikes
previous substitutions
equipment likely available
time remaining
```

Example:

```text
Switching Cable Fly → Pec Deck

Why:
• same remaining chest role
• avoids adding another press
• you prefer pec deck to DB flyes
• last time: 50 × 12/11/10
```

---

## 22. Progression engine

Progression must be deterministic.

Do not let the LLM randomly choose weights.

Minimum model:

```text
exercise
working weight
target rep range
working set count
previous performance
progression rule
```

Example double progression:

```text
target: 3 × 8–10

80 × 10
80 × 10
80 × 10

→ threshold achieved
→ next target weight: 82.5 kg
```

If target is repeatedly missed:

```text
hold weight
```

Do not implement complex deload science for the MVP.

---

## 23. Post-workout audit

After workout completion:

```text
planned sets
completed sets
substitutions
skips
actual duration
estimated vs actual finish
crowd feedback
equipment waits
progression events
```

Telegram example:

```text
UPPER A COMPLETE

17/18 sets
64 min
2 substitutions
1 progression

Bench:
82.5 × 8 → 9
↑ progressing

Pec deck:
target missed twice
→ hold weight next session

Gym congestion cost:
~9 min

Next adjustment:
move cable work later; cables were occupied
during the first half of 3 recent sessions.
```

---

## 24. Weekly audit / learned behavior

Sunday process should detect useful trends.

Examples:

```text
Thursday workouts average 16 min shorter.
Friday skip rate is high.
Wednesday 20:00 has the lowest perceived crowd level.
Cable station is frequently unavailable 18:00–19:00.
Actual bench rest averages 2:37.
```

Only store a "learned preference" after enough evidence.

The agent should be able to say:

```text
I changed next week's schedule because Wednesday evenings
have become your most reliable low-crowd slot.
```

This is one of the strongest competition narratives.

---

## 25. Proactive notifications

GymClaw should notify the user without requiring a message first.

For a planned workout at 20:00:

```text
19:15 — Get ready
19:40 — Leave now
20:00 — Gym session begins
```

Actual values derive from:

```text
prep_minutes
commute_to_gym_minutes
workout_start
```

Example:

```text
Gym in 45 minutes.

Start getting ready now.
Expected crowd: Fine.
Today: Upper A · ~67 min.
```

Then:

```text
Leave now.

Estimated travel: 20 min.
Workout starts at 20:00.
```

If the user moves the calendar event, these scheduled notifications must be cancelled and recreated around the new time.

---

## 26. OpenClaw automations

Use automations for exact future actions.

Required jobs:

### Weekly planning

```text
Sunday 19:00 Europe/Berlin
```

Payload:

```text
Run GymClaw weekly audit and rolling plan.
Update managed calendar events.
Send Telegram weekly briefing.
```

### Calendar watcher

MVP:

```text
every 60 seconds
```

Prefer a cheap deterministic script/trigger that only wakes the LLM when meaningful changes are found.

### Crowd polling

Suggested:

```text
every 15 minutes during relevant gym hours
```

Do not burn model calls for polling. Fetch/store data in code.

### Get-ready notification

One-shot job per committed session.

### Leave notification

One-shot job per committed session.

### Rest timer

One-shot job after every set requiring timed rest.

### General heartbeat

Use a slower heartbeat for unresolved agent concerns, not for precise timers.

Suggested:

```text
15m–30m
```

HEARTBEAT.md should tell the agent to:

```text
- inspect pending GymClaw events
- surface only actionable issues
- avoid duplicating scheduled notifications
- replan only when an event marks replan_required
```

---

## 27. External tools / providers

Design provider interfaces so integrations can be replaced.

### CalendarProvider

```python
list_busy_intervals(...)
create_gym_event(...)
update_gym_event(...)
delete_gym_event(...)
get_event(...)
incremental_sync(...)
```

### GymOccupancyProvider

```python
get_current_count(...)
```

### GoogleBusynessProvider

```python
get_current_busyness(...)
get_popular_times(...)
```

### HolidayProvider

```python
is_public_holiday(date, region)
get_holidays(start, end, region)
```

### ExerciseLibraryProvider

```python
get_exercise(...)
search_exercises(...)
get_image(...)
```

---

## 28. Agent skill instructions

Create:

```text
openclaw/skills/gymclaw/SKILL.md
```

Core rules:

```text
1. GymClaw owns routine workout scheduling.
2. Never ask the user to choose a time when a valid best slot can be selected.
3. Treat manual edits to GymClaw calendar events as explicit intent.
4. Replan around manually edited events instead of undoing them.
5. Keep structured state in GymClaw tools, never in conversation guesses.
6. Prefer deterministic tool results over model arithmetic.
7. Never fabricate gym occupancy or workout logs.
8. When equipment is busy, prefer reordering before substitution.
9. Never discard an unresolved deferred exercise silently.
10. Proactively communicate only when the user needs to act or when a meaningful plan changed.
11. Keep Telegram messages short while the user is training.
12. During rest, do not spam the user.
13. Do not give medical advice.
14. When the user reports illness, pause/replan instead of optimizing through it.
```

---

## 29. Agent-facing tool contract

Minimum tool/CLI operations:

```text
profile.get
profile.update

calendar.sync
calendar.get_week
calendar.create_session
calendar.update_session
calendar.delete_session

planning.plan_week
planning.replan
planning.explain_session_choice

crowd.poll
crowd.predict
crowd.record_feedback
crowd.get_source_health

workout.start
workout.current
workout.log_set
workout.machine_busy
workout.machine_free
workout.substitute
workout.skip_exercise
workout.finish

audit.workout
audit.week

notifications.schedule
notifications.cancel

events.pending
events.ack
```

Every result:

```json
{
  "ok": true,
  "data": {},
  "events": [],
  "user_message_hint": null
}
```

Errors:

```json
{
  "ok": false,
  "error": {
    "code": "CALENDAR_AUTH_REQUIRED",
    "message": "..."
  }
}
```

---

## 30. Event-driven replanning

Do not continuously ask the LLM to reconsider everything.

Use events.

Example:

```text
calendar.sync
    ↓
calendar.gym_event_changed
    ↓
planning.replan_required
    ↓
agent wakes
    ↓
planning.replan
    ↓
calendar updates
    ↓
notifications rescheduled
    ↓
Telegram message only if useful
```

Another:

```text
workout.machine_busy
    ↓
workout.reorder_or_substitute
    ↓
ETA recalculated
    ↓
user receives next action
```

This makes long-running behavior reliable and explainable.

---

## 31. MVP scope — MUST SHIP

The competition MVP is complete only when this end-to-end story works:

### A. Persistent profile

User preferences survive restart.

### B. Calendar-aware autonomous planning

GymClaw finds valid training slots itself and creates calendar events.

### C. Manual calendar changes are detected

Dragging/deleting/resizing a managed event causes GymClaw to react and replan.

### D. Proactive preparation/leave notifications

Telegram messages arrive at the correct times.

### E. Gym occupancy history

Gym API observations are persisted.

At least one second source interface exists.

User crowd feedback is persisted.

### F. Set-by-set Telegram workout

User can:

```text
start workout
log set
receive rest completion
mark machine busy
receive reordered/substituted exercise
finish workout
```

### G. Warm-up set

First primary exercise supports one warm-up set.

### H. ETA

Workout finish estimate updates after sets/exceptions.

### I. Post-workout audit

Session produces a concrete summary and progression update.

### J. Weekly planning

Sunday job creates/updates next week's plan and sends briefing.

---

## 32. Explicit non-goals for submission

Do **not** build these before the core loop works:

```text
Apple Watch / HealthKit
native iOS app
native watchOS app
nutrition
sleep coaching
heart-rate based training changes
computer vision
social features
multi-user support
payments
beautiful mobile app
advanced strength science
full machine-learning infrastructure
perfect Google Popular Times extraction
voice mode
```

These are post-MVP.

---

## 33. Apple Watch future extension

Future architecture:

```text
Apple Watch
    ↓
HealthKit
    ↓
iOS companion
    ↓
GymClaw API
    ↓
recovery / exertion context
```

Potential signals:

```text
workout duration
heart rate
heart-rate recovery
sleep
resting heart rate
activity
```

Do not implement for the competition MVP.

---

## 34. Dashboard for the demo

Telegram is the product UI.

A small dashboard is the **judge UI**.

It should make autonomy visible.

Minimum dashboard:

```text
GYMCLAW

NEXT SESSION
Wed 20:00 · Upper A
Expected crowd: 38%

AGENT STATE
● calendar monitoring
● gym traffic learning
● session committed
○ waiting for workout

MEMORY
42 workouts
518 sets
31 learned preferences
17 crowd labels

LAST AUTONOMOUS ACTION
Moved Friday → Thursday
Reason: calendar conflict + lower expected crowd

CURRENT WEEK
2/3 sessions complete
```

Optional second panel:

```text
EVENT LOG

22:03 calendar change detected
22:03 workout moved by user
22:03 leave reminder cancelled
22:03 recovery constraint recalculated
22:04 Friday session moved
22:04 Telegram notification sent
```

This is very valuable for judges because it proves the agent is doing work outside the chat.

Dashboard can read directly from SQLite.

A minimal FastAPI/Jinja or Streamlit page is sufficient.

---

## 35. Demo data / time acceleration

Real weekly behavior cannot be demonstrated in a 60–90 second video.

Support deterministic demo fixtures.

Create:

```bash
python scripts/seed_demo.py
python scripts/reset_demo.py
```

Seed:

```text
several weeks of crowd observations
several completed workouts
exercise preferences
calendar events
one upcoming GymClaw session
```

Do not fake live external calls as if they happened historically.

Clearly treat seed data as historical/demo state.

Support an internal `--now` override in domain services/tests where possible so Codex can simulate:

```text
Sunday weekly planning
calendar change
gym arrival
set logging
rest completion
post-workout learning
```

Production runtime uses real current time.

---

## 36. Suggested 75-second competition demo

### Scene 1 — autonomous plan

Dashboard:

```text
GymClaw planned:
Wed 20:00 · Upper A
Expected gym: quiet
```

Narration:

> "I don't schedule my workouts. GymClaw does."

### Scene 2 — life changes

Open Apple Calendar and drag Wednesday Gym event to Thursday.

Within the demo, trigger calendar sync manually rather than waiting 60 seconds.

Dashboard event log:

```text
calendar edit detected
user override accepted
prep/leave reminders changed
remaining week replanned
```

Telegram:

```text
I saw you moved gym to Thu 19:00.

Get ready: 18:25
Leave: 18:40

I moved the remaining session to Monday
to preserve your recovery day.
```

### Scene 3 — actual gym adaptation

Telegram:

```text
Bench Press
Warm-up 50 × 8
```

Log:

```text
80x9
```

GymClaw:

```text
✓ logged
Rest.
ETA 20:47
```

Trigger rest completion.

```text
Rest complete.
Set 2/3 — 80 kg · target 9+
```

Then:

```text
pec deck is busy
```

GymClaw:

```text
Pec deck deferred.
Do lateral raises now.
I'll retry it later.

New ETA: 20:51
```

### Scene 4 — learning

Finish workout.

```text
Workout complete.

17/18 sets
1 progression
2 substitutions

Crowd:
How did it feel?
```

User presses:

```text
Busy
```

Dashboard:

```text
crowd label stored
source reliability updated
future slot predictions recalculated
```

End:

> **GymClaw learns how I actually train and continuously runs my training life for me.**

---

## 37. Build order for Codex

Codex should implement in this order.

### Phase 1 — deterministic core

Build:

```text
SQLite models
profile
calendar candidate planner
workout state machine
set logging
progression
ETA
tests
```

Use fixtures. No external APIs yet.

Acceptance:

```text
pytest passes
CLI can run an entire fake workout
CLI can generate a valid weekly plan
```

### Phase 2 — calendar

Build:

```text
Google OAuth
read busy intervals
create managed gym event
private extended properties
incremental sync
reconcile move/resize/delete
replanning
```

Acceptance:

```text
create event
move it manually
run calendar sync
DB reflects edit
notifications timestamps update
other sessions replan
```

### Phase 3 — OpenClaw/NemoClaw

Build/configure:

```text
OpenClaw
NemoClaw if practical
Telegram
GymClaw skill
exec tool access
heartbeat
weekly automation
calendar watcher
```

Acceptance:

```text
Telegram can invoke GymClaw CLI
agent sends proactive scheduled message
```

### Phase 4 — workout Telegram flow

Build:

```text
start workout
exercise image
warmup
set parser
rest automation
machine busy
reorder
substitute
ETA
finish
```

### Phase 5 — crowd model

Build:

```text
gym API polling
second provider adapter
user feedback
prediction
simple calibration
source reliability
```

### Phase 6 — demo/dashboard

Build only after the end-to-end loop works.

---

## 38. Acceptance tests

### Scheduling

```text
Given target 3 sessions/week
and Mon–Fri only
and one rest day between workouts
when calendar availability exists
then planner creates up to 3 valid sessions
without violating recovery.
```

### Travel

```text
Given Oct 12–18 unavailable
then no session is scheduled inside that interval.
```

### Calendar move

```text
Given a GymClaw-managed Wed event
when user moves it to Thu
then GymClaw accepts Thu,
marks it user_locked,
reschedules reminders,
and replans conflicting sessions.
```

### Calendar delete

```text
Given a managed event
when user deletes it
then the exact event is not recreated,
and GymClaw searches for another valid weekly slot.
```

### Resize

```text
Given 70-minute workout
when calendar event is shortened to 45 minutes
then workout is compressed to <=45 minutes.
```

### Set logging

```text
Given active bench exercise
when user sends "80x9"
then working set is stored correctly
and rest timer starts.
```

### Rest

```text
when rest_due_at is reached
then Telegram sends next-set instruction.
```

### Busy machine

```text
when current machine becomes busy
then GymClaw first attempts reordering;
if unresolved later, substitution is offered.
```

### Learning

```text
when crowd feedback is stored
then future crowd prediction uses the new label.
```

### Persistence

```text
restart process
then active plan, history, and learned state remain intact.
```

---

## 39. Failure behavior

GymClaw must fail visibly and conservatively.

Examples:

### Calendar unavailable

```text
I couldn't sync your calendar.
I have not changed this week's plan.
I'll retry automatically.
```

### Gym API unavailable

```text
Use previous crowd model
reduce confidence
do not fabricate current people count
```

### Google busyness unavailable

```text
continue with gym API + historical + user data
reduce confidence
```

### Model unavailable

Deterministic state remains valid.

No duplicate events or duplicate set logs should be created on retry.

Use idempotency keys/correlation IDs for event handling.

---

## 40. Privacy / permissions

For MVP:

- single user only;
- restrict Telegram bot to the user's Telegram ID;
- use minimum Google Calendar scopes needed;
- do not log OAuth secrets;
- do not commit credentials;
- `.env` must be gitignored;
- database remains local/private;
- avoid storing unnecessary calendar event bodies;
- use event titles/times only when possible.

---

## 41. Environment variables

Example:

```bash
GYMCLAW_TIMEZONE=Europe/Berlin
GYMCLAW_DB_URL=sqlite:///data/gymclaw.db

GOOGLE_CLIENT_ID=
GOOGLE_CLIENT_SECRET=
GOOGLE_CALENDAR_ID=primary

GYM_API_BASE_URL=
GYM_API_TOKEN=

TELEGRAM_BOT_TOKEN=
TELEGRAM_ALLOWED_IDS=

NVIDIA_API_KEY=
```

Do not hard-code secrets.

---

## 42. README quickstart target

Eventually README should reduce setup to roughly:

```bash
git clone ...
cd gymclaw

uv sync
cp .env.example .env

python -m gymclaw.cli db init
python -m gymclaw.cli demo seed

# authenticate Google Calendar
python -m gymclaw.cli calendar auth

# install/onboard OpenClaw through NemoClaw
# configure Telegram

pytest
```

Exact commands can change based on implementation.

---

## 43. First Codex prompt

Use this after placing `SPEC.md` in the repository:

```text
Read SPEC.md completely before editing anything.

You are implementing the GymClaw hackathon MVP.

Rules:
1. Treat SPEC.md as the product source of truth.
2. Prioritize the end-to-end autonomous loop over breadth.
3. Implement deterministic domain logic before external integrations.
4. Keep all structured state in SQLite.
5. Make every domain operation testable without OpenClaw.
6. Expose domain operations through a JSON CLI first.
7. Add tests as each subsystem is implemented.
8. Do not implement non-goals from SPEC.md.
9. Do not add Apple Watch/HealthKit.
10. Do not spend time on UI polish until the required acceptance tests pass.
11. When a design choice is unspecified, choose the simplest implementation that preserves the architecture.
12. Never fabricate external API behavior. Build provider interfaces and fixtures where credentials/data are unavailable.

Start by:
- inspecting the repo,
- creating the Python project skeleton,
- defining SQLAlchemy models,
- implementing migrations/database initialization,
- implementing UserProfile and planning-domain types,
- implementing the deterministic candidate-slot planner,
- writing its tests.

After each coherent phase, run the relevant tests and fix failures before proceeding.
```

---

## 44. Product decisions locked by this spec

These should not be reopened during the hackathon unless technically impossible:

```text
✓ GymClaw schedules sessions autonomously.
✓ User gives constraints, not routine scheduling instructions.
✓ Calendar edits are treated as a user-control surface.
✓ Manually moved GymClaw sessions are respected.
✓ GymClaw proactively says when to get ready and leave.
✓ Telegram is the primary interface.
✓ Google Calendar is the MVP calendar backend.
✓ Apple Calendar can be the user's client UI for the Google calendar.
✓ Structured fitness state lives in SQLite.
✓ OpenClaw is the agent runtime.
✓ NemoClaw is preferred as the NVIDIA deployment wrapper.
✓ Warm-up = one warm-up set before first primary movement by default.
✓ Sets are logged as weight × reps.
✓ Logging a set starts an autonomous rest timer.
✓ Rest completion proactively triggers the next-set prompt.
✓ Machine-busy behavior first reorders, then substitutes.
✓ Workout ETA updates throughout the session.
✓ Gym busyness combines gym API + Google signal + user ground truth.
✓ The system learns source reliability and personal crowd tolerance over time.
✓ Sunday evening produces a weekly briefing.
✓ Plans remain rolling/adaptive after Sunday.
✓ Apple Watch is post-MVP.
```

---

## 45. Definition of done

GymClaw is ready to submit when a judge can watch this sequence without explanation:

```text
1. Agent has persistent training/calendar/crowd state.
2. Agent autonomously schedules a workout.
3. User changes the agent-created event in Calendar.
4. Agent detects the edit and replans around it.
5. Agent proactively tells user when to prepare/leave.
6. User starts the workout in Telegram.
7. User logs a real set.
8. Agent runs the rest timer.
9. Equipment becomes unavailable.
10. Agent adapts the workout and updates ETA.
11. Workout completes.
12. User supplies crowd ground truth.
13. Agent updates its history/learning.
14. Future planning now has new information.
```

If that loop works reliably, stop adding features and record the demo.
