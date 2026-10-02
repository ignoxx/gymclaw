import assert from "node:assert/strict";
import test from "node:test";
import { formatFailure, registerFailureDetails } from "./runtime.mjs";

const generic = "⚠️ Agent couldn't generate a response. Please try again.";
const runId = "d728a017-16c1-48a7-85f4-edaee525fe75";

function hooks() {
  const handlers = new Map();
  registerFailureDetails({ on: (name, handler) => handlers.set(name, handler) });
  return (name, event, ctx = {}) => handlers.get(name)(event, ctx);
}

test("output exhaustion reports reasoning usage without calling it context overflow", () => {
  const result = formatFailure(generic, { stopReason: "length", output: 4096, reasoning: 4096 });
  assert.match(result, /OUTPUT_LIMIT/);
  assert.match(result, /output 4,096 tokens · reasoning 4,096/);
  assert.doesNotMatch(result, /CONTEXT_LIMIT/);
  assert.match(result, /Action status unknown/);
});

test("session-correlated hooks rewrite Telegram failure and preserve replay warning", () => {
  const emit = hooks();
  const ctx = { runId, sessionKey: "agent:main:main" };
  emit("llm_input", { runId }, ctx);
  emit("after_tool_call", { runId }, ctx);
  emit("llm_output", { runId, lastAssistant: { stopReason: "length", usage: { output: 16384 } } }, ctx);
  emit("agent_end", { runId, success: false }, ctx);
  const result = emit("message_sending", { content: generic }, { channelId: "telegram", sessionKey: ctx.sessionKey });
  assert.match(result.content, /OUTPUT_LIMIT/);
  assert.match(result.content, /Check saved state before retrying/);
  assert.match(result.content, /Ref: d728a017/);
  assert.equal(emit("message_sending", { content: "Workout saved" }, { channelId: "telegram" }), undefined);
  assert.equal(emit("message_sending", { content: generic }, { channelId: "discord" }), undefined);
});

test("raw errors and secrets never appear in user-facing error", () => {
  const result = formatFailure(generic, { error: "HTTP 429 api_key=secret https://private.example" });
  assert.match(result, /RATE_LIMIT/);
  assert.doesNotMatch(result, /secret|private\.example/);
  assert.match(formatFailure(generic, { error: "deadline exceeded" }), /TIMEOUT/);
  assert.match(formatFailure(generic, { error: "context limit exceeded" }), /CONTEXT_LIMIT/);
});

test("existing core side-effect warning survives missing hook metadata", () => {
  const result = formatFailure("⚠️ Agent couldn't generate a response. Note: some tool actions may have already been executed — please verify before retrying.");
  assert.match(result, /Tool actions may already have run/);
});

test("concurrent session runs do not misattribute failure metadata", () => {
  const emit = hooks();
  const ctx = { sessionKey: "agent:main:main" };
  emit("llm_input", { runId }, ctx);
  emit("llm_output", { runId, lastAssistant: { stopReason: "length" } }, ctx);
  emit("llm_input", { runId: "another-run" }, ctx);
  const result = emit("message_sending", { content: generic }, { channelId: "telegram", ...ctx });
  assert.match(result.content, /NO_ANSWER/);
  assert.match(result.content, /Action status unknown/);
  assert.doesNotMatch(result.content, /OUTPUT_LIMIT/);
});
