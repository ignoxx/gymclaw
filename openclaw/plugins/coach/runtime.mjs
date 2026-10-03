import { execFile } from "node:child_process";

// Cheap pre-check so ordinary chat never pays for a Python start-up.
export const SET_LIKE = /^\s*\d+(?:[.,]\d+)?\s*(?:kg|reps?)?\s*[x×*]\s*\d+(?:[.,]\d+)?\s*(?:kg|reps?)?\s*$|^\s*\d+\s*reps?\s*(?:at|@)\s*\d+(?:[.,]\d+)?\s*(?:kg)?\s*$/i;
export const TICK_MS = 5000;
const ACTIONS = ["status", "preview", "start", "log", "card", "swap", "later", "next", "end"];

/** Run `gymclaw-tool coach ...`; resolves the JSON `data` or rejects with the CLI error code. */
export function cliRunner(toolPath, { timeoutMs = 20000 } = {}) {
  return (args) =>
    new Promise((resolve, reject) => {
      execFile(toolPath, args, { timeout: timeoutMs, maxBuffer: 1 << 20 }, (_error, stdout) => {
        let parsed;
        try {
          parsed = JSON.parse(String(stdout).trim().split("\n").pop());
        } catch {
          return reject(Object.assign(new Error("GymClaw CLI returned no JSON"), { code: "CLI_FAILED" }));
        }
        if (!parsed.ok) return reject(Object.assign(new Error(parsed.error?.message ?? "GymClaw CLI failed"), { code: parsed.error?.code }));
        resolve(parsed.data);
      });
    });
}

/** Rest cards count down in front of their text: "⏱ 1:25 until Bench set 2/2". */
export function withRest(card, nowMs) {
  if (!card.rest_until) return card.text;
  const left = Math.max(0, Math.ceil((Date.parse(card.rest_until) - nowMs) / 1000));
  return `⏱ ${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")} ${card.text}`;
}

const keyboard = (buttons) => buttons.map((row) => row.map((b) => ({ text: b.text, callback_data: b.data })));
// Countdowns and swap options are throwaway: deleted once replaced, so the chat stays clean.
const throwaway = (entry) => entry.card.kind === "rest" || entry.card.kind === "option";

/**
 * Renders coach results in one owner chat. Keeps one "live" card (current set or rest countdown) and
 * "extras" (swap options). Old buttons disappear as soon as the workout moves on.
 */
