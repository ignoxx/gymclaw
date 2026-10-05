import assert from "node:assert/strict";
import test from "node:test";
import { SET_LIKE, createCoach, createSyncer, registerCoach, withRest } from "./runtime.mjs";

const OWNER = "123456789";
const card = (text, extra = {}) => ({ text, photo: null, buttons: [[{ text: "✅ 10 × 40 kg", data: "gc:log:abcd1234:40:10" }]], rest_until: null, ...extra });

function fakeTelegram() {
  const calls = [];
  let next = 100;
  return {
    calls,
    api: {
      send: async (chat, text, opts) => (calls.push(["send", chat, text, opts]), next++),
      edit: async (chat, id, text, buttons) => calls.push(["edit", chat, id, text, buttons]),
      react: async (chat, id, emoji) => calls.push(["react", chat, id, emoji]),
      remove: async (chat, id) => calls.push(["remove", chat, id]),
    },
  };
}

function fakeTimers() {
  const intervals = new Map();
  let id = 0;
  return {
    intervals,
    setInterval: (fn) => (intervals.set(++id, fn), id),
    clearInterval: (key) => intervals.delete(key),
    tick: async () => { for (const fn of [...intervals.values()]) await fn(); },
  };
}

function plugin(run, { coaches = new Map(), telegram = fakeTelegram() } = {}) {
  const { calls, api } = telegram;
  const registered = { hooks: new Map() };
  registerCoach(
    {
      pluginConfig: { ownerId: OWNER, tool: "/unused" },
      logger: { warn: () => {}, debug: () => {} },
      registerInteractiveHandler: (r) => (registered.callback = r),
      registerTool: (t) => (registered.tool = t),
      on: (name, fn) => registered.hooks.set(name, fn),
    },
    { run, telegram: async () => api, coaches },
  );
  return { calls, registered };
}

test("set pre-check only matches whole set messages", () => {
  for (const text of ["80x9", "12x40kg", "90kgx10 reps", "9 reps at 80", "22,5 × 8"]) assert.match(text, SET_LIKE);
  for (const text of ["how long do I rest?", "it's occupied", "80x9 then 80x10", "x"]) assert.doesNotMatch(text, SET_LIKE);
});

test("rest countdown text", () => {
  const due = Date.parse("2026-10-02T18:00:00Z");
  const rest = card("until **Bench** set 2/2", { rest_until: "2026-10-02T18:00:00Z", kind: "rest" });
  assert.equal(withRest(rest, due - 85_000), "⏱ 1:25 until **Bench** set 2/2");
  assert.equal(withRest(rest, due + 1), "⏱ 0:00 until **Bench** set 2/2");
});

test("typed set: react, ack on the set card, countdown, then countdown deleted and fresh card sent", async () => {
  const { calls, api } = fakeTelegram();
  const timers = fakeTimers();
  let now = Date.parse("2026-10-02T18:00:00Z");
  const onRestOver = async () => ({ cleanup: "delete", cards: [card("Bench · set 2", { kind: "set" })] });
  const coach = createCoach({ telegram: async () => api, chatId: OWNER, now: () => now, timers, onRestOver });
  await coach.apply({ cards: [card("Bench · set 1", { kind: "set" })] });
  const rest = card("until Bench set 2/2", { rest_until: "2026-10-02T18:01:30Z", kind: "rest", buttons: [[{ text: "⏭ Skip", data: "gc:skip:abcd1234" }]] });
  await coach.apply({ react: "👍", ack: "✅ 10 × 40 kg", cards: [rest] }, { inboundMessageId: 7 });
  assert.deepEqual(calls[1], ["react", OWNER, 7, "👍"]);
  assert.deepEqual(calls[2], ["edit", OWNER, 100, "Bench · set 1\n✅ 10 × 40 kg", []]);
  assert.equal(calls[3][2], "⏱ 1:30 until Bench set 2/2");
  now += 5000;
  await timers.tick();
  assert.equal(calls.at(-1)[3], "⏱ 1:25 until Bench set 2/2");
  now += 90_000;
  await timers.tick();
  assert.deepEqual(calls.at(-2), ["remove", OWNER, 101]);
  assert.equal(calls.at(-1)[0], "send");
  assert.equal(coach.state().live.card.text, "Bench · set 2");
  assert.equal(timers.intervals.size, 0);
});

