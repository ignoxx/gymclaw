import assert from "node:assert/strict";
import test from "node:test";
import { registerOneReply } from "./runtime.mjs";

function hooks(clock = { t: 0 }) {
  const handlers = new Map();
  registerOneReply({ on: (name, handler) => handlers.set(name, handler) }, { now: () => clock.t });
  return (name, event, ctx = {}) => handlers.get(name)(event, ctx);
}
const sent = (action = "send") => ({ toolName: "message", params: { action }, result: { content: [{ type: "text", text: '{"ok":true,"messageId":"489"}' }] } });
const payload = (runId, text) => ({ runId, kind: "final", payload: { text } });

test("narration after the message tool answered is dropped (2026-10-09: three replies to one move)", () => {
  const emit = hooks();
  emit("after_tool_call", { runId: "r1", ...sent() });
  assert.deepEqual(emit("reply_payload_sending", payload("r1", "The planner picked 14:00 — fixing to 12:30:")), { cancel: true });
  assert.deepEqual(emit("reply_payload_sending", payload("r1", "Moved — Pull today at 12:30.")), { cancel: true });
  assert.equal(emit("reply_payload_sending", payload("r2", "Next run answers normally.")), undefined);
});

test("without a message tool send, only the first payload goes out", () => {
  const emit = hooks();
  assert.equal(emit("reply_payload_sending", payload("r1", "Moved to 12:30.")), undefined);
  assert.deepEqual(emit("reply_payload_sending", payload("r1", "Moved — 12:30.")), { cancel: true });
});

test("reactions, failed sends, status reads and runless payloads don't count", () => {
  const emit = hooks();
  emit("after_tool_call", { runId: "r1", ...sent("react") });
  emit("after_tool_call", { runId: "r1", toolName: "message", params: { action: "send" }, error: "blocked" });
  emit("after_tool_call", { runId: "r1", toolName: "gymclaw_workout", params: { action: "status" }, result: { content: [{ type: "text", text: '{"ok":true}' }] } });
  emit("after_tool_call", { runId: "r1", toolName: "gymclaw_workout", params: { action: "start" }, result: { content: [{ type: "text", text: '{"ok":false,"code":"X"}' }] } });
  assert.equal(emit("reply_payload_sending", payload("r1", "Here's why it failed.")), undefined);
  assert.equal(emit("reply_payload_sending", { kind: "final", payload: { text: "Gym soon." } }), undefined);
  assert.equal(emit("reply_payload_sending", { kind: "final", payload: { text: "Leave now." } }), undefined);
});

test("a workout card counts as the reply", () => {
  const emit = hooks();
  emit("after_tool_call", { toolName: "gymclaw_workout", params: { action: "start" }, result: { content: [{ type: "text", text: '{"ok":true,"card":"Bench"}' }] } }, { runId: "r1" });
  assert.deepEqual(emit("reply_payload_sending", payload("r1", "Started!")), { cancel: true });
});