export function createCoach({ telegram, chatId, now = () => Date.now(), timers = globalThis, log = console, onRestOver }) {
  let live = null;
  let extras = [];

  async function quietly(promise) {
    try {
      await promise;
    } catch (error) {
      // "message is not modified" and already-deleted messages are expected.
      log.debug?.(`gymclaw-coach edit skipped: ${error?.message ?? error}`);
    }
  }

  async function remove(entry) {
    if (entry.timer) timers.clearInterval(entry.timer);
    await quietly((await telegram()).remove(chatId, entry.messageId));
  }

  async function retire(entry, ack) {
    if (throwaway(entry)) return remove(entry);
    if (entry.timer) timers.clearInterval(entry.timer);
    await quietly((await telegram()).edit(chatId, entry.messageId, entry.card.text + (ack ? `\n${ack}` : ""), []));
  }

  async function clearExtras() {
    for (const entry of extras) await remove(entry);
    extras = [];
  }

  function startCountdown(entry) {
    entry.timer = timers.setInterval(async () => {
      if (Date.parse(entry.card.rest_until) > now()) {
        await quietly((await telegram()).edit(chatId, entry.messageId, withRest(entry.card, now()), keyboard(entry.card.buttons)));
        return;
      }
      timers.clearInterval(entry.timer);
      entry.timer = null;
      // A fresh set card (not an edit) so the phone notifies; the countdown message is deleted.
      const result = live === entry && onRestOver ? await onRestOver().catch(() => null) : null;
      if (result?.cards?.length) await apply(result);
    }, TICK_MS);
  }

  /** Apply one coach result. `tapped` is the pressed message; `inboundMessageId` is a typed set to react to. */
  async function apply(result, { tapped, inboundMessageId } = {}) {
    const api = await telegram();
    if (inboundMessageId && result.react) await quietly(api.react(chatId, inboundMessageId, result.react));
    if (result.standalone) {
      // Previews etc. are their own messages; the live workout card stays as it is.
      for (const card of result.cards ?? []) await api.send(chatId, card.text, { photo: card.photo, buttons: keyboard(card.buttons) });
      return;
    }
    if (result.live_buttons) {
      // Swap: the current card's buttons become wait/later, options go below it.
      if (live) await quietly(api.edit(chatId, live.messageId, withRest(live.card, now()), keyboard(result.live_buttons)));
      await clearExtras();
      for (const card of result.cards ?? []) extras.push({ messageId: await api.send(chatId, card.text, { photo: card.photo, buttons: keyboard(card.buttons) }), card });
      return;
    }
    if (result.restore) {
      await clearExtras();
      if (live) await quietly(api.edit(chatId, live.messageId, withRest(live.card, now()), keyboard(live.card.buttons)));
      return;
    }
    const known = [live, ...extras].find((entry) => entry && tapped && entry.messageId === tapped.messageId);
    if (result.cleanup === "delete") {
      for (const entry of [live, ...extras]) if (entry) await remove(entry);
      if (tapped && !known) await quietly(api.remove(chatId, tapped.messageId));
    } else {
      const target = known ?? (tapped ? null : live);
      if (target) await retire(target, result.ack);
      else if (tapped && result.ack) {
        // Message from before a gateway restart: we only know its visible text.
        await quietly(api.edit(chatId, tapped.messageId, `${tapped.text ?? ""}\n${result.ack}`, []));
      }
      for (const entry of [live, ...extras]) if (entry && entry !== target) await retire(entry);
    }
    live = null;
    extras = [];
    for (const card of result.cards ?? []) {
      live = { messageId: await api.send(chatId, withRest(card, now()), { photo: card.photo, buttons: keyboard(card.buttons) }), card };
      if (card.rest_until) startCountdown(live);
    }
  }

  return { apply, state: () => ({ live, extras }) };
}

/**
 * Rest pings are durable OpenClaw cron jobs created by `runtime sync`. Run it in the background after
 * workout changes; calls arriving while one runs collapse into a single follow-up sync.
 */
export function createSyncer(cli, ownerId, log) {
  let running = null;
  let again = false;
  return function sync() {
    if (running) {
      again = true;
      return running;
    }
    running = (async () => {
      do {
        again = false;
        try {
          await cli(["runtime", "sync", "--telegram-id", ownerId, "--allow-runtime-changes", "--allow-messages"]);
        } catch (error) {
          log.warn(`gymclaw-coach: runtime sync failed: ${error.code ?? error.message}`);
        }
      } while (again);
      running = null;
    })();
    return running;
  };
}

