import { dirname } from "node:path";

const NEEDED = ["sendMessageTelegram", "editMessageTelegram", "reactMessageTelegram"];

/**
 * Telegram send/edit/react from the installed OpenClaw Telegram channel (same module instance the
 * Gateway already loaded, so token, proxy and retries come from its config). These helpers are not
 * on a public SDK path: loaded by file URL next to the plugin SDK, and checked so an upgrade that
 * moves them fails loudly instead of silently dropping messages. Reviewed against OpenClaw 2026.7.1.
 */
export async function loadTelegram(getConfig) {
  const sdkEntry = import.meta.resolve("openclaw/plugin-sdk/plugin-entry");
  const mod = await import(new URL("../extensions/telegram/runtime-api.js", sdkEntry).href);
  const missing = NEEDED.filter((name) => typeof mod[name] !== "function");
  if (missing.length) throw new Error(`OpenClaw Telegram runtime lacks ${missing.join(", ")}; review gymclaw-coach for this version`);
  return {
    async send(chatId, text, { photo, buttons }) {
      const result = await mod.sendMessageTelegram(String(chatId), text, {
        cfg: getConfig(),
        textMode: "markdown",
        buttons,
        ...(photo ? { mediaUrl: photo, mediaLocalRoots: [dirname(dirname(photo))] } : {}),
      });
      return Number(result.messageId);
    },
    edit(chatId, messageId, text, buttons) {
      return mod.editMessageTelegram(String(chatId), messageId, text, { cfg: getConfig(), textMode: "markdown", buttons, editMode: "auto" });
    },
    react(chatId, messageId, emoji) {
      return mod.reactMessageTelegram(String(chatId), messageId, emoji, { cfg: getConfig() });
    },
  };
}
