# Crowd evidence and model limits

MySports integration follows reviewed handoff/FINDINGS and captured public response:

- Studio/tenant identifiers configured privately; no owner's gym in source.
- GET `https://www.mysports.com/nox/public/v1/studios/{studio_id}/utilization/v2/active-checkin`
- Header `x-tenant: {tenant}`; captured response shape `{"value":7}`.
- No SESSION cookie needed. Code neither accepts nor stores one.
- Response supplies no observation time, cache age, capacity or checkout semantics.
- Undocumented observed route, not guaranteed stable API. Unit tests use fixtures.

## Local activation

Owner approved live reads. Narrow MySports GET policy in
an ignored local policy generated from
`config/gymclaw-crowd-read-policy.example.yaml` applied to dedicated sandbox. Count and
`today` live requests returned 200; no Age/Cache-Control/Last-Modified provided.
No cache duration inferred. App stored first real count with retrieval timestamp,
not fabricated observation time. `today` exposes 24 items with start/end/current/
percentage; relative series is **not yet periodically collected**.

Count timer runs every 15 minutes around the clock (the full daily curve matters, not just
training hours). The watcher sends one Telegram alert when no reading arrives for 45 minutes,
and one when data flows again; the dashboard shows the same health. Scheduled callback tested:
healthy, skipped outside that window; manual live acquisition succeeded. Calendar
writes remain off. Polling depends on local Mac/VM/Gateway staying awake/running.

Existing `today` date-selection probes were ignored by server, so no dated lookback
claim. Historical weekdays are not an archive. Our persisted count samples build
our own dated history from activation forward, subject to unknown backend lag.

Reference investigation: September 30, 2026, private endpoint research.
Original gym-specific captures are not included in public source.
Tests reproduce response contract synthetically; they are not fabricated historical
live requests. Research capture timestamps are not reused as current observations.

## Private gym configuration

Set both `GYMCLAW_MYSPORTS_STUDIO_ID` and `GYMCLAW_MYSPORTS_TENANT`, or create
ignored `data/crowd-config.json` (mode 0600) in active sandbox repo:

```json
{"studio_id":"YOUR_NUMERIC_STUDIO_ID","tenant":"YOUR_TENANT"}
```

Optional `GYMCLAW_CROWD_CONFIG` selects another private JSON path. There is no
hardcoded gym fallback. Callbacks use repo cwd and load this same private config.
Identifiers are not API credentials, but identify a gym; don't publish config,
policy exports or runtime backups. Existing private deployment config preserved.

## Distinct quantities

1. Reported active count: raw integer, local retrieval timestamp. May lag reality.
2. `/today`: dated hourly percentages with `current`, unknown aggregation method.
3. `/historic/week`: undated weekday percentages with no sample counts. All 168
   observed values were 5%; meaning unknown (possible floor/fallback/baseline).
4. User felt crowding: EMPTY/FINE/BUSY/PACKED labels define personal score.

Only (1) and (4) enter the predictor. Google Popular Times is intentionally not used: there is no
official API, and our own 15-minute check-in polling is more direct for this gym.

## Calendar "Expected crowd"

Each planned session stores a forecast: typical reported check-ins for that weekday and hour (or
the same hour on other days while history is thin) plus the owner's own Empty/Fine/Busy/Packed
feel once labels exist, e.g. `Busy · ~13 people checked in (Fridays 19:00, 2 wk)`. The watcher
fills missing forecasts right away, re-checks sessions in the next 24 h every 30 min, and edits the
event only when the feel changes or the count moves by ≥3 people and ≥25%. Nothing changes within
1 h of start, so imminent reminders stay put. No invented gym
capacity, count↔percentage mapping, historical count archive or future attendance.
MySports display limits 14/57 are categories, not physical occupancy conversion.

## Model

- Raw counts stay unnormalized until paired arrival labels exist.
- Labels pair only with readings retrieved before arrival within 30 min. Unknown
  backend freshness still caps confidence. Known-old proxy readings are excluded.
- Up to 9 paired labels: nearest three label/count samples, distance-weighted heuristic.
- 10+: pool-adjacent-violators monotonic regression; prediction clamps outside range
  and halves confidence. This encodes simple count/crowding monotonic prior, not
  scientific proof. No neural model or unnecessary numerical dependencies.
- Future input uses local weekday/hour history, one mean per dated hour. Cached
  repeated polls cannot masquerade as many independent days.
- Blend available label-calibrated count and user slot means.
  No signal → null score/zero confidence; outages preserve history/decrease confidence.
- Source reliability uses exponential update from pre-label prediction error. Source
  prediction never trains on the label it is evaluated against.
- Historical lookback: 90 days. Confidence is heuristic evidence support, never
  measured validation accuracy or empty-gym guarantee. Holiday/weather not modeled.
- Discomfort summary only after 3+ busy/packed visits with paired counts. It describes
  median *reported* count associated with those ratings, not max capacity or hard limit.

SQLite stores readings, retrieval health, frozen label features and learned summary.
Calendar descriptions label personal score explicitly; synthetic evidence stays demo.
No model calls needed for collection/scoring. Agent explains only meaningful decisions.

## Outstanding

Real arrival labels, refresh-cadence investigation and live MySports
verification remain operator-dependent. Do not advertise validated crowd accuracy.
