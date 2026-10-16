// The attended adapter reports the live context to Privateer through the real
// CLI boundary (a recording fake CLI here). No model or network.
import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import attended from "../plugins/attended/mail.mjs";

const dir = mkdtempSync(join(tmpdir(), "privateer-attention-"));
try {
  const calls = join(dir, "calls.jsonl"), holder = join(dir, "holder");
  const cli = join(dir, "privateer");
  writeFileSync(cli, `#!/usr/bin/env python3
import json, sys
command, args = sys.argv[3], json.loads(sys.argv[5])
open(${JSON.stringify(calls)}, "a").write(json.dumps([command, args]) + "\\n")
if command == "ship.presence":
    print(json.dumps({"attended": True, "lease": {"holder": open(${JSON.stringify(holder)}).read().strip()}}))
else:
    print("{}")
`);
  chmodSync(cli, 0o755);
  writeFileSync(holder, "this-session");
  Object.assign(process.env, { PRIVATEER_ROLE: "agent", PRIVATEER_HOME: join(dir, ".privateer"),
    PRIVATEER_CONTROL_CLI: cli, PRIVATEER_ACTOR: "privateer", PRIVATEER_PRESENCE_HOLDER: "this-session",
    PRIVATEER_ATTENDED_HARNESS: "pi", PRIVATEER_QUIET_COMPACT_SECS: "0" });
  const handlers = new Map();
  attended({ on: (name, fn) => { (handlers.get(name) ?? handlers.set(name, []).get(name)).push(fn); return () => {}; },
    registerCommand() {}, registerMessageRenderer() {}, sendMessage() {} });
  const fire = (name, event, ctx) => Promise.all((handlers.get(name) ?? []).map(fn => fn(event, ctx)));
  const ctx = { hasUI: true, isIdle: () => true, ui: { setStatus() {} }, sessionManager: { getBranch: () => [] },
    getContextUsage: () => ({ tokens: 150_000.4, contextWindow: 272_000, percent: 55 }) };
  await fire("session_start", {}, ctx);
  const reports = () => readFileSync(calls, "utf8").trim().split("\n").map(l => JSON.parse(l))
    .filter(([command]) => command === "attention.report").map(([, args]) => args);

  await fire("agent_settled", {}, ctx);
  await fire("session_compact", { compactionEntry: { tokensBefore: 258_000, summary: "s".repeat(5_000) } }, ctx);
  assert.deepEqual(reports(), [
    { ship: "privateer", kind: "turn", tokens: 150_000, window: 272_000, holder: "this-session" },
    { ship: "privateer", kind: "compaction", tokens: 258_000, window: 272_000, holder: "this-session", summary_chars: 5_000 },
  ]);

  // A session that does not hold the ship's lease reports nothing.
  writeFileSync(holder, "another-session");
  await fire("agent_settled", {}, ctx);
  assert.equal(reports().length, 2);
  await fire("session_shutdown", {}, ctx);
  console.log("ok - the attended adapter reports runs and compactions from the lease holder only");
} finally { rmSync(dir, { recursive: true, force: true }); }
