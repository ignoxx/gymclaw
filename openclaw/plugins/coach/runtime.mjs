import { execFile } from "node:child_process";
import { createTurnLog } from "../turn-log/log.mjs";

// Cheap pre-check so ordinary chat never pays for a Python start-up. Reps alone ("13 reps") only log
// on a bodyweight exercise; otherwise Python returns unhandled and the agent gets the message.
export const SET_LIKE = /^\s*\d+(?:[.,]\d+)?\s*(?:kg|reps?)?\s*[x×*]\s*\d+(?:[.,]\d+)?\s*(?:kg|reps?)?\s*$|^\s*\d+\s*reps?\s*(?:at|@)\s*\d+(?:[.,]\d+)?\s*(?:kg)?\s*$|^\s*\d+\s*(?:reps?|[x×*])?\s*$/i;
export const TICK_MS = 5000;
const ACTIONS = ["status", "preview", "start", "log", "card", "swap", "switch", "relabel", "rest", "later", "next", "end"];
// Arguments an action can't do without. Checked before the CLI so the model gets an example to copy.
const REQUIRED = {
  log: ["text", '{"action":"log","text":"10x40"}'],
  switch: ["exercise", '{"action":"switch","exercise":"pec-deck"}'],
  relabel: ["exercise", '{"action":"relabel","exercise":"pec-deck"}'],
  rest: ["seconds", '{"action":"rest","seconds":90}'],
};

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

