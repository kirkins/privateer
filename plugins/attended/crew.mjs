// A hosted crew member runs interactively in its own pane. Its job ends when
// the harness exits, so the session shuts down once its work settles: with
// status 1 when the last turn failed. An interrupted run (Esc) stays open for
// the captain to steer, and ends at the next settle.
import { constants, fstatSync, openSync, writeSync } from "node:fs";

export class HostedCrew {
  constructor(dependencies = {}) {
    this.fail = dependencies.fail ?? (() => process.once("exit", () => process.exit(1)));
    this.lastStop = undefined;
  }

  messageEnded(event) {
    if (event.message?.role === "assistant") this.lastStop = event.message.stopReason;
  }

  settled(event, ctx) {
    const stop = this.lastStop;
    this.lastStop = undefined;
    if (event.aborted === true || stop === "aborted") return;
    if (stop === "error") this.fail();
    ctx.shutdown();
  }
}

// The events a headless Pi prints as JSON lines, here written for the crew's
// supervisor, which masks them into the job log. The pane keeps the terminal.
// Streaming updates are left out: each finished message and tool result is
// written whole. A run's and a turn's end repeat messages already written, so
// only their markers are.
export const LOGGED = [
  "agent_start", "turn_start", "message_end", "tool_execution_start", "tool_execution_end",
  "turn_end", "agent_end", "agent_settled", "auto_retry_start", "auto_retry_end",
  "compaction_start", "compaction_end",
];
const STALL_MS = 5000;

const pause = ms => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);

export class EventLog {
  constructor(path, io = { openSync, fstatSync, writeSync, pause }) {
    this.path = path;
    this.io = io;
    this.fd = undefined;
    this.closed = !path;
  }

  // The supervisor holds the FIFO's read end before the harness starts. With
  // no reader the open fails at once rather than hanging the session, and the
  // log stays empty as before.
  open() {
    if (this.closed || this.fd !== undefined) return this.fd !== undefined;
    try {
      const fd = this.io.openSync(this.path, constants.O_WRONLY | constants.O_NONBLOCK);
      if (!this.io.fstatSync(fd).isFIFO()) throw new Error("not a FIFO");
      this.fd = fd;
    } catch {
      this.closed = true;
    }
    return this.fd !== undefined;
  }

  header(ctx) {
    const sessions = ctx?.sessionManager;
    const header = sessions?.getHeader?.();
    const id = header?.id ?? sessions?.getSessionId?.();
    if (typeof id === "string" && id) this.write({ ...(header ?? {}), type: "session", id });
  }

  record(type, event) {
    let shown = { ...event, type };
    if (type === "turn_end") shown = { type, stopReason: event?.message?.stopReason };
    if (type === "agent_end") shown = { type, willRetry: event?.willRetry };
    this.write(shown);
  }

  // A full pipe waits for the supervisor briefly; a gone or stalled one ends
  // the log, never the session.
  write(event) {
    if (!this.open()) return;
    let line;
    try {
      line = Buffer.from(`${JSON.stringify(event)}\n`);
    } catch {
      return;
    }
    const until = Date.now() + STALL_MS;
    let at = 0;
    while (at < line.length) {
      try {
        at += this.io.writeSync(this.fd, line, at, line.length - at);
      } catch (error) {
        if (error?.code === "EAGAIN" && Date.now() < until) {
          this.io.pause(10);
          continue;
        }
        this.closed = true;
        this.fd = undefined;
        return;
      }
    }
  }
}

export default function crew(api) {
  if (process.env.PRIVATEER_HOSTED_CREW !== "1") return;
  // Registered first, so a settle is logged before the session shuts down.
  const log = new EventLog(process.env.PRIVATEER_CREW_EVENTS);
  api.on("session_start", (_event, ctx) => log.header(ctx));
  for (const type of LOGGED) api.on(type, event => log.record(type, event));
  const crew = new HostedCrew();
  api.on("message_end", event => crew.messageEnded(event));
  api.on("agent_settled", (event, ctx) => crew.settled(event, ctx));
}
