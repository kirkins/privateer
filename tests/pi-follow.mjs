// Follow-through decisions at Pi's before-settle boundary. No model or network.
import assert from "node:assert/strict";
import { FollowThrough } from "../plugins/attended/follow.mjs";
import attended from "../plugins/attended/mail.mjs";

let idle = [{ id: "dashboard", title: "Build the bridge" }, { id: "bridge", title: "Keep one bridge" }];
let owned = true, calls = 0, broken = false;
const follow = new FollowThrough({ ship: "privateer" }, async () => owned, async (command, args) => {
  calls++;
  assert.equal(command, "ship.bearings");
  assert.deepEqual(args, { ship: "privateer" });
  if (broken) throw new Error("runtime unavailable");
  return { idle_tasks: idle };
});
const earlier = { type: "custom", customType: "other", data: {} };
const settle = (outcome = "completed") => follow.beforeSettle({ outcome, entries: [earlier] });

// Announced work with nothing on it gets one more step, keeping other entries.
let result = await settle();
assert.equal(result.continue, true);
assert.equal(result.entries.length, 2);
assert.deepEqual(result.entries[0], earlier);
const nudge = result.entries[1];
assert.equal(nudge.type, "custom_message");
assert.match(nudge.content, /- dashboard: Build the bridge\n- bridge: Keep one bridge/);
assert.match(nudge.content, /"key":"waiting"/);
assert.deepEqual(nudge.details.tasks, ["dashboard", "bridge"]);
// The same tasks, in any order, are not nudged twice: no loop.
idle = [...idle].reverse();
assert.equal(await settle(), undefined);
// A new idle task is a new state.
idle = [...idle, { id: "hosting", title: "Land hosting" }];
assert.equal((await settle()).continue, true);
// Once nothing is idle, a later return of the same set is news again.
idle = [];
assert.equal(await settle(), undefined);
idle = [{ id: "dashboard", title: "Build the bridge" }];
assert.equal((await settle()).continue, true);

// Never after an interrupted or failed run, without the lease, or if the
// runtime cannot answer.
idle = [{ id: "fresh", title: "Fresh work" }];
const before = calls;
assert.equal(await settle("aborted"), undefined);
assert.equal(await settle("error"), undefined);
assert.equal(calls, before);
owned = false;
assert.equal(await settle(), undefined);
owned = true; broken = true;
assert.equal(await settle(), undefined);

// The attended adapter registers the boundary for Pi, not for OMP.
for (const [harness, expected] of [["pi", true], ["pi-signed", true], ["omp", false]]) {
  Object.assign(process.env, { PRIVATEER_ROLE: "agent", PRIVATEER_HOME: "/fixture/home",
    PRIVATEER_CONTROL_CLI: "/fixture/privateer", PRIVATEER_ACTOR: "privateer",
    PRIVATEER_PRESENCE_HOLDER: "holder", PRIVATEER_ATTENDED_HARNESS: harness });
  const events = new Set();
  attended({ on: name => { events.add(name); return () => {}; }, registerCommand() {}, registerMessageRenderer() {} });
  assert.equal(events.has("agent_before_settle"), expected, harness);
}
console.log("ok - follow-through nudges once per idle set and never loops");