// One coach per owner chat for the whole process. OpenClaw may call a plugin's register() more than
// once (gateway channel handlers, agent tools); separate coaches each think they own the live card,
// and a countdown the other one deleted would still fire and send a duplicate set card.
const COACHES = (globalThis[Symbol.for("gymclaw-coach.coaches")] ??= new Map());
// Telegram's answer when the message we edit was deleted (e.g. by another handler or the owner).
const GONE = /message to edit not found|message can't be edited|message_id_invalid/i;

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

  function stop(entry) {
    if (entry.timer) timers.clearTimeout(entry.timer);
    entry.timer = null;
    entry.countdown = null;
  }

  async function remove(entry) {
    stop(entry);
    await quietly((await telegram()).remove(chatId, entry.messageId));
  }

  // Photo cards are edited as captions; a text edit on them fails first and costs a round trip.
  async function edit(entry, text, buttons) {
    return (await telegram()).edit(chatId, entry.messageId, text, buttons, { caption: Boolean(entry.card.photo) });
  }

  async function retire(entry, ack) {
    if (throwaway(entry)) return remove(entry);
    stop(entry);
    await quietly(edit(entry, entry.card.text + (ack ? `\n${ack}` : ""), []));
  }

  async function clearExtras() {
    for (const entry of extras) await remove(entry);
    extras = [];
  }

  // Edits the countdown every TICK_MS and fires exactly at rest_until (not on the next tick), so the
  // next card isn't up to a tick late. Restart it when rest_until changes.
  function startCountdown(entry) {
    stop(entry);
    // Identifies this countdown: a step whose edit was in flight during a restart or stop ends there.
    const countdown = (entry.countdown = {});
    const running = () => entry.countdown === countdown && live === entry;
    // The last step lands just after rest_until (ms here, µs in Python), so Python sees the rest as due.
    const schedule = () => {
      entry.timer = timers.setTimeout(step, Math.max(0, Math.min(TICK_MS, Date.parse(entry.card.rest_until) - now() + 50)));
    };
    const step = async () => {
      entry.timer = null;
      if (Date.parse(entry.card.rest_until) > now()) {
        try {
          await edit(entry, withRest(entry.card, now()), keyboard(entry.card.buttons));
        } catch (error) {
          if (GONE.test(String(error?.message ?? error))) {
            // The countdown is gone, so is its job: stop, and never announce the rest it was showing.
            if (live === entry) live = null;
            return;
          }
          log.debug?.(`gymclaw-coach edit skipped: ${error?.message ?? error}`);
        }
        if (running()) schedule();
        return;
      }
      // A fresh set card (not an edit) so the phone notifies; the countdown message is deleted.
      const result = running() && onRestOver ? await onRestOver().catch(() => null) : null;
      if (result?.cards?.length) await apply(result);
    };
    schedule();
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
      if (live) await quietly(edit(live, withRest(live.card, now()), keyboard(result.live_buttons)));
      await clearExtras();
      for (const card of result.cards ?? []) extras.push({ messageId: await api.send(chatId, card.text, { photo: card.photo, buttons: keyboard(card.buttons) }), card });
      return;
    }
    if (result.refresh && live && result.cards?.length) {
      // Same message, new content (rest shortened): the running countdown picks up the new time.
      live.card = result.cards[0];
      await quietly(edit(live, withRest(live.card, now()), keyboard(live.card.buttons)));
      if (live.card.rest_until) startCountdown(live);
      return;
    }
    if (result.keep && !result.cards?.length) return;
    if (result.restore) {
      await clearExtras();
      if (live) await quietly(edit(live, withRest(live.card, now()), keyboard(live.card.buttons)));
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

/**
 * Wire hooks, the callback namespace and the agent tool. `deps` lets tests inject fakes.
 * Taps and typed sets are logged as `fast` turn-log events (see the turn-log plugin), since they
 * never reach the agent hooks.
 */
export function registerCoach(api, { run, telegram, coaches = COACHES, turnLog = createTurnLog(process.env.GYMCLAW_TURN_LOG_DIR), now = Date.now } = {}) {
  const config = api.pluginConfig ?? {};
  const ownerId = String(config.ownerId ?? "");
  if (!/^[1-9]\d{0,19}$/.test(ownerId) || !(run || config.tool)) {
    api.logger.warn("gymclaw-coach disabled: set plugins.entries.gymclaw-coach.config.ownerId and tool");
    return;
  }
  const cli = run ?? cliRunner(config.tool);
  // Owner DM: chat ID equals the owner's Telegram user ID.
  const sync = createSyncer(cli, ownerId, api.logger);
  if (!coaches.has(ownerId)) {
    coaches.set(ownerId, createCoach({
      telegram,
      chatId: ownerId,
      log: api.logger,
      // Closing the rest job here keeps the cron fallback ping silent; sync then removes it.
      onRestOver: async () => {
        const result = await cli(["coach", "rest-over"]);
        if (result.rest_over) sync();
        return result;
      },
    }));
  }
  const coach = coaches.get(ownerId);
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
      const started = now();
      const entry = { ev: "fast", flow: "tap", action: String(ctx.callback.payload).split(":")[0], msg: ctx.callback.messageId };
      try {
        const result = await cli(["coach", "tap", "--data", ctx.callback.payload, "--request-id", `tg-cb:${ctx.callbackId}`]);
        entry.cliMs = now() - started;
        await coach.apply(result, { tapped: { messageId: ctx.callback.messageId, text: ctx.callback.messageText } });
        if (changed(result)) sync();
      } catch (error) {
        entry.error = error.code ?? "ERROR";
        await report(error);
      }
      turnLog({ ...entry, ok: !entry.error, ms: now() - started });
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
    const started = now();
    // handled: false means the agent gets the message after all; cliMs is what the pre-check cost it.
    const entry = { ev: "fast", flow: "text", text, msg: messageId };
    let result;
    try {
      result = await cli(["coach", "text", "--text", text, "--request-id", `tg-msg:${messageId ?? `${event.timestamp ?? Date.now()}:${text}`}`]);
    } catch (error) {
      // Let the agent explain anything unexpected (e.g. no workout in a loggable state).
      api.logger.warn(`gymclaw-coach: typed set not handled: ${error.code ?? error.message}`);
      turnLog({ ...entry, handled: false, error: error.code ?? "ERROR", ms: now() - started });
      return;
    }
    entry.cliMs = now() - started;
    if (!result.handled) {
      turnLog({ ...entry, handled: false, ms: entry.cliMs });
      return;
    }
    await coach.apply(result, { inboundMessageId: messageId });
    sync();
    turnLog({ ...entry, handled: true, ok: true, ms: now() - started });
    return { handled: true };
  });

  api.registerTool({
    name: "gymclaw_workout",
    description:
      "Drive the live workout in Telegram. Sends exercise cards (image, target, quick buttons) to the owner directly, " +
      "so after calling reply NO_REPLY unless the owner asked a question. Actions: status (read the running workout, sends " +
      "nothing), preview (next or given planned session: one image with every exercise + plan; use for \"what's on today\"), " +
      "log (text like \"10x40\" = 10 reps at 40 kg: logs a set), start (no args: starts today's planned " +
      "session; template_id/planned_session_id only to start something else), card (resend current card), swap (show up to 3 same-muscle alternatives as buttons; only " +
      "before the first set), switch (exercise: the owner is doing this exercise now — any exercise, any time; " +
      "finishes or replaces the current one), relabel (exercise: what the owner really did; which: the logged exercise " +
      "to fix, default the latest; works after the workout too), rest (seconds between sets for this workout; " +
      "remember=true also makes it the default), later (do current exercise later), next (move on to the next " +
      "exercise), end (end early, save, show summary). exercise takes a guide_id or exact name; on EXERCISE_UNKNOWN " +
      "pick one of the suggested guide_ids. The result's `card` is what the owner now sees.",
    parameters: {
      type: "object",
      additionalProperties: false,
      required: ["action"],
      properties: {
        action: { type: "string", enum: ACTIONS },
        template_id: { type: "string" },
        text: { type: "string", description: "Set for action log, reps first, e.g. 10x40" },
        planned_session_id: { type: "string" },
        exercise: { type: "string", description: "switch/relabel: guide_id or exact exercise name" },
        which: { type: "string", description: "relabel: the logged exercise to correct" },
        seconds: { type: "integer", description: "rest: seconds between sets" },
        remember: { type: "boolean", description: "rest: also the default for future workouts" },
      },
    },
    async execute(toolCallId, params) {
      const reply = (body) => ({ content: [{ type: "text", text: JSON.stringify(body) }] });
      const [field, example] = REQUIRED[params.action] ?? [];
      if (field && (params[field] === undefined || params[field] === "")) {
        return reply({ ok: false, code: "ARGUMENT_REQUIRED", message: `${params.action} needs ${field}, e.g. ${example}. Don't call it again without it; if you don't know the value, ask the owner.` });
      }
      const requestId = `tool:${toolCallId}`;
      const args =
        params.action === "status"
          ? ["workout", "current"]
          : params.action === "preview"
            ? ["coach", "preview", ...(params.planned_session_id ? ["--planned-session-id", params.planned_session_id] : [])]
            : params.action === "log"
            ? ["coach", "text", "--text", params.text ?? "", "--request-id", requestId]
            : params.action === "start"
          ? ["coach", "start", ...(params.template_id ? ["--template-id", params.template_id] : []), ...(params.planned_session_id ? ["--planned-session-id", params.planned_session_id] : []), "--request-id", requestId]
          : params.action === "card"
            ? ["coach", "card"]
            : ["coach", "act", "--action", params.action, "--request-id", requestId,
                ...(params.exercise ? ["--exercise", params.exercise] : []),
                ...(params.which ? ["--which", params.which] : []),
                ...(params.seconds ? ["--seconds", String(params.seconds)] : []),
                ...(params.remember ? ["--remember"] : [])];
      try {
        const result = await cli(args);
        if (params.action === "status") return reply({ ok: true, workout: result });
        if (result.handled === false) {
          // "log" text that isn't a set, or no workout in a loggable state: nothing happened.
          return reply({ ok: false, code: "NOT_LOGGED", message: "Nothing logged: no running set, or the text isn't reps × weight (e.g. 10x40; bodyweight: 13 reps). Check status, then ask the owner." });
        }
        await coach.apply(result);
        if (changed(result)) sync();
        // Exactly what the owner sees now, so replies describe the real card, not a guess.
        const shown = (result.cards ?? []).map((card) => card.text);
        return reply({ ok: true, ack: result.ack ?? null, ...(result.live_buttons ? { swap_options: shown } : { card: shown[0] ?? null }) });
      } catch (error) {
        return reply({ ok: false, code: error.code, message: error.message });
      }
    },
  });
}
