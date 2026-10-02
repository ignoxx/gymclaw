import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import { registerCoach } from "./runtime.mjs";
import { loadTelegram } from "./telegram.mjs";

export default definePluginEntry({
  id: "gymclaw-coach",
  name: "GymClaw coach",
  register(api) {
    let telegram;
    const getTelegram = () => (telegram ??= loadTelegram(() => api.config));
    registerCoach(api, { telegram: getTelegram });
    // Fail at startup, not on the owner's first tap.
    api.on("gateway_start", () =>
      getTelegram().then(
        () => api.logger.info("gymclaw-coach: Telegram runtime ready"),
        (error) => api.logger.error(`gymclaw-coach: ${error.message}`),
      ),
    );
  },
});
