import assert from "node:assert/strict";
import test from "node:test";
import { registerSendGuard } from "./guard.mjs";

function hook() {
  let handler;
  registerSendGuard({ on: (_, h) => (handler = h) });
  return (params, runId = "run-1") => handler({ toolName: "message", params, runId }, {});
}

test("second send in the same run is blocked, reactions and other runs pass", () => {
  const call = hook();
  assert.equal(call({ action: "send", message: "When today?" }), undefined);
  assert.equal(call({ action: "react", emoji: "👍" }), undefined);
  assert.equal(call({ action: "send", message: "Pick a time:" }).block, true);
  assert.equal(call({ action: "send", message: "Next turn" }, "run-2"), undefined);
});
