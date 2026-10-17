// Selected-MCP helper tests: registry-shaped refusals, token handling and the
// readiness gate. No network, real secrets or Pi process. Run: node tests/pi-mcp-selected.mjs
import assert from "node:assert/strict";
import {
  EXIT_UNAVAILABLE, foreignTool, gate, loadEntries, missing, parseRecords, prefixOf, readToken, serverEntry, UnavailableError,
} from "../plugins/pi-mcp/selected.mjs";

const SENTINEL = `SENTINEL-${"a".repeat(24)}`;
const record = (overrides = {}) => ({
  name: "docs", url: "https://mcp.example.test/mcp", credential: "DOCS_TOKEN", timeout: 5, ...overrides,
});
const env = (records, extra = {}) => ({ PRIVATEER_MCP_SERVERS: JSON.stringify(records), ...extra });

// Every mutation must refuse, never normalize into a working server.
const refusals = {
  "unknown key": [record({ command: "x" })],
  "headers key": [record({ headers: { Authorization: "x" } })],
  "dollar in url": [record({ url: "https://h.example/$TOKEN" })],
  "bang-leading description": [record({ description: "!cat /etc/passwd" })],
  "control in description": [record({ description: "a\u0007b" })],
  "description too long": [record({ description: "x".repeat(201) })],
  "remote plain http": [record({ url: "http://mcp.example.test/mcp" })],
  "userinfo": [record({ url: "https://user:pw@mcp.example.test/mcp" })],
  "fragment": [record({ url: "https://mcp.example.test/mcp#x" })],
  "bad credential name": [record({ credential: "docs_token" })],
  "timeout zero": [record({ timeout: 0 })],
  "timeout 301": [record({ timeout: 301 })],
  "timeout string": [record({ timeout: "5" })],
  "name with underscore": [record({ name: "docs_1" })],
  "nine servers": Array.from({ length: 9 }, (_, i) => record({ name: `s${i}` })),
  "backslash host": [record({ url: "https://evil.example\\.good.example/mcp" })],
  "backslash path": [record({ url: "https://mcp.example.test\\/mcp" })],
  "space in host": [record({ url: "https://mcp example.test/mcp" })],
  "tab in url": [record({ url: "https://mcp.example.test/mcp\t" })],
  "bad port": [record({ url: "https://mcp.example.test:99999/mcp" })],
  "non-numeric port": [record({ url: "https://mcp.example.test:abc/mcp" })],
  "underscore host": [record({ url: "https://bad_host.example.test/mcp" })],
  "userinfo in host": [record({ url: "https://user@mcp.example.test/mcp" })],
};

for (const [label, records] of Object.entries(refusals)) {
  assert.throws(() => parseRecords(env(records)), undefined, label);
}
assert.throws(() => parseRecords({}), undefined, "missing variable");
assert.throws(() => parseRecords(env([])), undefined, "empty list");
// Two servers whose names collide are refused as a collision, not as a bad name.
assert.throws(() => parseRecords(env([record({ name: "a-b" }), record({ name: "a-b" })])), { message: "collision" });
// Names that differ before normalization do not both pass the name rule.
assert.throws(() => parseRecords(env([record({ name: "a-b" }), record({ name: "a_b" })])), { message: "name" });

// Accepted shapes: loopback http, query strings and a description.
assert.equal(parseRecords(env([record({ url: "http://127.0.0.1:9000/mcp?x=1" })])).length, 1);
assert.equal(parseRecords(env([record({ url: "http://[::1]:9000/mcp" })])).length, 1);
assert.equal(parseRecords(env([record({ url: "http://localhost/mcp", description: "Docs search" })])).length, 1);

// Tokens: one trailing newline stripped; anything else outside the RFC 6750 set refuses.
assert.equal(readToken("/x", () => `${SENTINEL}\n`), SENTINEL);
assert.equal(readToken("/x", () => `${SENTINEL}`), SENTINEL);
for (const bad of ["a b", "a$b", "!cmd", "", "a\nb", "a".repeat(4097)]) {
  assert.throws(() => readToken("/x", () => bad), undefined, JSON.stringify(bad.slice(0, 8)));
}

