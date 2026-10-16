// Presentation policy only. No prompt, context or session data is rewritten.
import { readFileSync, mkdirSync, openSync, writeFileSync, closeSync, renameSync, unlinkSync } from "node:fs";
import { join } from "node:path";
import { randomUUID } from "node:crypto";

export const OWNED_TOOLS = new Set(["read", "bash", "edit", "write", "grep", "find", "ls"]);

// Privateer's authoritative producer is AttendedMail.message(), a custom
// message, not a marker embedded in human input. Never classify user text.
export function operational(message, home) {
  const d = message?.details;
  return message?.role === "custom" && message.customType === "privateer-mail"
    && typeof message.content === "string"
    && d?.home === home && typeof d.ship === "string" && d.ship.length > 0
    && Number.isSafeInteger(d.throughSeq) && d.throughSeq >= 0
    && Number.isSafeInteger(d.count) && d.count > 0;
}

export function assistantView(message, streaming, collapsed, overrides = new Map()) {
  const working = !streaming && message.stopReason !== "pending"
    && (message.stopReason === "toolUse" || (message.stopReason === "length"
      && message.content.some(c => c.type === "toolCall")));
  const content = [], thinkingRuns = [];
  let run = 0;
  for (let i = 0; i < message.content.length; i++) {
    const block = message.content[i];
    if (block.type === "thinking") {
      const blocks = [block];
      while (message.content[i + 1]?.type === "thinking") blocks.push(message.content[++i]);
      if (!blocks.some(b => b.thinking.trim())) { content.push(...blocks); continue; }
      const index = run++;
      if (!(overrides.get(index) ?? collapsed)) {
        thinkingRuns.push(index);
        content.push(...blocks);
      }
    } else if (!(working && block.type === "text" && !/[\r\n]/u.test(block.text)
        && block.text.trim().length < 240)) content.push(block);
  }
  return { message: { ...message, content }, thinkingRuns };
}

export function readPreference(home) {
  try { return readFileSync(join(home, "config", "calm"), "utf8") === "on\n"; }
  catch (error) { if (error.code === "ENOENT") return false; throw error; }
}

export function writePreference(home, enabled) {
  const directory = join(home, "config");
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const temporary = join(directory, `.calm-${randomUUID()}`);
  let fd;
  try {
    fd = openSync(temporary, "wx", 0o600);
    writeFileSync(fd, enabled ? "on\n" : "off\n");
    closeSync(fd); fd = undefined;
    renameSync(temporary, join(directory, "calm"));
  } finally {
    if (fd !== undefined) closeSync(fd);
    try { unlinkSync(temporary); } catch (error) { if (error.code !== "ENOENT") throw error; }
  }
}
