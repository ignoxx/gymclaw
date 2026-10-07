import { existsSync, realpathSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const NEEDED = ["sendMessageTelegram", "editMessageTelegram", "reactMessageTelegram", "deleteMessageTelegram"];

/** The Gateway's own OpenClaw install: SDK resolution when the loader supports it, else the running CLI. */
function runtimeApiPath() {
  const candidates = [];
  try {
    candidates.push(fileURLToPath(new URL("../extensions/telegram/runtime-api.js", import.meta.resolve("openclaw/plugin-sdk/plugin-entry"))));
  } catch {
    // Plugin loaders without import.meta.resolve fall through to the CLI path.
  }
  if (process.argv[1]) candidates.push(join(dirname(realpathSync(process.argv[1])), "dist/extensions/telegram/runtime-api.js"));
  const found = candidates.find((path) => existsSync(path));
  if (!found) throw new Error("OpenClaw Telegram runtime not found; review gymclaw-coach for this install");
  return found;
}

/**
 * Telegram send/edit/react from the installed OpenClaw Telegram channel (same module instance the
 * Gateway already loaded, so token, proxy and retries come from its config). These helpers are not
 * on a public SDK path: located via the SDK or the running CLI, and checked so an upgrade that
 * moves them fails loudly instead of silently dropping messages. Reviewed against OpenClaw 2026.7.1.
 */
export async function loadTelegram(getConfig) {
  const mod = await import(runtimeApiPath());
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
    // caption: the message is a photo. Unknown (false) tries a text edit, then the caption.
    edit(chatId, messageId, text, buttons, { caption = false } = {}) {
      return mod.editMessageTelegram(String(chatId), messageId, text, { cfg: getConfig(), textMode: "markdown", buttons, editMode: caption ? "caption" : "auto" });
    },
    remove(chatId, messageId) {
      return mod.deleteMessageTelegram(String(chatId), messageId, { cfg: getConfig() });
    },
    react(chatId, messageId, emoji) {
      return mod.reactMessageTelegram(String(chatId), messageId, emoji, { cfg: getConfig() });
    },
  };
}
