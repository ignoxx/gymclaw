# Crowd evidence and model limits

MySports integration follows reviewed handoff/FINDINGS and captured public response:

- Studio `1234567890`, tenant `fixture-tenant`, Europe/Berlin.
- GET `https://www.mysports.com/nox/public/v1/studios/1234567890/utilization/v2/active-checkin`
- Header `x-tenant: fixture-tenant`; captured response `{"value":7}`.
- No SESSION cookie needed. Code neither accepts nor stores one.
- Response supplies no observation time, cache age, capacity or checkout semantics.
- Undocumented observed route, not guaranteed stable API. No live call during build.

Reference investigation: September 30, 2026, adjacent research repo
`clawchallenge/research/mysports/IMPLEMENTATION_HANDOFF.md` and `FINDINGS.md`.
Tests reproduce response contract synthetically; they are not fabricated historical
live requests. Research capture timestamps are not reused as current observations.

## Distinct quantities

1. Reported active count: raw integer, local retrieval timestamp. May lag reality.
2. `/today`: dated hourly percentages with `current`, unknown aggregation method.
3. `/historic/week`: undated weekday percentages with no sample counts. All 168
   observed values were 5%; meaning unknown (possible floor/fallback/baseline).
4. User felt crowding: EMPTY/FINE/BUSY/PACKED labels define personal score.
5. Google busyness: optional percentage proxy from verified adapter/explicit demo.

Only (1), (4), and fixture/verified (5) enter implemented predictor. No invented gym
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
- Blend available label-calibrated count, optional Google proxy and user slot means.
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

Real arrival labels, refresh-cadence investigation, Google acquisition and live MySports
verification remain operator-dependent. Do not advertise validated crowd accuracy.
