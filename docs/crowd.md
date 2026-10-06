# Crowd data

GymClaw learns when your gym is quiet from a live check-in count, polled every 15 minutes. Every
gym exposes that differently, if at all, so the source is configured per deployment.

## Sources

Private JSON at `data/crowd-config.json` (or the path in `GYMCLAW_CROWD_CONFIG`). Pick one:

```jsonc
// A JSON API: dot path to the number (list indices are numbers)
{"kind": "http", "url": "https://…", "headers": {"x-tenant": "…"}, "json_path": "data.0.count"}

// A web page or other text: the regex's first group is the number
{"kind": "http", "url": "https://…", "regex": "(\\d+) people"}

// Anything else (login, several requests, a page that needs parsing): a script you provide.
// Runs from the repo root, no shell, 30 s timeout. Print the count, alone or as {"value": 12}.
{"kind": "command", "argv": [".venv/bin/python", "data/crowd-source.py"]}

// MySports gyms, built in. A file without "kind" means this.
{"kind": "mysports", "studio_id": "123456", "tenant": "my-gym"}
```

Only GET, HTTPS, no redirects. The count must be a whole nonnegative number; anything else is
recorded as a failed read, never stored. Errors never echo the config or response.

Check it, without storing anything:

```bash
scripts/gymclaw-tool crowd test
```

Then enable polling with `runtime sync … --with-crowd-poll`. The DB binds to the first source it
reads from; switching sources later needs a fresh DB.

**Docker:** the egress proxy only allows known hosts. Add your source's host(s) to
`EGRESS_EXTRA_HOSTS` and redeploy. Put command scripts in `data/` so they live in the persistent
volume. `.venv/bin/python` has `requests` available.

## Finding your gym's source

Most gyms with a "how busy is it" number in their app or website get it from a public endpoint.
Finding it is a one-off job for whatever coding agent you use, or for you with browser dev tools.
A prompt that works:

> My gym is **NAME, ADDRESS** (website/app: …). Find out whether it publishes a live check-in or
> occupancy count: its website, its member app's backend, or the booking platform it uses
> (MySports, Eversports, Virtuagym, PerfectGym, …). I need the current number of people, not a
> percentage if a count exists. Then write a GymClaw crowd source config following
> https://github.com/ignoxx/gymclaw/blob/main/docs/crowd.md: prefer `http` with `json_path`, use a
> `command` script in `data/` only if one request isn't enough. Tell me which hosts to add to
> `EGRESS_EXTRA_HOSTS`, and verify with `scripts/gymclaw-tool crowd test`.

No public count? Skip it. Planning still works, and your answers after each workout teach it
which hours feel busy.

## MySports notes

`GET https://www.mysports.com/nox/public/v1/studios/{studio_id}/utilization/v2/active-checkin` with
header `x-tenant`, response `{"value": 7}`. No session cookie needed. Undocumented route, no
observation time or cache age, so freshness stays unknown. `GYMCLAW_MYSPORTS_STUDIO_ID` and
`GYMCLAW_MYSPORTS_TENANT` override the file.

## What the forecast uses

1. Reported count: a whole number with local retrieval time. May lag reality.
2. Felt crowding: the owner's Empty/Fine/Busy/Packed answers after workouts, a personal score.

Percentages, capacity guesses and undated "typical week" profiles are never treated as counts.
Without a source, (2) alone still learns which hours feel quiet.

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
