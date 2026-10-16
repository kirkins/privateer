// Quiet compaction decisions on a fake clock and Pi context. No model or network.
import assert from "node:assert/strict";
import { INSTRUCTIONS, QuietCompaction } from "../plugins/attended/quiet.mjs";

const MIN = 60_000;
function fixture({ percent = 55, owned = true, afterMs = 10 * MIN } = {}) {
  const t = { now: 0, tick: undefined, compactions: [], sent: [], notes: [], idle: true, percent, owned, draft: "" };
  const ctx = {
    hasUI: true,
    isIdle: () => t.idle,
    getContextUsage: () => ({ tokens: 1000, contextWindow: 2000, percent: t.percent }),
    compact: options => t.compactions.push(options),
    ui: { getEditorText: () => t.draft, notify: (text, level) => t.notes.push({ text, level }) },
  };
  const api = { sendMessage: (message, options) => t.sent.push({ message, options }) };
  const calls = [];
  const call = async (command, args) => {
    calls.push(command);
    if (command === "ship.bearings") return { text: "Bearings for infra: 1 unacknowledged messages.\nOpen tasks:\n- task repair [running]: Fix the cache\n" };
    return { notes: [{ body: "Captain asked whether the cache fix can land today." }] };
  };
  const quiet = new QuietCompaction(api, { ship: "infra", afterMs, minPercent: 40 }, async () => t.owned, call, {
    now: () => t.now, every: fn => { t.tick = fn; return {}; }, cancel: () => {},
  });
  quiet.start(ctx);
  t.wait = async minutes => { t.now += minutes * MIN; await t.tick(); };
  return { t, quiet, calls };
}

// A quiet, full, owned session compacts once, then attaches the records.
{
  const { t, quiet, calls } = fixture();
  await t.wait(9);
  assert.equal(t.compactions.length, 0, "not quiet long enough");
  await t.wait(2);
  assert.equal(t.compactions.length, 1);
  assert.equal(t.compactions[0].customInstructions, INSTRUCTIONS);
  assert.match(INSTRUCTIONS, /verbatim every question or request from the captain/);
  t.compactions[0].onComplete();
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(calls, ["ship.bearings", "note.get"]);
  assert.equal(t.sent.length, 1);
  assert.equal(t.sent[0].options.triggerTurn, false);
  assert.match(t.sent[0].message.content, /Open tasks:\n- task repair/);
  assert.match(t.sent[0].message.content, /cache fix can land today/);
  // Nothing new happened, so it does not compact again however long it waits.
  await t.wait(60);
  assert.equal(t.compactions.length, 1);
  // New work, then quiet again: it may compact again.
  quiet.began(); t.idle = false; await t.wait(30);
  quiet.settled(); t.idle = true; await t.wait(11);
  assert.equal(t.compactions.length, 2);
}

// Each reason to leave the session alone.
for (const [why, setup] of [
  ["small context", t => { t.percent = 20; }],
  ["unknown size", t => { t.percent = null; }],
  ["draft in editor", t => { t.draft = "Also, can you check"; }],
  ["another session holds the ship", t => { t.owned = false; }],
  ["working", (t, quiet) => { quiet.began(); t.idle = false; }],
]) {
  const { t, quiet } = fixture();
  setup(t, quiet);
  await t.wait(30);
  assert.equal(t.compactions.length, 0, why);
}

// A failed compaction is reported and waits for new activity.
{
  const { t } = fixture();
  await t.wait(11);
  t.compactions[0].onError(new Error("provider unavailable"));
  assert.equal(t.notes[0].level, "warning");
  assert.match(t.notes[0].text, /provider unavailable/);
  await t.wait(30);
  assert.equal(t.compactions.length, 1);
}

// Zero seconds turns it off.
{
  const { t } = fixture({ afterMs: 0 });
  assert.equal(t.tick, undefined);
}
