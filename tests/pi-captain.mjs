// The captain's slash commands drive the real Privateer CLI. No model or network.
import assert from "node:assert/strict";
import { registerCaptain } from "../plugins/attended/captain.mjs";

const commands = new Map();
const api = { registerCommand: (name, spec) => commands.set(name, spec) };
// Only an attended Pi first mate's session carries them.
assert.deepEqual(registerCaptain(api, { ...process.env, PRIVATEER_ROLE: "operator" }), []);
assert.deepEqual(registerCaptain(api, { ...process.env, PRIVATEER_ATTENDED_HARNESS: "codex" }), []);
assert.equal(commands.size, 0);
assert.deepEqual(registerCaptain(api, process.env).sort(), ["afk", "back", "ships", "stow", "yolo"]);

const run = async (name, args) => {
  const seen = [];
  await commands.get(name).handler(args, { ui: { notify: (text, level) => seen.push({ text, level }) } });
  assert.equal(seen.length, 1);
  return seen[0];
};
// The session runs as the ship's agent, which may not stow the fleet; the
// command runs as the captain at the keyboard.
let said = await run("stow", "status");
assert.equal(said.level, "info", said.text);
assert.match(said.text, /never been stowed/);
said = await run("stow", "sideways");
assert.equal(said.level, "warning");
assert.match(said.text, /Usage/);
said = await run("stow", "");
assert.match(said.text, /Stowing the fleet/);
assert.match(said.text, /asked fixture to stow/);
said = await run("stow", " status ");
assert.match(said.text, /\(held\)/);
said = await run("stow", "resume");
assert.match(said.text, /Resumed the fleet/);
said = await run("ships", "");
assert.match(said.text, /fixture/);
said = await run("yolo", "status");
assert.equal(said.level, "info", said.text);
