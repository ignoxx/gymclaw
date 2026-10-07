import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import { registerTurnLog } from "./runtime.mjs";

export default definePluginEntry({
  id: "gymclaw-turn-log",
  name: "GymClaw turn log",
  register: registerTurnLog,
});
