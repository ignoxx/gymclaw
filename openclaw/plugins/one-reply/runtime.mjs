const TTL_MS = 10 * 60 * 1000;

/** True when a tool call put something in front of the owner: a `message` send, or a workout card. */
export function reachedOwner(event) {
  if (event.error) return false;
  const text = event.result?.content?.find?.((part) => part?.type === "text")?.text ?? "";
  if (/"ok"\s*:\s*false/.test(text)) return false;
  if (event.toolName === "message") return event.params?.action === "send";
  return event.toolName === "gymclaw_workout" && event.params?.action !== "status";
}

/**
 * One owner message per run. Models narrate between tool calls ("The planner picked 14:00, fixing…"),
 * and OpenClaw delivers that leftover text as extra replies after the `message` tool already answered.
 * Once a run has reached the owner, later reply payloads from it are dropped. Payloads without a
 * runId (reminders, deliveries, recovered replays) are never touched.
 */
export function registerOneReply(api, { now = Date.now } = {}) {
  const answered = new Map(); // runId -> time it reached the owner

  function mark(runId) {
    if (!runId) return;
    const at = now();
    for (const [id, seen] of answered) if (at - seen > TTL_MS) answered.delete(id);
    answered.set(runId, at);
  }

  api.on("after_tool_call", (event, ctx) => {
    if (reachedOwner(event)) mark(event.runId ?? ctx.runId);
  });

  api.on("reply_payload_sending", (event, ctx) => {
    const runId = event.runId ?? ctx.runId;
    if (!runId) return;
    if (answered.has(runId)) return { cancel: true };
    mark(runId);
  });
}