test("shortened rest edits the same countdown and keeps counting to the new time", async () => {
  const { calls, api } = fakeTelegram();
  const timers = fakeTimers();
  let now = Date.parse("2026-10-02T18:00:00Z");
  const coach = createCoach({ telegram: async () => api, chatId: OWNER, now: () => now, timers });
  await coach.apply({ cards: [card("until Bench set 2/2", { rest_until: "2026-10-02T18:01:30Z", kind: "rest" })] });
  await coach.apply({ refresh: true, cards: [card("until Bench set 2/2", { rest_until: "2026-10-02T18:01:00Z", kind: "rest" })] });
  assert.deepEqual(calls.at(-1).slice(0, 4), ["edit", OWNER, 100, "⏱ 1:00 until Bench set 2/2"]);
  now += 5000;
  await timers.tick();
  assert.equal(calls.at(-1)[3], "⏱ 0:55 until Bench set 2/2");
  assert.equal(calls.filter((c) => c[0] === "send").length, 1, "no new message, no extra notification");
  // A settings change with nothing to show leaves the live card alone.
  await coach.apply({ keep: true, cards: [] });
  assert.equal(coach.state().live.messageId, 100);
});

test("swap: menu on the card, wait restores it, choosing deletes options and the old card", async () => {
  const { calls, api } = fakeTelegram();
  const coach = createCoach({ telegram: async () => api, chatId: OWNER, timers: fakeTimers() });
  await coach.apply({ cards: [card("Cable fly")] });
  const menu = [[{ text: "⏳ I'll wait", data: "gc:wait:abcd1234" }]];
  const options = [card("Option A", { kind: "option" }), card("Option B", { kind: "option" })];
  await coach.apply({ keep: true, live_buttons: menu, cards: options }, { tapped: { messageId: 100 } });
  assert.deepEqual(calls[1], ["edit", OWNER, 100, "Cable fly", [[{ text: "⏳ I'll wait", callback_data: "gc:wait:abcd1234" }]]]);
  assert.equal(coach.state().extras.length, 2);
  await coach.apply({ restore: true, cards: [] }, { tapped: { messageId: 100 } });
  assert.deepEqual(calls.slice(-3).map((c) => c[0]), ["remove", "remove", "edit"]);
  assert.equal(calls.at(-1)[4].length, 1, "original buttons are back");
  await coach.apply({ keep: true, live_buttons: menu, cards: options }, { tapped: { messageId: 100 } });
  await coach.apply({ cleanup: "delete", cards: [card("Option A · set 1")] }, { tapped: { messageId: 104 } });
  const removed = calls.filter((c) => c[0] === "remove").map((c) => c[2]);
  assert.deepEqual(removed.slice(-3), [100, 103, 104]);
  assert.equal(coach.state().live.card.text, "Option A · set 1");
  assert.equal(coach.state().extras.length, 0);
});

test("standalone preview is sent without touching the live card", async () => {
  const { calls, api } = fakeTelegram();
  const coach = createCoach({ telegram: async () => api, chatId: OWNER, timers: fakeTimers() });
  await coach.apply({ cards: [card("Bench · set 1")] });
  await coach.apply({ standalone: true, cards: [card("Pull · Mon 10:30", { kind: "preview", buttons: [] })] });
  assert.deepEqual(calls.map((c) => c[0]), ["send", "send"]);
  assert.equal(coach.state().live.card.text, "Bench · set 1");
});