/** Wire hooks, the callback namespace and the agent tool. `deps` lets tests inject fakes. */
export function registerCoach(api, { run, telegram } = {}) {
  const config = api.pluginConfig ?? {};
  const ownerId = String(config.ownerId ?? "");
  if (!/^[1-9]\d{0,19}$/.test(ownerId) || !(run || config.tool)) {
    api.logger.warn("gymclaw-coach disabled: set plugins.entries.gymclaw-coach.config.ownerId and tool");
    return;
  }
  const cli = run ?? cliRunner(config.tool);
  // Owner DM: chat ID equals the owner's Telegram user ID.
  const sync = createSyncer(cli, ownerId, api.logger);
  const coach = createCoach({
    telegram,
    chatId: ownerId,
    log: api.logger,
    // Closing the rest job here keeps the cron fallback ping silent; sync then removes it.
    onRestOver: async () => {
      const result = await cli(["coach", "rest-over"]);
      if (result.rest_over) sync();
      return result;
    },
  });
  const changed = (result) => result.handled !== false && !result.keep && (result.cards?.length || result.ack);

  async function report(error) {
    api.logger.warn(`gymclaw-coach: ${error.code ?? "ERROR"} ${error.message}`);
    await coach.apply({ cards: [{ text: `⚠️ ${error.message}`, buttons: [] }], keep: true });
  }

  api.registerInteractiveHandler({
    channel: "telegram",
    namespace: "gc",
    handler: async (ctx) => {
      if (!ctx.auth?.isAuthorizedSender || ctx.isGroup || String(ctx.senderId) !== ownerId) return { handled: true };
      try {
        const result = await cli(["coach", "tap", "--data", ctx.callback.payload, "--request-id", `tg-cb:${ctx.callbackId}`]);
        await coach.apply(result, { tapped: { messageId: ctx.callback.messageId, text: ctx.callback.messageText } });
        if (changed(result)) sync();
      } catch (error) {
        await report(error);
      }
      return { handled: true };
    },
  });

  // before_dispatch has no Telegram message ID; message_received (fired just before it) does.
  // Remember the latest ID per text so a typed set can get its 👍.
  const recentIds = new Map();
  api.on("message_received", (event) => {
    if (!event.messageId || !SET_LIKE.test(String(event.content ?? ""))) return;
    recentIds.set(String(event.content).trim(), event.messageId);
    if (recentIds.size > 20) recentIds.delete(recentIds.keys().next().value);
  });

  // before_dispatch runs for every inbound message (inbound_claim only fires for plugin-bound chats).
  api.on("before_dispatch", async (event, ctx) => {
    const sender = String(event.senderId ?? ctx?.senderId ?? "").replace(/^telegram:/, "");
    if ((event.channel ?? ctx?.channelId) !== "telegram" || event.isGroup || sender !== ownerId) return;
    const text = [event.content, event.body].map((value) => String(value ?? "").trim()).find((value) => SET_LIKE.test(value));
    if (!text) return;
    const messageId = recentIds.get(text);
    recentIds.delete(text);
    let result;
    try {
      result = await cli(["coach", "text", "--text", text, "--request-id", `tg-msg:${messageId ?? `${event.timestamp ?? Date.now()}:${text}`}`]);
    } catch (error) {
      // Let the agent explain anything unexpected (e.g. no workout in a loggable state).
      api.logger.warn(`gymclaw-coach: typed set not handled: ${error.code ?? error.message}`);
      return;
    }
    if (!result.handled) return;
    await coach.apply(result, { inboundMessageId: messageId });
    sync();
    return { handled: true };
  });

  api.registerTool({
    name: "gymclaw_workout",
    description:
      "Drive the live workout in Telegram. Sends exercise cards (image, target, quick buttons) to the owner directly, " +
      "so after calling reply NO_REPLY unless the owner asked a question. Actions: status (read the running workout, sends " +
      "nothing), preview (next or given planned session: one image with every exercise + plan; use for \"what's on today\"), " +
      "log (text like \"40x10\": logs a set), start (template_id, optional " +
      "planned_session_id), card (resend current card), swap (equipment taken: show same-muscle alternatives), " +
      "later (do current exercise later), next (move on to the next exercise), end (end early, save, show summary).",
    parameters: {
      type: "object",
      additionalProperties: false,
      required: ["action"],
      properties: {
        action: { type: "string", enum: ACTIONS },
        template_id: { type: "string" },
        text: { type: "string", description: "Set for action log, e.g. 40x10" },
        planned_session_id: { type: "string" },
      },
    },
    async execute(toolCallId, params) {
      const requestId = `tool:${toolCallId}`;
      const args =
        params.action === "status"
          ? ["workout", "current"]
          : params.action === "preview"
            ? ["coach", "preview", ...(params.planned_session_id ? ["--planned-session-id", params.planned_session_id] : [])]
            : params.action === "log"
            ? ["coach", "text", "--text", params.text ?? "", "--request-id", requestId]
            : params.action === "start"
          ? ["coach", "start", "--template-id", params.template_id ?? "", ...(params.planned_session_id ? ["--planned-session-id", params.planned_session_id] : []), "--request-id", requestId]
          : params.action === "card"
            ? ["coach", "card"]
            : ["coach", "act", "--action", params.action, "--request-id", requestId];
      try {
        const result = await cli(args);
        if (params.action === "status") return { content: [{ type: "text", text: JSON.stringify({ ok: true, workout: result }) }] };
        await coach.apply(result);
        if (changed(result)) sync();
        return { content: [{ type: "text", text: JSON.stringify({ ok: true, cards_sent: result.cards?.length ?? 0, ack: result.ack ?? null }) }] };
      } catch (error) {
        return { content: [{ type: "text", text: JSON.stringify({ ok: false, code: error.code, message: error.message }) }] };
      }
    },
  });
}
