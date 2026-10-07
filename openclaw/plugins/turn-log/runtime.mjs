import { clean, createTurnLog, scrub, sha } from "./log.mjs";

const TTL_MS = 30 * 60 * 1000;

const textOf = (message) =>
  typeof message?.content === "string"
    ? message.content
    : (message?.content ?? []).filter((part) => part?.type === "text").map((part) => part.text).join("\n");

const usageOf = (usage) =>
  usage && {
    input: usage.input,
    output: usage.output,
    reasoning: usage.reasoningTokens,
    cacheRead: usage.cacheRead,
    cacheWrite: usage.cacheWrite,
  };

const size = (value) => {
  try {
    return JSON.stringify(value ?? null).length;
  } catch {
    return undefined;
  }
};

/** Tool result as logged: size + hash of the whole thing, a short text preview, image count. */
function resultOf(result) {
  const parts = Array.isArray(result?.content) ? result.content : [];
  const text = parts.length ? parts.filter((part) => part?.type === "text").map((part) => part.text).join("\n") : typeof result === "string" ? result : "";
  const chars = size(result);
  return { chars, sha: chars === undefined ? undefined : sha(JSON.stringify(result ?? null)), images: parts.filter((part) => part?.type === "image").length || undefined, preview: scrub(text, 1000) };
}

/**
 * Observe every agent turn and append one JSONL event per hook (see README for the schema).
 * Correlation: `run` is OpenClaw's runId; inbound/outbound hooks carry only `session`, so readers
 * join them to runs by session and time (summarize.mjs does). Handlers only read and enqueue.
 */
export function registerTurnLog(api, { log = createTurnLog(process.env.GYMCLAW_TURN_LOG_DIR), now = Date.now } = {}) {
  // runId → history length at llm_input, so agent_end can tell this run's messages from history.
  const runs = new Map();
  const safe = (handler) => (event, ctx = {}) => {
    try {
      handler(event ?? {}, ctx);
    } catch (error) {
      api.logger?.debug?.(`gymclaw-turn-log: ${error?.message ?? error}`);
    }
  };

  api.on("message_received", safe((event, ctx) => {
    log({
      ev: "inbound",
      session: ctx.sessionKey ?? event.sessionKey,
      channel: ctx.channelId,
      msg: event.messageId ?? ctx.messageId,
      sentAt: event.timestamp,
      chars: String(event.content ?? "").length,
      text: scrub(event.content),
    });
  }));

  api.on("llm_input", safe((event, ctx) => {
    const run = event.runId ?? ctx.runId;
    const history = event.historyMessages?.length ?? 0;
    const at = now();
    for (const [key, value] of runs) if (at - value.at > TTL_MS) runs.delete(key);
    if (run && !runs.has(run)) runs.set(run, { history, at });
    log({
      ev: "llm_input",
      run,
      session: ctx.sessionKey,
      trigger: ctx.trigger,
      job: ctx.jobId,
      provider: event.provider,
      model: event.model,
      history,
      historyChars: size(event.historyMessages),
      systemChars: event.systemPrompt?.length,
      systemSha: event.systemPrompt ? sha(event.systemPrompt) : undefined,
      tools: event.tools?.length,
      toolsChars: size(event.tools),
      images: event.imagesCount || undefined,
      chars: event.prompt?.length,
      prompt: scrub(event.prompt),
    });
  }));

  api.on("model_call_ended", safe((event, ctx) => {
    log({
      ev: "model_call",
      run: event.runId ?? ctx.runId,
      call: event.callId,
      provider: event.provider,
      model: event.model,
      ms: event.durationMs,
      ttfbMs: event.timeToFirstByteMs,
      outcome: event.outcome,
      error: event.failureKind ?? event.errorCategory,
      reqBytes: event.requestPayloadBytes,
      resBytes: event.responseStreamBytes,
      budget: event.contextTokenBudget,
    });
  }));

  api.on("llm_output", safe((event, ctx) => {
    const last = event.lastAssistant;
    log({
      ev: "llm_output",
      run: event.runId ?? ctx.runId,
      model: event.resolvedRef ?? event.model,
      usage: { ...usageOf(event.usage), reasoning: last?.usage?.reasoningTokens },
      stop: last?.stopReason,
      error: last?.errorMessage ? scrub(last.errorMessage, 500) : undefined,
      budget: event.contextTokenBudget,
    });
  }));

  api.on("after_tool_call", safe((event, ctx) => {
    log({
      ev: "tool",
      run: event.runId ?? ctx.runId,
      call: event.toolCallId ?? ctx.toolCallId,
      name: event.toolName,
      ms: event.durationMs,
      ok: !event.error,
      error: event.error ? scrub(event.error, 500) : undefined,
      args: clean(event.params),
      result: resultOf(event.result),
    });
  }));

  api.on("agent_end", safe((event, ctx) => {
    const run = event.runId ?? ctx.runId;
    const messages = Array.isArray(event.messages) ? event.messages : [];
    const own = messages.slice(runs.get(run)?.history ?? 0);
    runs.delete(run);
    const assistant = own.filter((message) => message?.role === "assistant");
    const reply = textOf(assistant.at(-1)).trim();
    log({
      ev: "run_end",
      run,
      session: ctx.sessionKey,
      trigger: ctx.trigger,
      ok: event.success,
      error: event.error ? scrub(event.error, 500) : undefined,
      ms: event.durationMs,
      // One entry per model call, in order; pairs with this run's model_call events.
      calls: assistant.map((message) => ({
        ...usageOf(message.usage),
        stop: message.stopReason,
        toolCalls: Array.isArray(message.content) ? message.content.filter((part) => part?.type === "toolCall").length : 0,
      })),
      noReply: /^NO_REPLY$/.test(reply) || undefined,
      reply: scrub(reply),
    });
  }));

  api.on("message_sent", safe((event, ctx) => {
    log({
      ev: "outbound",
      session: ctx.sessionKey ?? event.sessionKey,
      run: event.runId ?? ctx.runId,
      channel: ctx.channelId,
      msg: event.messageId,
      ok: event.success,
      error: event.error ? scrub(event.error, 500) : undefined,
      text: scrub(event.content),
    });
  }));
}
