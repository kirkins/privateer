// Helpers for the run-selected MCP extension. Pure except for the token file
// read, so Node tests import them directly (like calm.mjs). The launcher sets
// PRIVATEER_MCP_SERVERS (records only); each token file is read here, never
// passed through argv, env values, logs or errors.
import { readFileSync } from "node:fs";

export const EXIT_UNAVAILABLE = 78;
const READY_SECONDS = 30;
const POLL_MS = 100;
const NAME = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const CREDENTIAL = /^[A-Z][A-Z0-9_]{0,63}$/;
const TOKEN = /^[A-Za-z0-9._~+\/-]+=*$/;
const KEYS = ["name", "url", "credential", "timeout", "description"];
const LOOPBACK = ["127.0.0.1", "[::1]", "localhost"];

// Ban what Pi would expand or run: "$" (env expansion), a leading "!" (shell
// command), and control characters.
const plain = (text) => !text.includes("$") && !text.startsWith("!") && !/[\u0000-\u001f\u007f]/.test(text);

function checkUrl(text) {
  if (typeof text !== "string" || text.length > 2048 || !plain(text) || text.includes("#")) throw new Error("url");
  const url = new URL(text);
  if (url.username || url.password || text.includes("@")) throw new Error("url");
  if (url.protocol === "https:") return;
  if (url.protocol !== "http:" || !LOOPBACK.includes(url.hostname)) throw new Error("url");
}

function checkRecord(record) {
  if (!record || typeof record !== "object" || Array.isArray(record)) throw new Error("record");
  for (const key of Object.keys(record)) if (!KEYS.includes(key)) throw new Error("key");
  for (const key of ["name", "url", "credential", "timeout"]) if (!(key in record)) throw new Error("key");
  if (typeof record.name !== "string" || record.name.length > 32 || !NAME.test(record.name) || !plain(record.name)) throw new Error("name");
  checkUrl(record.url);
  if (typeof record.credential !== "string" || !CREDENTIAL.test(record.credential)) throw new Error("credential");
  if (!Number.isInteger(record.timeout) || record.timeout < 1 || record.timeout > 300) throw new Error("timeout");
  if ("description" in record && (typeof record.description !== "string" || record.description.length > 200 || !plain(record.description))) throw new Error("description");
}

/** Parsed and re-checked selected records, or a throw that names no value. */
export function parseRecords(env) {
  const list = JSON.parse(env.PRIVATEER_MCP_SERVERS ?? "null");
  if (!Array.isArray(list) || list.length < 1 || list.length > 8) throw new Error("servers");
  for (const record of list) checkRecord(record);
  const keys = list.map((record) => record.name.replaceAll("-", "_"));
  if (new Set(keys).size !== keys.length) throw new Error("collision");
  return list;
}

/** A token file's contents: one trailing newline stripped, RFC 6750 shape only. */
export function readToken(path, read = (file) => readFileSync(file, "utf8")) {
  let text = read(path);
  if (text.endsWith("\n")) text = text.slice(0, -1);
  if (text.length > 4096 || !TOKEN.test(text)) throw new Error("token");
  return text;
}

/** Pi's tool namespace for a server: non-alphanumerics become "_". */
export const prefixOf = (name) => `mcp__${name.replace(/[^A-Za-z0-9]/g, "_")}__`;

/** The config Pi's createMcpExtension receives for one selected server. */
export function serverEntry(record, token) {
  const config = { url: record.url, headers: { Authorization: `Bearer ${token}` }, timeout: record.timeout, exposure: "direct" };
  if ("description" in record) config.description = record.description;
  return { name: record.name, config, source: "privateer-selected", scope: "extension" };
}

export class UnavailableError extends Error {
  constructor(server) {
    super("unavailable");
    this.server = server;
  }
}

/** A loader result for every selected record. Any token problem names only its server. */
export function loadEntries(env, read) {
  return parseRecords(env).map((record) => {
    try {
      const file = env[`PRIVATEER_CREDENTIAL_${record.credential}_FILE`];
      if (!file) throw new Error("missing");
      return serverEntry(record, readToken(file, read));
    } catch {
      throw new UnavailableError(record.name);
    }
  });
}

/** Exactly one fixed stderr line naming the server, then a refusal exit. */
export function unavailable(server, stderr = (text) => process.stderr.write(text), exit = (code) => process.exit(code)) {
  stderr(`privateer: MCP server "${server}" is unavailable; the run stopped before a model turn.\n`);
  exit(EXIT_UNAVAILABLE);
}

/** Tools present for every selected server, and none from another namespace.
 *  Returns the first foreign server's name (or null) and the first absent one (or null). */
export function missing(names, records) {
  const prefixes = records.map((record) => ({ name: record.name, prefix: prefixOf(record.name) }));
  const stray = names.find((name) => name.startsWith("mcp__") && !prefixes.some(({ prefix }) => name.startsWith(prefix)));
  const absent = prefixes.find(({ prefix }) => !names.some((name) => name.startsWith(prefix)));
  return { foreign: stray ? stray.split("__")[1] ?? "unknown" : null, absent: absent?.name ?? null };
}

/**
 * The readiness gate. session_start refuses another extension's server, then
 * polls for every selected server's tools. before_agent_start awaits the same
 * promise, so a failure ends the run before any model turn.
 */
export function gate(pi, records, { fail = unavailable, now = Date.now, sleep = (ms) => new Promise((r) => setTimeout(r, ms)) } = {}) {
  const windowMs = Math.min(READY_SECONDS, Math.max(...records.map((record) => record.timeout))) * 1000;
  let pending = null;
  return {
    start() {
      const extra = pi.getMcpServers();
      if (extra.length > 0) return fail(String(extra[0]?.name ?? "another extension"));
      pending = (async () => {
        const deadline = now() + windowMs;
        for (;;) {
          const names = pi.getAllTools().map((tool) => tool.name);
          const { foreign, absent } = missing(names, records);
          // Name the other namespace's server (a tool name's middle segment).
          if (foreign) return fail(foreign);
          if (!absent) return;
          if (now() >= deadline) return fail(absent);
          await sleep(POLL_MS);
        }
      })();
    },
    wait() {
      return pending ?? Promise.resolve();
    },
  };
}
