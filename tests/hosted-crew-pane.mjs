// A hosted crew's Pi, as the fake `pi` in hosted.rs runs it in a pane: it
// loads the real crew extension the supervisor passed with `-e` and plays a
// short run through it. No models. Its root holds `crew-hold` to keep the run
// open after its first tool until `crew-release` exists.
//   node hosted-crew-pane.mjs ROOT PI-ARGV...
import { existsSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [root, ...argv] = process.argv.slice(2);
const at = argv.findIndex((arg, i) => argv[i - 1] === "-e" && arg.endsWith("attended/crew.ts"));
if (at < 0) process.exit(0);
const { default: crew } = await import(pathToFileURL(argv[at].replace(/\.ts$/, ".mjs")).href);

const handlers = [];
crew({ on: (type, fn) => handlers.push([type, fn]) });
const ctx = {
  sessionManager: {
    getHeader: () => ({ type: "session", version: 3, id: "0d5a1f0e-7b1c-4b8e-9c55-1f2e3d4c5b6a", cwd: process.cwd() }),
  },
  shutdown: () => process.exit(0),
};
const emit = (type, event = {}) => {
  for (const [name, fn] of handlers) if (name === type) fn({ type, ...event }, ctx);
};
// Shaped like a token, but not one.
const planted = ["ghp", "0123456789abcdefABCDEF0123456789abcd"].join("_");
const say = (text, stopReason) => ({ message: { role: "assistant", stopReason, content: [{ type: "text", text }] } });

emit("session_start");
emit("agent_start");
emit("turn_start");
emit("message_end", say("Fixture crew is working", "toolUse"));
emit("tool_execution_start", { toolCallId: "call_1", toolName: "bash", args: { command: "make", env: { API_TOKEN: planted } } });
emit("tool_execution_end", {
  toolCallId: "call_1", toolName: "bash", isError: false,
  result: { content: [{ type: "text", text: `\u001b[1;32mbuilt\u001b[0m\r\nexport GITHUB_TOKEN=${planted}\n${planted}` }] },
});
emit("turn_end", say("Fixture crew is working", "toolUse"));
if (existsSync(join(root, "crew-hold"))) {
  while (!existsSync(join(root, "crew-release"))) await new Promise(resolve => setTimeout(resolve, 50));
}
emit("turn_start");
emit("message_end", say("Fixture crew finished the work", "stop"));
emit("agent_end", { messages: [], willRetry: false });
emit("agent_settled", { aborted: false });
