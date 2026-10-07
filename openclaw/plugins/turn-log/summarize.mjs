#!/usr/bin/env node
// Summarize turn logs: latency per flow, tokens per model turn, model vs fast-path counts.
//   node summarize.mjs [dir-or-files…] [--since YYYY-MM-DD] [--json]
// Default dir: $GYMCLAW_TURN_LOG_DIR, else /opt/gymclaw/data/turns.
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

/** Nearest-rank percentile of numbers (undefined when empty). */
export function pct(values, p) {
  const sorted = values.filter(Number.isFinite).sort((a, b) => a - b);
  return sorted.length ? sorted[Math.max(0, Math.ceil((p / 100) * sorted.length) - 1)] : undefined;
}

const stats = (values) => ({ n: values.length, p50: pct(values, 50), p95: pct(values, 95) });
const sum = (values) => values.reduce((total, value) => total + (value ?? 0), 0);

/** Short name for a tool call: `workout.log`, `tool coach text`, `message.send`, … */
export function toolLabel(event) {
  const args = event.args ?? {};
  if (event.name === "gymclaw_workout") return `workout.${args.action}`;
  if (event.name === "message") return `message.${args.action}`;
  if (event.name === "exec") {
    const match = /gymclaw-tool\s+([\w-]+)(?:\s+([\w-]+))?/.exec(String(args.command ?? ""));
    return match ? `tool ${match[1]}${match[2] ? ` ${match[2]}` : ""}` : "exec";
  }
  return event.name;
}

/**
 * Join raw events into turns. Model runs are keyed by `run`; inbound and outbound messages only
 * carry a session, so they belong to the run in that session they fall between. Wall time runs
 * from the earliest inbound message the run answered (or its first model input, for cron and
 * heartbeat) to its last outbound message (or run end, for NO_REPLY).
 */
export function turns(events) {
  const sorted = [...events].sort((a, b) => String(a.ts).localeCompare(String(b.ts)));
  const fastMsgs = new Set(sorted.filter((e) => e.ev === "fast" && e.flow === "text" && e.handled).map((e) => e.msg));
  const runs = new Map();
  for (const event of sorted) {
    if (!event.run || !["llm_input", "model_call", "llm_output", "tool", "run_end"].includes(event.ev)) continue;
    const run = runs.get(event.run) ?? { run: event.run, start: Date.parse(event.ts), tools: [], modelCalls: [], outputs: [] };
    runs.set(event.run, run);
    if (event.ev === "llm_input") {
      run.session ??= event.session;
      run.trigger ??= event.trigger;
    }
    if (event.ev === "tool") run.tools.push(event);
    if (event.ev === "model_call") run.modelCalls.push(event);
    if (event.ev === "llm_output") run.outputs.push(event);
    if (event.ev === "run_end") {
      run.end = run.last = Date.parse(event.ts);
      run.result = event;
      run.session ??= event.session;
      run.trigger ??= event.trigger;
    }
  }
  const bySession = new Map();
  for (const run of runs.values()) {
    if (!run.session) continue;
    bySession.set(run.session, [...(bySession.get(run.session) ?? []), run]);
  }
  for (const list of bySession.values()) list.sort((a, b) => a.start - b.start);
  // Run in this session that a message at `at` belongs to: inbound → next run, outbound → current run.
  const owner = (session, at, inbound) => {
    const list = bySession.get(session) ?? [];
    return inbound ? list.find((run) => run.start >= at && run.trigger === "user") : list.findLast((run) => run.start <= at);
  };
  for (const event of sorted) {
    const at = Date.parse(event.ts);
    if (event.ev === "inbound" && !fastMsgs.has(event.msg)) {
      const run = owner(event.session, at, true);
      if (run && run.inboundAt === undefined) run.inboundAt = at;
    }
    if (event.ev === "outbound") {
      const run = owner(event.session, at, false);
      if (run) run.last = Math.max(run.last ?? at, at);
    }
  }
  return [...runs.values()].map((run) => {
    const calls = run.result?.calls?.length ? run.result.calls : run.outputs.map((output) => ({ ...output.usage, stop: output.stop }));
    return {
      run: run.run,
      trigger: run.trigger ?? "unknown",
      flow: `model ${run.trigger ?? "unknown"}/${run.tools.length ? toolLabel(run.tools[0]) : "chat"}`,
      wallMs: (run.last ?? run.start) - (run.inboundAt ?? run.start),
      ok: run.result?.ok,
      noReply: Boolean(run.result?.noReply),
      calls: calls.length,
      context: Math.max(0, ...calls.map((c) => (c.input ?? 0) + (c.cacheRead ?? 0) + (c.cacheWrite ?? 0))),
      input: sum(calls.map((c) => c.input)),
      cacheRead: sum(calls.map((c) => c.cacheRead)),
      output: sum(calls.map((c) => c.output)),
      reasoning: sum(calls.map((c) => c.reasoning)),
      lengthStops: calls.filter((c) => c.stop === "length").length,
      modelCalls: run.modelCalls,
      tools: run.tools,
    };
  });
}

