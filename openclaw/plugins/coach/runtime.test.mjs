import assert from "node:assert/strict";
import test from "node:test";
import { SET_LIKE, createCoach, createSyncer, registerCoach, withRest } from "./runtime.mjs";

const OWNER = "123456789";
const card = (text, extra = {}) => ({ text, photo: null, buttons: [[{ text: "✅ 40 kg × 10", data: "gc:log:abcd1234:40:10" }]], rest_until: null, ...extra });

function fakeTelegram() {
  const calls = [];
  let next = 100;
  return {
    calls,
    api: {
      send: async (chat, text, opts) => (calls.push(["send", chat, text, opts]), next++),
      edit: async (chat, id, text, buttons) => calls.push(["edit", chat, id, text, buttons]),
      react: async (chat, id, emoji) => calls.push(["react", chat, id, emoji]),
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

function plugin(run) {
  const { calls, api } = fakeTelegram();
  const registered = { hooks: new Map() };
  registerCoach(
    {
      pluginConfig: { ownerId: OWNER, tool: "/unused" },
      logger: { warn: () => {}, debug: () => {} },
      registerInteractiveHandler: (r) => (registered.callback = r),
      registerTool: (t) => (registered.tool = t),
      on: (name, fn) => registered.hooks.set(name, fn),
    },
    { run, telegram: async () => api },
  );
  return { calls, registered };
}

test("set pre-check only matches whole set messages", () => {
  for (const text of ["80x9", "12x40kg", "90kgx10 reps", "9 reps at 80", "22,5 × 8"]) assert.match(text, SET_LIKE);
  for (const text of ["how long do I rest?", "it's occupied", "80x9 then 80x10", "x"]) assert.doesNotMatch(text, SET_LIKE);
});

test("rest line counts down and ends", () => {
  const due = Date.parse("2026-10-02T18:00:00Z");
  assert.equal(withRest(card("Bench", { rest_until: "2026-10-02T18:00:00Z" }), due - 85_000), "Bench\n⏱ Rest 1:25");
  assert.equal(withRest(card("Bench", { rest_until: "2026-10-02T18:00:00Z" }), due + 1), "Bench\n⏱ Rest over");
});

test("typed set: react, retire old card buttons, send next card with live countdown", async () => {
  const { calls, api } = fakeTelegram();
  const timers = fakeTimers();
  let now = Date.parse("2026-10-02T18:00:00Z");
  const coach = createCoach({ telegram: async () => api, chatId: OWNER, now: () => now, timers });
  await coach.apply({ cards: [card("Bench · set 1")] });
  await coach.apply({ react: "👍", cards: [card("Bench · set 2", { rest_until: "2026-10-02T18:01:30Z" })] }, { inboundMessageId: 7 });
  assert.deepEqual(calls[1], ["react", OWNER, 7, "👍"]);
  assert.deepEqual(calls[2], ["edit", OWNER, 100, "Bench · set 1", []]);
  assert.equal(calls[3][2], "Bench · set 2\n⏱ Rest 1:30");
  now += 5000;
  await timers.tick();
  assert.equal(calls.at(-1)[3], "Bench · set 2\n⏱ Rest 1:25");
  assert.equal(calls.at(-1)[4].length, 1, "countdown edits keep the buttons");
  now += 90_000;
  await timers.tick();
  assert.equal(calls.at(-1)[3], "Bench · set 2\n⏱ Rest over");
  assert.equal(timers.intervals.size, 0);
});

test("rest over: old card loses its buttons and a fresh card with buttons is sent", async () => {
  const { calls, api } = fakeTelegram();
  const timers = fakeTimers();
  let now = Date.parse("2026-10-02T18:00:00Z");
  const onRestOver = async () => ({ handled: true, rest_over: true, cards: [card("Bench · set 2")] });
  const coach = createCoach({ telegram: async () => api, chatId: OWNER, now: () => now, timers, onRestOver });
  await coach.apply({ cards: [card("Bench · set 2", { rest_until: "2026-10-02T18:01:30Z" })] });
  now += 91_000;
  await timers.tick();
  assert.deepEqual(calls.at(-2), ["edit", OWNER, 100, "Bench · set 2\n⏱ Rest over", []]);
  assert.equal(calls.at(-1)[0], "send");
  assert.equal(calls.at(-1)[3].buttons.length, 1);
  assert.equal(coach.state().live.messageId, 101);
});

test("tap acks the pressed card; swap menus stay until the workout moves on", async () => {
  const { calls, api } = fakeTelegram();
  const coach = createCoach({ telegram: async () => api, chatId: OWNER, timers: fakeTimers() });
  await coach.apply({ cards: [card("Incline")] });
  await coach.apply({ keep: true, cards: [card("Option A"), card("Taken?")] }, { tapped: { messageId: 100 } });
  assert.equal(coach.state().extras.length, 2);
  assert.ok(!calls.some((c) => c[0] === "edit"), "live card keeps its buttons while the menu is open");
  await coach.apply({ ack: "🔄 Swapped", cards: [card("Option A · set 1")] }, { tapped: { messageId: 101 } });
  assert.deepEqual(calls.find((c) => c[0] === "edit" && c[2] === 101), ["edit", OWNER, 101, "Option A\n🔄 Swapped", []]);
  assert.ok(calls.some((c) => c[0] === "edit" && c[2] === 100), "old exercise card loses its buttons");
  assert.equal(coach.state().live.card.text, "Option A · set 1");
});

test("hooks ignore non-owners and non-set chat; owner sets and taps go to the CLI", async () => {
  const seen = [];
  const { calls, registered } = plugin(async (args) => (seen.push(args), { handled: true, react: "👍", cards: [card("Next")] }));
  const claim = registered.hooks.get("inbound_claim");
  assert.equal(await claim({ channel: "telegram", isGroup: false, senderId: "1", body: "80x9", messageId: "5" }), undefined);
  assert.equal(await claim({ channel: "telegram", isGroup: false, senderId: OWNER, body: "it's occupied", messageId: "6" }), undefined);
  assert.deepEqual(await claim({ channel: "telegram", isGroup: false, senderId: OWNER, body: "12x40kg", messageId: "7" }), { handled: true });
  assert.deepEqual(seen[0], ["coach", "text", "--text", "12x40kg", "--request-id", "tg-msg:7"]);
  const ctx = { auth: { isAuthorizedSender: true }, isGroup: false, senderId: OWNER, callbackId: "cb1", callback: { payload: "next:abcd1234", messageId: 100 } };
  assert.deepEqual(await registered.callback.handler(ctx), { handled: true });
  assert.ok(seen.some((args) => args[0] === "runtime" && args[1] === "sync"), "rest timers are synced after a logged set");
  assert.ok(seen.some((args) => args.join(" ") === "coach tap --data next:abcd1234 --request-id tg-cb:cb1"));
  assert.ok(calls.some((c) => c[0] === "react"));
});

test("unhandled CLI result falls through to the agent", async () => {
  const { registered } = plugin(async () => ({ handled: false }));
  assert.equal(await registered.hooks.get("inbound_claim")({ channel: "telegram", isGroup: false, senderId: OWNER, body: "80x9", messageId: "8" }), undefined);
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