test("hooks ignore non-owners and non-set chat; owner sets and taps go to the CLI", async () => {
  const seen = [];
  const { calls, registered } = plugin(async (args) => (seen.push(args), { handled: true, react: "👍", cards: [card("Next")] }));
  registered.hooks.get("message_received")({ content: "12x40kg", messageId: "77" });
  const claim = registered.hooks.get("before_dispatch");
  assert.equal(await claim({ channel: "telegram", isGroup: false, senderId: "1", content: "9x80", timestamp: 5 }), undefined);
  assert.equal(await claim({ channel: "telegram", isGroup: false, senderId: OWNER, content: "it's occupied", timestamp: 6 }), undefined);
  assert.deepEqual(await claim({ channel: "telegram", isGroup: false, senderId: `telegram:${OWNER}`, content: "12x40kg", timestamp: 7 }), { handled: true });
  assert.deepEqual(seen[0], ["coach", "text", "--text", "12x40kg", "--request-id", "tg-msg:77"]);
  const ctx = { auth: { isAuthorizedSender: true }, isGroup: false, senderId: OWNER, callbackId: "cb1", callback: { payload: "next:abcd1234", messageId: 100 } };
  assert.deepEqual(await registered.callback.handler(ctx), { handled: true });
  assert.ok(seen.some((args) => args[0] === "runtime" && args[1] === "sync"), "rest timers are synced after a logged set");
  assert.ok(seen.some((args) => args.join(" ") === "coach tap --data next:abcd1234 --request-id tg-cb:cb1"));
  assert.ok(calls.some((c) => c[0] === "react" && c[2] === "77"), "typed set gets its 👍");
});

test("unhandled CLI result falls through to the agent", async () => {
  const { registered } = plugin(async () => ({ handled: false }));
  assert.equal(await registered.hooks.get("before_dispatch")({ channel: "telegram", isGroup: false, senderId: OWNER, content: "9x80", timestamp: 8 }), undefined);
});

test("overlapping syncs collapse into one follow-up run", async () => {
  let calls = 0;
  let release;
  const sync = createSyncer(() => (calls++, new Promise((resolve) => (release = resolve))), OWNER, { warn: () => {} });
  const first = sync();
  sync();
  sync();
  release();
  await new Promise((r) => setImmediate(r));
  release();
  await first;
  assert.equal(calls, 2);
});

test("two registrations share one coach: a skip tap retires the countdown the agent tool sent", async () => {
  const coaches = new Map();
  const telegram = fakeTelegram();
  const rest = card("until Leg press set 2/2", { rest_until: "2099-01-01T00:00:00Z", kind: "rest" });
  const run = async (args) => (args[1] === "act" ? { handled: true, cards: [rest] } : args[1] === "tap" ? { handled: true, cleanup: "delete", cards: [card("Leg press · set 2")] } : {});
  const agent = plugin(run, { coaches, telegram });
  const gateway = plugin(run, { coaches, telegram });
  await agent.registered.tool.execute("t1", { action: "next" });
  const ctx = { auth: { isAuthorizedSender: true }, isGroup: false, senderId: OWNER, callbackId: "cb1", callback: { payload: "skip:abcd1234", messageId: 100 } };
  await gateway.registered.callback.handler(ctx);
  assert.equal(coaches.size, 1);
  assert.equal(coaches.get(OWNER).state().live.card.text, "Leg press · set 2");
});

test("a countdown whose message is gone stops instead of sending a second set card", async () => {
  const { calls, api } = fakeTelegram();
  const timers = fakeTimers();
  let now = Date.parse("2026-10-05T11:06:00Z");
  let restOvers = 0;
  api.edit = async () => { throw new Error("Call to 'editMessageText' failed! (400: Bad Request: message to edit not found)"); };
  const coach = createCoach({ telegram: async () => api, chatId: OWNER, now: () => now, timers, onRestOver: async () => (restOvers++, { cards: [card("Leg extension · set 1")] }) });
  await coach.apply({ cards: [card("until Leg press set 2/2", { rest_until: "2026-10-05T11:08:16Z", kind: "rest" })] });
  await timers.tick();
  assert.equal(timers.intervals.size, 0);
  now += 180_000;
  await timers.tick();
  assert.equal(restOvers, 0);
  assert.equal(calls.filter((c) => c[0] === "send").length, 1);
});
