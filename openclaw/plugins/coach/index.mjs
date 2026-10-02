import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import { registerCoach } from "./runtime.mjs";
import { loadTelegram } from "./telegram.mjs";

export default definePluginEntry({
  id: "gymclaw-coach",
  name: "GymClaw coach",
  register(api) {
    let telegram;
    registerCoach(api, { telegram: () => (telegram ??= loadTelegram(() => api.config)) });
  },
});
