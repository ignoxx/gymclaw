import { execFile } from "node:child_process";

// Cheap pre-check so ordinary chat never pays for a Python start-up.
export const SET_LIKE = /^\s*\d+(?:[.,]\d+)?\s*(?:kg|reps?)?\s*[x×*]\s*\d+(?:[.,]\d+)?\s*(?:kg|reps?)?\s*$|^\s*\d+\s*reps?\s*(?:at|@)\s*\d+(?:[.,]\d+)?\s*(?:kg)?\s*$/i;
export const TICK_MS = 5000;
const ACTIONS = ["status", "start", "card", "swap", "later", "next", "end"];

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

/** Card text with the rest on top while resting: the card shows what comes *after* the rest. */
export function withRest(card, nowMs) {
  if (!card.rest_until) return card.text;
  const left = Math.max(0, Math.ceil((Date.parse(card.rest_until) - nowMs) / 1000));
  return left ? `⏱ Rest ${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")} · next up\n${card.text}` : `⏱ Rest over · go\n${card.text}`;
}

const keyboard = (buttons) => buttons.map((row) => row.map((b) => ({ text: b.text, callback_data: b.data })));

/**
 * Renders coach results in one owner chat. Keeps one "live" card (the current exercise, with buttons and
 * countdown) plus "extras" (swap options/menu). Old buttons are removed as soon as the workout moves on.
 */
export function createCoach({ telegram, chatId, now = () => Date.now(), timers = globalThis, log = console, onRestOver }) {
  let live = null;
  let extras = [];

  async function quietly(promise) {
    try {
      await promise;
    } catch (error) {
      // "message is not modified" and deleted messages are expected during edits.
      log.debug?.(`gymclaw-coach edit skipped: ${error?.message ?? error}`);
    }
  }

  async function retire(entry, ack) {
    if (entry.timer) timers.clearInterval(entry.timer);
    await quietly((await telegram()).edit(chatId, entry.messageId, entry.card.text + (ack ? `\n${ack}` : ""), []));
  }

  function startCountdown(entry) {
    entry.timer = timers.setInterval(async () => {
      const text = withRest(entry.card, now());
      if (!text.startsWith("⏱ Rest over")) {
        await quietly((await telegram()).edit(chatId, entry.messageId, text, keyboard(entry.card.buttons)));
        return;
      }
      timers.clearInterval(entry.timer);
      entry.timer = null;
      // Rest is over: a fresh card with buttons notifies the phone; edits are silent.
      const result = live === entry && onRestOver ? await onRestOver().catch(() => null) : null;
      if (result?.cards?.length) await apply(result);
      else await quietly((await telegram()).edit(chatId, entry.messageId, text, keyboard(entry.card.buttons)));
    }, TICK_MS);
  }

  /** Apply one coach result. `tapped` is the pressed message; `inboundMessageId` is a typed set to react to. */
  async function apply(result, { tapped, inboundMessageId } = {}) {
    const api = await telegram();
    if (inboundMessageId && result.react) await quietly(api.react(chatId, inboundMessageId, result.react));
    const known = [live, ...extras].find((entry) => entry && tapped && entry.messageId === tapped.messageId);
    if (known && result.keep && !result.ack) {
      // Swap menu opened from the live card: that card stays usable ("I'll wait").
    } else if (known) {
      await retire(known, result.ack);
      if (known === live) live = null;
      extras = extras.filter((entry) => entry !== known);
    } else if (tapped && result.ack) {
      // Message from before a gateway restart: we only know its visible text.
      const text = (tapped.text ?? "").replace(/^⏱ Rest[^\n]*\n/, "");
      await quietly(api.edit(chatId, tapped.messageId, `${text}\n${result.ack}`, []));
    } else if (!tapped && result.ack && live) {
      await retire(live, result.ack);
      live = null;
    }
    if (!result.cards?.length) return;
    if (!result.keep) {
      for (const entry of [live, ...extras]) if (entry) await retire(entry);
      live = null;
      extras = [];
    }
    for (const [index, card] of result.cards.entries()) {
      const messageId = await api.send(chatId, withRest(card, now()), { photo: card.photo, buttons: keyboard(card.buttons) });
      const entry = { messageId, card };
      if (result.keep || index < result.cards.length - 1) {
        extras.push(entry);
      } else {
        live = entry;
        if (card.rest_until) startCountdown(entry);
      }
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

  api.on("inbound_claim", async (event) => {
    if (event.channel !== "telegram" || event.isGroup || String(event.senderId) !== ownerId) return;
    const text = String(event.body ?? event.content ?? "");
    if (!SET_LIKE.test(text)) return;
    let result;
    try {
      result = await cli(["coach", "text", "--text", text.trim(), "--request-id", `tg-msg:${event.messageId}`]);
    } catch (error) {
      // Let the agent explain anything unexpected (e.g. no workout in a loggable state).
      api.logger.warn(`gymclaw-coach: typed set not handled: ${error.code ?? error.message}`);
      return;
    }
    if (!result.handled) return;
    await coach.apply(result, { inboundMessageId: event.messageId });
    sync();
    return { handled: true };
  });

  api.registerTool({
    name: "gymclaw_workout",
    description:
      "Drive the live workout in Telegram. Sends exercise cards (image, target, quick buttons) to the owner directly, " +
      "so after calling reply NO_REPLY unless the owner asked a question. Actions: status (read the running workout, sends " +
      "nothing), start (template_id, optional " +
      "planned_session_id), card (resend current card), swap (equipment taken: show same-muscle alternatives), " +
      "later (do current exercise later), next (move on to the next exercise), end (end early, save, show summary).",
    parameters: {
      type: "object",
      additionalProperties: false,
      required: ["action"],
      properties: {
        action: { type: "string", enum: ACTIONS },
        template_id: { type: "string" },
        planned_session_id: { type: "string" },
      },
    },
    async execute(toolCallId, params) {
      const requestId = `tool:${toolCallId}`;
      const args =
        params.action === "status"
          ? ["workout", "current"]
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
