// Privateer's run-selected MCP servers: remote streamable-HTTP only, each with a
// Bearer token read from a credential file inside this Pi process. Loaded only
// by a bounded Pi run that selects servers; helpers and the gate are in selected.mjs.
import { createMcpExtension } from "@earendil-works/pi-coding-agent";
import { gate, loadEntries, parseRecords, unavailable, UnavailableError } from "./selected.mjs";

const factory = createMcpExtension({
  loadConfig: () => {
    try {
      return { servers: loadEntries(process.env), errors: [], autoEnableCodemode: false };
    } catch (error) {
      // A selected server failed to load: name only that server, then refuse.
      unavailable(error instanceof UnavailableError ? error.server : "selection");
      throw error;
    }
  },
  // Config writes and browser opening are never granted to a selected run.
  updateConfig: () => { throw new Error("config writes are not allowed"); },
  openUrl: () => { throw new Error("browser opening is not allowed"); },
});

export default function selected(pi: any) {
  let records;
  try {
    records = parseRecords(process.env);
  } catch {
    // Malformed selection: refuse before any server or model turn.
    unavailable("selection");
    return;
  }
  const checks = gate(pi, records);
  // Registered before the MCP factory, so the refusal sees only servers that
  // other extensions registered at load time, not this run's own.
  pi.on("session_start", () => checks.start());
  factory(pi);
  pi.on("before_agent_start", () => checks.wait());
}
