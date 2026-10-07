import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { createTurnLog } from "./log.mjs";
import { registerTurnLog } from "./runtime.mjs";
import { summarize } from "./summarize.mjs";

const run = "d728a017-16c1-48a7-85f4-edaee525fe75";
const session = "agent:main:telegram:direct:123";

function hooks() {
  const events = [];
  const handlers = new Map();
  registerTurnLog({ on: (name, handler) => handlers.set(name, handler) }, { log: (event) => events.push(event) });
  return { events, emit: (name, event, ctx = {}) => handlers.get(name)(event, ctx) };
}

test("one turn: per-call usage from this run only, secrets redacted, blobs hashed", () => {
  const { events, emit } = hooks();
  const ctx = { runId: run, sessionKey: session, trigger: "user" };
  const history = [{ role: "user", content: "old" }, { role: "assistant", content: [{ type: "text", text: "old reply" }], usage: { input: 999 } }];
  emit("message_received", { content: "Log 10x40 please", messageId: "42" }, { sessionKey: session, channelId: "telegram" });
  emit("llm_input", { runId: run, provider: "openrouter", model: "glm", systemPrompt: "sys", prompt: "Log 10x40 please", historyMessages: history, imagesCount: 0, tools: [{}] }, ctx);
  emit("after_tool_call", {
    runId: run,
    toolName: "exec",
    params: { command: "gymclaw-tool coach text --text 10x40", env: { OPENROUTER_API_KEY: "sk-or-v1-abcdefabcdefabcdef" } },
    result: { content: [{ type: "text", text: "ok Bearer abc.def" }, { type: "image", data: "A".repeat(5000) }] },
    durationMs: 812,
  }, ctx);
  emit("agent_end", {
    runId: run,
    success: true,
    durationMs: 4000,
    messages: [
      ...history,
      { role: "user", content: "Log 10x40 please" },
      { role: "assistant", content: [{ type: "toolCall" }], usage: { input: 140000, output: 30, reasoningTokens: 20, cacheRead: 5000 }, stopReason: "toolUse" },
      { role: "assistant", content: [{ type: "text", text: "NO_REPLY" }], usage: { input: 140500, output: 5 }, stopReason: "stop" },
    ],
  }, ctx);

  const [inbound, input, tool, end] = events;
  assert.equal(inbound.msg, "42");
  assert.equal(input.history, 2);
  assert.equal(input.trigger, "user");
  assert.equal(tool.args.env.OPENROUTER_API_KEY, "[redacted]");
  assert.equal(tool.result.images, 1);
  assert.doesNotMatch(JSON.stringify(events), /sk-or|abc\.def|AAAA/);
  assert.deepEqual(end.calls.map((c) => [c.input, c.reasoning, c.toolCalls]), [[140000, 20, 1], [140500, undefined, 0]]);
  assert.equal(end.noReply, true);
});

test("a throwing writer never breaks a hook", () => {
  const handlers = new Map();
  registerTurnLog({ on: (name, handler) => handlers.set(name, handler) }, { log: () => { throw new Error("disk full"); } });
  assert.doesNotThrow(() => handlers.get("message_received")({ content: "hi" }, {}));
});

test("writer appends one JSON line per event to the day's file; no dir means off", async () => {
  const dir = mkdtempSync(join(tmpdir(), "turn-log-"));
  const log = createTurnLog(join(dir, "turns"), { now: () => Date.parse("2026-10-08T12:00:00Z") });
  log({ ev: "inbound", text: "a" });
  log({ ev: "outbound", text: "b" });
  await new Promise((resolve) => setTimeout(resolve, 50));
  const lines = readFileSync(join(dir, "turns", "2026-10-08.jsonl"), "utf8").trim().split("\n").map((line) => JSON.parse(line));
  assert.deepEqual(lines.map((line) => line.ev), ["inbound", "outbound"]);
  assert.doesNotThrow(() => createTurnLog(undefined)({ ev: "x" }));
  // A file where the directory should be: writes fail quietly.
  writeFileSync(join(dir, "blocked"), "");
  assert.doesNotThrow(() => createTurnLog(join(dir, "blocked"))({ ev: "x" }));
});

test("summary joins inbound and outbound to runs by session and skips fast-path messages", () => {
  const t = (s) => `2026-10-08T12:00:${String(s).padStart(2, "0")}.000Z`;
  const events = [
    { ts: t(0), ev: "inbound", session, msg: "1", text: "10x40" },
    { ts: t(1), ev: "fast", flow: "text", handled: true, msg: "1", ms: 900 },
    { ts: t(10), ev: "inbound", session, msg: "2", text: "how was my week?" },
    { ts: t(11), ev: "llm_input", run, session, trigger: "user" },
    { ts: t(13), ev: "model_call", run, ms: 2000, ttfbMs: 800, outcome: "completed" },
    { ts: t(14), ev: "tool", run, name: "exec", args: { command: "scripts/gymclaw-tool workout history" }, ms: 700, ok: true, result: { chars: 3000 } },
    { ts: t(17), ev: "run_end", run, session, ok: true, calls: [{ input: 1000, cacheRead: 146000, output: 10 }, { input: 2000, cacheRead: 146000, output: 90, reasoning: 20 }] },
    { ts: t(18), ev: "outbound", session, text: "Strong week." },
    { ts: t(20), ev: "fast", flow: "tap", action: "log", ms: 1200 },
    { ts: t(21), ev: "fast", flow: "text", handled: false, ms: 300 },
  ];
  const summary = summarize(events);
  assert.deepEqual(summary.counts, { modelTurns: 1, fastHandled: 2, fastToModel: 1 });
  const flow = (name) => summary.flows.find((f) => f.flow === name);
  assert.equal(flow("model user/tool workout history").p50, 8000); // inbound 2 at :10 → reply at :18
  assert.equal(flow("fast tap log").p50, 1200);
  assert.equal(summary.tokens.context.p50, 148000);
  assert.equal(summary.tokens.output.p50, 100);
  assert.equal(summary.tools[0].name, "tool workout history");
});
