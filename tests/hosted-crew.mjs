// The hosted crew extension, with simulated native events. No models.
import assert from "node:assert/strict";
import { constants } from "node:fs";
import crew, { DROPPED, EventLog, HostedCrew, LOGGED, MAX_LINE, O_CLOEXEC, sharedLog } from "../plugins/attended/crew.mjs";

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
const flagged = {};
const io = (writes, { fifo = true, fail = [], opened = [], unlinked = [] } = {}) => ({
  openSync: (path, flags) => (opened.push(path), (flagged.at = flags), 9),
  fstatSync: () => ({ isFIFO: () => fifo }),
  unlinkSync: path => unlinked.push(path),
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

// An event too long to send is a marker saying how long it was, and the log
// goes on.
writes = [];
log = new EventLog("/x/events", io(writes));
const image = "A".repeat(MAX_LINE);
log.record("tool_execution_end", { toolCallId: "c1", result: { content: [{ type: "image", data: image }] } });
log.record("turn_end", { message: { stopReason: "toolUse" } });
const [marker, after] = lines(writes);
assert.equal(marker.type, DROPPED);
assert.equal(marker.event, "tool_execution_end");
assert(marker.bytes > MAX_LINE, `${marker.bytes}`);
assert.deepEqual(after, { type: "turn_end", stopReason: "toolUse" });
assert(writes.join("").length < 1000);
// Just under the limit is sent whole.
writes = [];
log = new EventLog("/x/events", { ...io(writes), writeSync: (fd, buffer, at, length) => (writes.push(buffer.subarray(at, at + length).toString()), length) });
log.write({ type: "message_end", text: "B".repeat(MAX_LINE - 100) });
assert.equal(lines(writes)[0].type, "message_end");

// Once open, the FIFO's name is gone; a path that is not one is never removed.
let unlinked = [];
new EventLog("/x/events", io([], { unlinked })).open();
new EventLog("/x/file", io([], { fifo: false, unlinked })).open();
assert.deepEqual(unlinked, ["/x/events"]);
// Its descriptor is closed across exec, so no command the crew runs holds it.
assert(O_CLOEXEC > 0);
assert.equal(flagged.at, constants.O_WRONLY | constants.O_NONBLOCK | O_CLOEXEC);

// The first load takes the FIFO's name out of the environment the crew's
// commands inherit, and opens it; each later load (Pi's /new and /reload run
// extension factories again) shares that log.
const SHARED = Symbol.for("privateer.crew.events");
delete globalThis[SHARED];
const env = { PRIVATEER_CREW_EVENTS: "/x/events", PATH: "/bin" };
let opened = [];
unlinked = [];
writes = [];
const first = sharedLog(env, io(writes, { opened, unlinked }));
assert.deepEqual(env, { PATH: "/bin" });
assert.deepEqual(opened, ["/x/events"]);
assert.deepEqual(unlinked, ["/x/events"]);
env.PRIVATEER_CREW_EVENTS = "/y/events";
assert.equal(sharedLog(env, io([])), first);
assert.deepEqual(opened, ["/x/events"]);
first.record("agent_start", {});
assert.deepEqual(lines(writes), [{ type: "agent_start" }]);
// So does the extension itself, from the process environment.
delete globalThis[SHARED];
process.env.PRIVATEER_CREW_EVENTS = "/nonexistent/privateer-test/events";
crew({ on: () => {} });
assert.equal(process.env.PRIVATEER_CREW_EVENTS, undefined);
crew({ on: () => {} });
assert.equal(globalThis[SHARED].closed, true);
console.log("Hosted crew: settle, failure, interruption and event log passed");
