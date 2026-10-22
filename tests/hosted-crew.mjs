// The hosted crew extension, with simulated native events. No models.
import assert from "node:assert/strict";
import crew, { EventLog, HostedCrew, LOGGED } from "../plugins/attended/crew.mjs";

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
// Its events are logged before its settle can shut the session down.
const order = [];
crew({ on: event => order.push(event) });
assert.deepEqual(order.slice(0, LOGGED.length + 1), ["session_start", ...LOGGED]);
assert.deepEqual(order.slice(-2), ["message_end", "agent_settled"]);

// The event log writes whole JSON lines to the supervisor's FIFO.
const io = (writes, { fifo = true, fail = [] } = {}) => ({
  openSync: () => 9,
  fstatSync: () => ({ isFIFO: () => fifo }),
  // Writes at most 8 bytes at once; listed errors are thrown in turn.
  writeSync: (fd, buffer, at, length) => {
    const error = fail.shift();
    if (error) throw Object.assign(new Error(error), { code: error });
    const n = Math.min(8, length);
    writes.push(buffer.subarray(at, at + n).toString());
    return n;
  },
  pause: () => {},
});
const lines = writes => writes.join("").split("\n").filter(Boolean).map(line => JSON.parse(line));
let writes = [];
let log = new EventLog("/x/events", io(writes, { fail: ["EAGAIN"] }));
log.header({ sessionManager: { getHeader: () => ({ type: "session", id: "s1", cwd: "/w" }) } });
log.record("message_end", { message: { role: "assistant", stopReason: "stop" } });
log.record("turn_end", { message: { role: "assistant", stopReason: "stop", content: ["long"] }, toolResults: [1] });
log.record("agent_end", { messages: [1, 2], willRetry: false });
assert.deepEqual(lines(writes), [
  { type: "session", id: "s1", cwd: "/w" },
  { type: "message_end", message: { role: "assistant", stopReason: "stop" } },
  { type: "turn_end", stopReason: "stop" },
  { type: "agent_end", willRetry: false },
]);
// Without a header, the session id alone; without either, nothing.
writes = [];
log = new EventLog("/x/events", io(writes));
log.header({ sessionManager: { getSessionId: () => "s2" } });
log.header({});
assert.deepEqual(lines(writes), [{ type: "session", id: "s2" }]);
// No FIFO named, a path that is not one, or a reader gone: nothing is
// written, and the session goes on.
writes = [];
new EventLog(undefined, io(writes)).record("agent_start", {});
new EventLog("/x/file", io(writes, { fifo: false })).record("agent_start", {});
log = new EventLog("/x/events", io(writes, { fail: ["EPIPE"] }));
log.record("agent_start", {});
log.record("agent_start", {});
assert.deepEqual(writes, []);
console.log("Hosted crew: settle, failure, interruption and event log passed");
