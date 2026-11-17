# Agent turns: capture, copy, query

Every agent turn is logged in full when `GYMCLAW_TURN_LOG_DIR` is set (the image sets it to
`data/turns`). The events and their fields are documented in
[`src/agent/turnlog.rs`](../src/agent/turnlog.rs).

The files hold full prompts (chat, notes, calendar), so they're owner-only (0600). Keep copies that
way and never commit them.

## Copy them off the host

```bash
scripts/pull-turns <ssh host> ~/gymclaw-turns            # turn log → ~/gymclaw-turns/turns
scripts/pull-turns <ssh host> ~/gymclaw-turns --with-db  # plus a consistent DB snapshot
```

It reads the running container over `ssh … docker exec` and replaces the local copy atomically;
run it nightly from cron (example in the script's header).

## Query with DuckDB

```sql
-- One row per event, every day file.
CREATE VIEW ev AS SELECT * FROM read_json_auto('~/gymclaw-turns/turns/*.jsonl', union_by_name = true);

-- Turns that went wrong, newest first.
SELECT ts, run, flags, error, reply FROM ev WHERE ev = 'run_end' AND (NOT ok OR flags IS NOT NULL) ORDER BY ts DESC;

-- Steps and tokens per run, and which release served it.
SELECT i.run, i.commit, i.model, count(*) AS steps, sum(o.usage.input) AS input, sum(o.usage.output) AS output
FROM ev i JOIN ev o ON o.run = i.run AND o.ev = 'llm_output'
WHERE i.ev = 'llm_input' GROUP BY ALL ORDER BY min(i.ts) DESC;

-- Tools that fail most.
SELECT name, count(*) FILTER (WHERE NOT ok) AS failed, count(*) AS calls FROM ev WHERE ev = 'tool' GROUP BY name ORDER BY failed DESC;

-- Typed sets the coach couldn't read (parser improvements).
SELECT ts, text, error FROM ev WHERE ev = 'fast' AND flow = 'text' AND NOT handled;
```

## Rewind a turn

`gymclaw rewind <run> --db copy.db --turns ~/gymclaw-turns/turns` undoes that run's tool calls on a
DB copy from their inverse changesets, so `gymclaw replay` starts from the state the turn saw. A row
changed since (a later turn, a tap) aborts the rewind without writing: rewind later runs first.

## Backfill and labels

```bash
gymclaw backfill --db ~/gymclaw-turns/gymclaw.db --turns ~/gymclaw-turns/turns --git . --out turns-index.jsonl
```

One record per past turn, read-only on the copies. Exactly captured turns are indexed as
`source: capture` (their trajectory stays in the log); older ones are rebuilt as `source: backfill`
with a `fidelity`: `events+chat` (log previews plus full chat texts), `events` or `chat` (no log,
only the reply's `run` in `chat_message`). Each carries its mutations (`agent_event` by request ID
`tg-<message>-<n>`), the release (`--git`: newest first-parent commit before it, so merge time, not
deploy time) and labels: log flags, reactions, and inferred ones from the owner's next message
within 10 minutes (`corrected`, `undone`, `repeat_ask`; English phrasing only). Past system prompts
and reasoning can't be recovered: backfilled turns serve root-cause analysis and replay seeds, and
fine-tuning only after a strong model relabels them from replay.
