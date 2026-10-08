import { createHash } from "node:crypto";
import { appendFile, mkdir } from "node:fs/promises";
import { join } from "node:path";

const SECRET_KEY = /token|secret|password|passwd|api.?key|authorization|cookie|credential/i;
const SECRET_VALUE = [
  /\bBearer\s+[\w.~+/=-]+/gi,
  /\bsk-[\w-]{16,}/g, // OpenRouter/OpenAI-style keys
  /\b\d{6,12}:[\w-]{30,}/g, // Telegram bot tokens
];
// Inline media: data URLs and long unbroken base64 runs are stored as size + hash only.
const BLOB = /^data:[^,]*,|^[A-Za-z0-9+/=\r\n]{2000,}$/;
const MAX_PENDING = 1000;

export const sha = (text) => createHash("sha256").update(String(text)).digest("hex").slice(0, 12);

/** Redact secrets and cap length; the cut is marked with how much was dropped. */
export function scrub(text, max = 4000) {
  let value = String(text ?? "");
  for (const pattern of SECRET_VALUE) value = value.replace(pattern, "[redacted]");
  return value.length > max ? `${value.slice(0, max)}…[+${value.length - max}]` : value;
}

/** JSON-safe copy of tool args/results: secret keys redacted, strings scrubbed, blobs → {bytes, sha}. */
export function clean(value, max = 1000, depth = 0) {
  if (typeof value === "string") return BLOB.test(value) ? { bytes: value.length, sha: sha(value) } : scrub(value, max);
  if (value === null || typeof value !== "object") return value;
  if (depth > 4) return "[deep]";
  if (Array.isArray(value)) return value.slice(0, 50).map((item) => clean(item, max, depth + 1));
  return Object.fromEntries(
    Object.entries(value).map(([key, item]) => [key, SECRET_KEY.test(key) ? "[redacted]" : clean(item, max, depth + 1)]),
  );
}

/**
 * Append-only JSONL turn log: one line per event in `<dir>/YYYY-MM-DD.jsonl` (UTC date).
 * Returns `log(event)`, which never throws and never waits: writes are queued, errors dropped,
 * and if the disk stalls past MAX_PENDING lines new events are dropped. No dir = disabled.
 */
export function createTurnLog(dir, { now = Date.now } = {}) {
  if (!dir) return () => {};
  let queue = mkdir(dir, { recursive: true, mode: 0o700 });
  let pending = 0;
  return function log(event) {
    try {
      if (pending >= MAX_PENDING) return;
      const ts = new Date(now()).toISOString();
      const line = `${JSON.stringify({ ts, ...event })}\n`;
      pending++;
      queue = queue
        .catch(() => {})
        .then(() => appendFile(join(dir, `${ts.slice(0, 10)}.jsonl`), line, { mode: 0o600 }))
        .catch(() => {})
        .finally(() => pending--);
    } catch {
      // Logging must never break a turn.
    }
  };
}