/** Everything the CLI prints, as data. */
export function summarize(events) {
  const model = turns(events);
  const fast = events.filter((e) => e.ev === "fast");
  const fastFlow = (e) => (e.flow === "tap" ? `fast tap ${e.action}` : e.handled ? "fast text" : "fast text (to model)");
  const flows = new Map();
  for (const turn of model) flows.set(turn.flow, [...(flows.get(turn.flow) ?? []), turn.wallMs]);
  for (const e of fast) flows.set(fastFlow(e), [...(flows.get(fastFlow(e)) ?? []), e.ms]);
  const tools = new Map();
  for (const tool of model.flatMap((turn) => turn.tools)) {
    const entry = tools.get(toolLabel(tool)) ?? { ms: [], errors: 0, resultChars: [] };
    entry.ms.push(tool.ms);
    entry.resultChars.push(tool.result?.chars);
    if (!tool.ok) entry.errors++;
    tools.set(toolLabel(tool), entry);
  }
  const modelCalls = model.flatMap((turn) => turn.modelCalls);
  const callErrors = {};
  for (const call of modelCalls) if (call.outcome === "error") callErrors[call.error ?? "unknown"] = (callErrors[call.error ?? "unknown"] ?? 0) + 1;
  return {
    counts: {
      modelTurns: model.length,
      fastHandled: fast.filter((e) => e.flow === "tap" || e.handled).length,
      fastToModel: fast.filter((e) => e.flow === "text" && !e.handled).length,
    },
    flows: [...flows].map(([flow, ms]) => ({ flow, ...stats(ms) })).sort((a, b) => b.n - a.n),
    tokens: Object.fromEntries(
      ["context", "input", "cacheRead", "output", "reasoning", "calls"].map((key) => [key, stats(model.map((turn) => turn[key]))]),
    ),
    modelCalls: { ...stats(modelCalls.map((c) => c.ms)), ttfb: stats(modelCalls.map((c) => c.ttfbMs)), errors: callErrors },
    tools: [...tools].map(([name, t]) => ({ name, ...stats(t.ms), errors: t.errors, resultP95: pct(t.resultChars, 95) })).sort((a, b) => b.n - a.n),
    failures: {
      failedRuns: model.filter((turn) => turn.ok === false).length,
      lengthStops: sum(model.map((turn) => turn.lengthStops)),
      noReply: model.filter((turn) => turn.noReply).length,
    },
  };
}

/** Read every event from JSONL files or directories of them, skipping broken lines. */
export function readEvents(paths, since) {
  const files = paths.flatMap((path) =>
    statSync(path).isDirectory()
      ? readdirSync(path).filter((name) => name.endsWith(".jsonl") && (!since || name.slice(0, 10) >= since)).sort().map((name) => join(path, name))
      : [path],
  );
  return files.flatMap((file) =>
    readFileSync(file, "utf8").split("\n").flatMap((line) => {
      try {
        return line ? [JSON.parse(line)] : [];
      } catch {
        return [];
      }
    }),
  );
}

function format(summary) {
  const n = (value) => (value === undefined ? "-" : Math.round(value).toLocaleString("en-US"));
  const row = (cells, widths) => cells.map((cell, i) => (i ? String(cell).padStart(widths[i]) : String(cell).padEnd(widths[i]))).join("  ");
  const { counts, tokens, modelCalls, failures } = summary;
  const total = counts.modelTurns + counts.fastHandled;
  const lines = [
    `Turns: ${counts.modelTurns} model, ${counts.fastHandled} fast path (${total ? Math.round((100 * counts.fastHandled) / total) : 0}% fast); ${counts.fastToModel} typed sets fell through to the model`,
    "",
    row(["Flow (wall ms)", "n", "p50", "p95"], [34, 5, 8, 8]),
    ...summary.flows.map((f) => row([f.flow, f.n, n(f.p50), n(f.p95)], [34, 5, 8, 8])),
    "",
    row(["Tokens per model turn", "p50", "p95"], [34, 10, 10]),
    ...Object.entries(tokens).map(([key, s]) => row([key === "context" ? "context (largest call)" : key, n(s.p50), n(s.p95)], [34, 10, 10])),
    "",
    `Model calls: ${modelCalls.n}, p50 ${n(modelCalls.p50)} ms, p95 ${n(modelCalls.p95)} ms; first byte p50 ${n(modelCalls.ttfb.p50)} ms, p95 ${n(modelCalls.ttfb.p95)} ms` +
      (Object.keys(modelCalls.errors).length ? `; errors ${JSON.stringify(modelCalls.errors)}` : ""),
    "",
    row(["Tool", "n", "p50 ms", "p95 ms", "errors", "p95 chars"], [34, 5, 8, 8, 6, 10]),
    ...summary.tools.map((t) => row([t.name, t.n, n(t.p50), n(t.p95), t.errors, n(t.resultP95)], [34, 5, 8, 8, 6, 10])),
    "",
    `Failures: ${failures.failedRuns} failed runs, ${failures.lengthStops} length stops; ${failures.noReply} NO_REPLY turns`,
  ];
  return lines.join("\n");
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  const args = process.argv.slice(2);
  const since = args.includes("--since") ? args[args.indexOf("--since") + 1] : undefined;
  const paths = args.filter((arg, i) => !arg.startsWith("--") && args[i - 1] !== "--since");
  const events = readEvents(paths.length ? paths : [process.env.GYMCLAW_TURN_LOG_DIR || "/opt/gymclaw/data/turns"], since);
  const summary = summarize(events);
  console.log(args.includes("--json") ? JSON.stringify(summary, null, 2) : format(summary));
}
