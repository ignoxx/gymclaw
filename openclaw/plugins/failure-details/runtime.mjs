const GENERIC_FAILURE = /^⚠️ Agent couldn't generate a response\./;
const TTL_MS = 10 * 60 * 1000;

/** Classify locally; never expose raw provider errors, prompts, tool args or results. */
export function formatFailure(content, state = {}) {
  if (!GENERIC_FAILURE.test(content)) return null;
  const error = `${state.error ?? ""} ${state.providerError ?? ""}`.toLowerCase();
  let code = "NO_ANSWER";
  let reason = "Model returned no usable answer; cause not reported.";
  if (state.stopReason === "length") {
    code = "OUTPUT_LIMIT";
    reason = "Model exhausted output/reasoning budget before finishing its answer.";
  } else if (/context.*(exceed|limit|overflow|long)|too many tokens/.test(error)) {
    code = "CONTEXT_LIMIT";
    reason = "Model request exceeded context capacity.";
  } else if (/429|rate.?limit|quota/.test(error)) {
    code = "RATE_LIMIT";
    reason = "Provider rate limit or quota blocked response.";
  } else if (/401|403|unauthoriz|authentication|api key/.test(error)) {
    code = "PROVIDER_AUTH";
    reason = "Provider rejected authentication or access.";
  } else if (/timeout|timed out|deadline/.test(error)) {
    code = "TIMEOUT";
    reason = "Model request exceeded time limit.";
  } else if (/connection|upstream|502|503|504/.test(error)) {
    code = "UPSTREAM_ERROR";
    reason = "Model provider connection failed.";
  }
  const number = (value) => Number.isSafeInteger(value) && value >= 0;
  const metrics = [];
  if (number(state.output)) metrics.push(`output ${state.output.toLocaleString("en-US")} tokens`);
  if (number(state.reasoning)) metrics.push(`reasoning ${state.reasoning.toLocaleString("en-US")}`);
  const sideEffects = content.includes("some tool actions") || (state.toolCalls ?? 0) > 0;
  // Missing hook evidence is not evidence that no tools ran.
  const safety = sideEffects
    ? "Tool actions may already have run. Check saved state before retrying."
    : state.observedInput
      ? "No tool calls observed in this run. No automatic retry performed."
      : "Action status unknown. Check saved state before retrying.";
  const reference = typeof state.runId === "string" && /^[a-f0-9-]{36}$/i.test(state.runId)
    ? ` Ref: ${state.runId.slice(0, 8)}.`
    : "";
  return `⚠️ ${code}: ${reason}${metrics.length ? `\n${metrics.join(" · ")}.` : ""}\n${safety}${reference}`;
}

/** Observe per-run metadata and rewrite only OpenClaw's generic Telegram failure. */
export function registerFailureDetails(api) {
  const runs = new Map();
  const sessions = new Map();
  function stateFor(event, ctx) {
    const runId = event.runId ?? ctx.runId;
    if (!runId) return;
    const now = Date.now();
    for (const [key, value] of runs) {
      if (now - value.updatedAt > TTL_MS) runs.delete(key);
    }
    for (const [key, value] of sessions) {
      if (!runs.has(value.runId)) sessions.delete(key);
    }
    const state = runs.get(runId) ?? { runId, toolCalls: 0 };
    state.updatedAt = now;
    runs.set(runId, state);
    if (ctx.sessionKey) {
      const previous = sessions.get(ctx.sessionKey);
      const overlaps = previous && previous.runId !== runId && !runs.get(previous.runId)?.ended;
      sessions.set(ctx.sessionKey, { runId, ambiguous: Boolean(previous?.ambiguous || overlaps) });
    }
    return state;
  }
  api.on("llm_input", (event, ctx) => {
    const state = stateFor(event, ctx);
    if (state) state.observedInput = true;
  });
  api.on("after_tool_call", (event, ctx) => {
    const state = stateFor(event, ctx);
    if (state) state.toolCalls++;
  });
  api.on("llm_output", (event, ctx) => {
    const state = stateFor(event, ctx);
    if (!state) return;
    const assistant = event.lastAssistant;
    state.stopReason = assistant?.stopReason;
    state.output = assistant?.usage?.output ?? event.usage?.output;
    state.reasoning = assistant?.usage?.reasoningTokens;
    state.providerError = assistant?.errorMessage;
  });
  api.on("model_call_ended", (event, ctx) => {
    const state = stateFor(event, ctx);
    if (state && event.outcome === "error") {
      state.providerError = event.failureKind ?? event.errorCategory;
    }
  });
  api.on("agent_end", (event, ctx) => {
    const state = stateFor(event, ctx);
    if (state) {
      state.error = event.error;
      state.ended = true;
    }
  });
  api.on("message_sending", (event, ctx) => {
    if (ctx.channelId !== "telegram" || !GENERIC_FAILURE.test(event.content)) return;
    // Outbound hooks currently lack runId. Session fallback is bounded by TTL;
    // ambiguous concurrent runs must not be presented as authoritative evidence.
    const session = sessions.get(ctx.sessionKey);
    const runId = ctx.runId ?? (session?.ambiguous ? undefined : session?.runId);
    const candidate = runs.get(runId);
    const state = candidate && Date.now() - candidate.updatedAt <= TTL_MS ? candidate : undefined;
    const content = formatFailure(event.content, state);
    if (content) return { content };
  });
}
