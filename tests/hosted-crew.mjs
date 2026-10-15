// The hosted crew extension, with simulated native events. No models.
import assert from "node:assert/strict";
import crew, { HostedCrew } from "../plugins/attended/crew.mjs";

const run = (stops, settled = {}) => {
  let failed = 0, shutdowns = 0;
  const member = new HostedCrew({ fail: () => failed++ });
  for (const stopReason of stops) member.messageEnded({ message: { role: "assistant", stopReason } });
  member.settled(settled, { shutdown: () => shutdowns++ });
  return { failed, shutdowns };
};
// Finished work ends the session; a failed last turn ends it as a failure.
assert.deepEqual(run(["toolUse", "stop"]), { failed: 0, shutdowns: 1 });
assert.deepEqual(run(["stop", "error"]), { failed: 1, shutdowns: 1 });
// An interrupted run stays open for the captain.
assert.deepEqual(run(["aborted"]), { failed: 0, shutdowns: 0 });
assert.deepEqual(run(["toolUse"], { aborted: true }), { failed: 0, shutdowns: 0 });
// Only a hosted crew session loads it.
const handlers = new Map();
delete process.env.PRIVATEER_HOSTED_CREW;
crew({ on: (event, fn) => handlers.set(event, fn) });
assert.equal(handlers.size, 0);
process.env.PRIVATEER_HOSTED_CREW = "1";
crew({ on: (event, fn) => handlers.set(event, fn) });
assert(handlers.has("agent_settled") && handlers.has("message_end"));
console.log("Hosted crew: settle, failure and interruption passed");
