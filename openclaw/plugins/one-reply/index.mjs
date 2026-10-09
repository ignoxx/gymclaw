import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import { registerOneReply } from "./runtime.mjs";

export default definePluginEntry({
  id: "gymclaw-one-reply",
  name: "GymClaw one reply",
  register: (api) => registerOneReply(api),
});
