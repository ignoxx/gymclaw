const TTL_MS = 10 * 60 * 1000;

/** One owner-facing `message send` per agent run. A second send is blocked with a reason the model
 * can act on, so a run can't spiral into re-asking the same question (reactions stay allowed). */
export function registerSendGuard(api) {
  const sent = new Map(); // runId → time of the first send
  api.on("before_tool_call", (event, ctx) => {
    if (event.toolName !== "message" || event.params?.action !== "send") return;
    const runId = event.runId ?? ctx?.runId;
    if (!runId) return;
    const now = Date.now();
    for (const [key, at] of sent) if (now - at > TTL_MS) sent.delete(key);
    if (!sent.has(runId)) {
      sent.set(runId, now);
      return;
    }
    return {
      block: true,
      blockReason: "Already messaged the owner this turn. Don't send again: finish with NO_REPLY and wait for their answer. Put choices as buttons in the one message you send.",
    };
  });
}
