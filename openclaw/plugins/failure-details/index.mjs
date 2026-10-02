import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import { registerFailureDetails } from "./runtime.mjs";

export default definePluginEntry({
  id: "gymclaw-failure-details",
  name: "GymClaw failure details",
  register: registerFailureDetails,
});