// Loader: a missing file or bad token names only its server, never the value.
const files = { "/fleet/docs": `${SENTINEL}\n` };
const read = (file) => {
  if (!(file in files)) throw new Error("ENOENT");
  return files[file];
};
const entries = loadEntries(env([record()], { PRIVATEER_CREDENTIAL_DOCS_TOKEN_FILE: "/fleet/docs" }), read);
assert.deepEqual(entries, [{
  name: "docs",
  config: { url: "https://mcp.example.test/mcp", headers: { Authorization: `Bearer ${SENTINEL}` }, timeout: 5, exposure: "direct" },
  source: "privateer-selected",
  scope: "extension",
}]);
assert.equal(serverEntry(record({ description: "Docs" }), "t").config.description, "Docs");
assert.equal("description" in serverEntry(record(), "t").config, false);
try {
  loadEntries(env([record()]), read);
  assert.fail("missing credential path must refuse");
} catch (error) {
  assert.ok(error instanceof UnavailableError);
  assert.equal(error.server, "docs");
  assert.equal(String(error.message).includes(SENTINEL), false);
}
try {
  loadEntries(env([record()], { PRIVATEER_CREDENTIAL_DOCS_TOKEN_FILE: "/fleet/bad" }), () => `${SENTINEL} with space\n`);
  assert.fail("bad token must refuse");
} catch (error) {
  assert.ok(error instanceof UnavailableError);
  assert.equal(JSON.stringify(error).includes(SENTINEL), false);
}

// Pi's namespace for a server, and a plain verdict on tool names.
assert.equal(prefixOf("docs-search"), "mcp__docs_search__");
const records = [record({ name: "docs" })];
assert.deepEqual(missing(["mcp__docs__search"], records), { foreign: null, absent: null });
assert.deepEqual(missing([], records), { foreign: null, absent: "docs" });
assert.deepEqual(missing(["mcp__docs__search", "mcp__other__x"], records), { foreign: "other", absent: null });
// Only a selected namespace's tool passes the tool_call guard; built-in tools are not MCP.
assert.equal(foreignTool("mcp__docs__search", records), false);
assert.equal(foreignTool("mcp__other__search", records), true);
assert.equal(foreignTool("bash", records), false);

// Readiness gate, on a fake clock: no real sleeping.
function fakePi({ tools = [], servers = [] } = {}) {
  return { getAllTools: () => tools.map((name) => ({ name })), getMcpServers: () => servers };
}
function clock() {
  let now = 0;
  return { now: () => now, sleep: async (ms) => { now += ms; } };
}
{
  // Ready on the second poll: no failure, and before_agent_start resolves.
  const calls = [];
  const pi = fakePi();
  let polls = 0;
  pi.getAllTools = () => (++polls > 1 ? [{ name: "mcp__docs__search" }] : []);
  const checks = gate(pi, records, { ...clock(), fail: (server) => calls.push(server) });
  checks.start();
  await checks.wait();
  assert.deepEqual(calls, []);
}
{
  // Another extension's server is refused at session_start.
  const calls = [];
  const checks = gate(fakePi({ servers: [{ name: "rogue" }] }), records, { ...clock(), fail: (server) => calls.push(server) });
  checks.start();
  assert.deepEqual(calls, ["rogue"]);
}
{
  // Never ready within min(30, timeout) seconds: refuse, naming the absent server.
  const calls = [];
  const checks = gate(fakePi(), [record({ timeout: 3 })], { ...clock(), fail: (server) => calls.push(server) });
  checks.start();
  await checks.wait();
  assert.deepEqual(calls, ["docs"]);
}
{
  // A foreign-namespace tool refuses, naming that namespace's server.
  const calls = [];
  const pi = fakePi({ tools: ["mcp__docs__search", "mcp__intruder__x"] });
  const checks = gate(pi, records, { ...clock(), fail: (server) => calls.push(server) });
  checks.start();
  await checks.wait();
  assert.deepEqual(calls, ["intruder"]);
}
{
  // A server registered after startup refuses at mcp_servers_change; none passes.
  const calls = [];
  const fail = (server) => calls.push(server);
  gate(fakePi({ servers: [{ name: "late" }] }), records, { ...clock(), fail }).late();
  gate(fakePi(), records, { ...clock(), fail }).late();
  assert.deepEqual(calls, ["late"]);
}
{
  // Readiness that was never started refuses; it does not resolve as ready.
  const calls = [];
  const checks = gate(fakePi(), records, { ...clock(), fail: (server) => calls.push(server) });
  assert.throws(() => checks.wait(), /never started/);
  assert.deepEqual(calls, ["readiness"]);
}
for (const url of ["https://evil.example\\.good.example/mcp", "https://mcp.example.test\\/mcp", "https://mcp example.test/mcp", "https://mcp.example.test/mcp\n"]) {
  assert.throws(() => parseRecords(env([record({ url })])), undefined, JSON.stringify(url));
}
assert.equal(EXIT_UNAVAILABLE, 78);
console.log("pi-mcp-selected: ok");
