// Loaded per invocation by privateer start. No model, account or tool policies.
const PREFIX = "privateer-mail:";
// A hosted session has nobody to type /privateer-mail resume after an error.
const HOSTED_RETRY_MS = 60000;
let config, session, key, timer, flight, pending, prepared, nudge, offer, resumeAt;
let active = false, busy = false, paused = false, seen = 0, generation = 0, statusText, nudged = "";

// The captain's fleet commands, as a front door and the Pi session offer them.
// Only the person at the keyboard runs them, as the captain, not as the ship.
const CAPTAIN = {
  stow: { description: "Stow the fleet before an update: /stow, /stow status or /stow resume",
    argv: rest => ({ "": ["stow"], status: ["stow", "status"], resume: ["stow", "resume"] })[rest] },
  yolo: { description: "Fleet tool approvals for every first mate: /yolo on, off or status",
    argv: rest => ({ "": ["permissions", "show"], status: ["permissions", "show"],
      on: ["permissions", "yolo"], off: ["permissions", "standard"] })[rest] },
  afk: { description: "Away: hold the fleet's mail except emergencies, /afk [until 8am] [instructions]",
    argv: rest => (rest === "" ? ["afk"] : ["afk", rest]) },
  back: { description: "Back from away: a digest now, the liaison's summary next",
    argv: rest => (rest === "" ? ["back"] : undefined) },
  ships: { description: "The fleet's ships and what each handles",
    argv: rest => (rest === "" ? ["ships"] : undefined) },
};

async function status($, text) {
  if (text === statusText) return;
  statusText = text;
  await $.ui.status(text);
}

async function call($, command, args) {
  const result = await $.process.run([config.cli, "--home", config.home, command, "--json", JSON.stringify(args)], {
    timeoutMs: 3000, env: { PRIVATEER_ROLE: "agent", PRIVATEER_ACTOR: config.ship },
  });
  if (result.exitCode !== 0 || result.stdout.length > 65536) throw Error("mail probe failed");
  return JSON.parse(result.stdout);
}

async function owned($, gen) {
  const value = await call($, "ship.presence", { ship: config.ship });
  return active && generation === gen && value.attended === true && value.lease?.holder === config.holder
    && await $.session.id() === session;
}

async function healthy($) {
  // Classic settings hooks stand down only while a loaded mod is healthy.
  // No marker is set by the launcher: disabled/refused mods retain the old hooks.
  await $.env.set("PRIVATEER_CLAUDE_MAIL_UNTIL", String(await $.clock.now() + 12000));
}

async function pause($) {
  paused = true;
  resumeAt = config?.hosted ? await $.clock.now() + HOSTED_RETRY_MS : undefined;
}

async function save($) {
  await $.store.set(key, { through: seen, paused, busy, holder: config.holder,
    pending: pending ? { text: pending.text, through: pending.through } : undefined });
}

function receipt(snapshot) {
  return { home: config.home, ship: config.ship, session, through: snapshot.through_seq };
}

function marker(snapshot) { return PREFIX + JSON.stringify(receipt(snapshot)); }

function message(snapshot) {
  const inbox = [config.cli, "--home", config.home, "mail.inbox", "--json",
    JSON.stringify({ address: config.ship, after_seq: snapshot.after_seq, limit: 40 })];
  return `${marker(snapshot)}\n${snapshot.count} new mailbox arrivals for ${config.ship}. `
    + `Read bodies with this argv: ${JSON.stringify(inbox)}. Page as needed; reply with correlation IDs, `
    + `and acknowledge only handled messages. This notification is not an acknowledgement. `
    + `Existing charter and production deployment approval rules apply. Headers: ${JSON.stringify(snapshot.headers)}`;
}

async function snapshot($, gen) {
  if (!await owned($, gen)) {
    if (active && gen === generation) {
      await $.env.set("PRIVATEER_CLAUDE_MAIL_UNTIL", undefined);
      await status($, "⚓ mail held by another session");
    }
    return;
  }
  const value = await call($, "mail.pending", { address: config.ship, after_seq: seen });
  if (!active || gen !== generation) return;
  if (value.address !== config.ship || ![value.count, value.actionable, value.through_seq,
      value.after_seq, value.acknowledged_through].every(n => Number.isSafeInteger(n) && n >= 0)
      || value.through_seq < value.after_seq || value.actionable > value.count
      || !Array.isArray(value.headers) || value.headers.length > Math.min(6, value.count)) throw Error("invalid mailbox snapshot");
  seen = Math.max(seen, value.acknowledged_through);
  if (paused && resumeAt !== undefined && await $.clock.now() >= resumeAt) {
    paused = false; resumeAt = undefined; await save($);
  }
  await healthy($);
  await status($, paused ? `⚓ ${value.count} new mail • paused; /privateer-mail resume` : `⚓ ${value.count} new mail`);
  return value;
}

async function deliver($, gen) {
  const value = await snapshot($, gen);
  if (!value || !value.actionable || busy || paused || pending || prepared || nudge || offer) return;
  if ((await $.prompt.read()).text.length || !await owned($, gen) || busy || paused || pending || nudge || offer) return;
  const item = { gen, text: message(value), through: value.through_seq };
  pending = item;
  try {
    await save($);
    const result = await $.prompt.submit({ text: item.text });
    if (result.drop && pending === item) {
      pending = undefined; await pause($); await save($);
      await status($, "⚓ native delivery declined; /privateer-mail resume");
    }
    // A queued prompt is not delivery evidence. turn.start records the receipt.
  } catch (error) {
    if (pending === item) pending = undefined;
    await pause($);
    try { await save($); } catch { /* No delivery watermark advanced. */ }
    throw error;
  }
}

// Runtime-owned scheduled work for a hosted session. Eligibility, budgets and
// leases live in Rust; the mod reports idleness and starts one offered turn.
async function work($, gen) {
  if (!config.hosted || !active || gen !== generation) return;
  const idle = !busy && !paused && !pending && !prepared && !nudge && !offer
    && !(await $.prompt.read()).text.length;
  let attention = null;
  try {
    const { context } = await $.session.usage();
    if (Number.isFinite(context?.tokens) && context?.window > 0) {
      attention = { tokens: Math.round(context.tokens), window: context.window };
    }
  } catch { /* the context reading is optional */ }
  if (!await owned($, gen)) return;
  const response = await call($, "work.poll", { ship: config.ship, holder: config.holder, idle, attention });
  const next = response?.offer;
  if (!next) return;
  const release = () => call($, "work.release", { ship: config.ship, holder: config.holder, offer_id: next.id });
  if (gen !== generation || offer || !idle || busy || pending || nudge) { await release(); return; }
  if (typeof next.id !== "string" || typeof next.task_id !== "string"
      || typeof next.prompt !== "string" || next.prompt.length > 24000) throw Error("invalid work offer");
  const item = { gen, id: next.id, task: next.task_id, text: next.prompt, started: false };
  offer = item;
  try {
    const result = await $.prompt.submit({ text: item.text });
    if (result.drop && offer === item) { offer = undefined; await release(); }
  } catch (error) {
    if (offer === item) { offer = undefined; await release(); }
    throw error;
  }
}

async function poll($) {
  if (!active || flight) return;
  const gen = generation;
  const operation = deliver($, gen).then(() => work($, gen).catch(async () => {
    if (active && gen === generation) await status($, "⚓ work delivery unavailable; durable offer retained");
  })).catch(async () => {
    if (active && gen === generation) await status($, "⚓ mail unavailable; inbox preserved; /privateer-mail resume");
  });
  flight = operation;
  try { await operation; }
  finally { if (flight === operation) flight = undefined; }
}

// The ship's attention: its live context after each turn and each compaction,
// reported by the session that holds the lease. Best effort.
async function attention($, kind, tokens, extra = {}) {
  try {
    const gen = generation;
    const { context } = await $.session.usage();
    tokens = tokens ?? context?.tokens;
    if (!active || !Number.isFinite(tokens) || !(context?.window > 0) || !await owned($, gen)) return;
    await call($, "attention.report", { ship: config.ship, kind, tokens: Math.round(tokens),
      window: context.window, holder: config.holder, ...extra });
  } catch { /* attention reports are best effort */ }
}

function followText(idle) {
  const list = idle.map(t => `- ${t.id}: ${t.title}`).join("\n");
  return `Before you stop: these tasks are marked running, but no job, crew or open request is moving them:\n${list}\n`
    + "Continue each one now (start its crew, its check or its next step), or record why it waits: "
    + "set it blocked with task.update, or pin what wakes it with note.pin "
    + `{"ship":"${config.ship}","focus":"task:<id>","key":"waiting","value":"..."}. `
    + "Do not end on a promise to do it later.";
}

// A turn can end on "I'll carry on with X", leaving X running with nothing on
// it. After a completed turn, give the first mate one more prompt to continue
// such tasks or record why they wait, once per distinct set, so it cannot loop.
async function follow($) {
  const gen = generation;
  try {
    if (!active || busy || paused || pending || prepared || nudge || offer || !await owned($, gen)) return;
    const idle = (await call($, "ship.bearings", { ship: config.ship })).idle_tasks;
    if (!Array.isArray(idle) || !idle.length) { nudged = ""; return; }
    const set = idle.map(t => t.id).sort().join(" ");
    if (set === nudged || gen !== generation || busy || pending || nudge
        || (await $.prompt.read()).text.length) return;
    nudged = set;
    const item = { gen, text: followText(idle) };
    nudge = item;
    const result = await $.prompt.submit({ text: item.text });
    if (result.drop && nudge === item) nudge = undefined;
  } catch { nudge = undefined; }
}

async function captain($, name, args) {
  const command = CAPTAIN[name];
  const argv = command.argv(args.trim());
  if (!argv) return { text: `Usage: ${command.description}` };
  // An operator role, not the ship's agent identity: the captain is typing.
  let result;
  try {
    result = await $.process.run([config.cli, "--home", config.home, ...argv], {
      cwd: config.home.replace(/\/[^/]+$/, ""), timeoutMs: 30000, env: { PRIVATEER_ROLE: "operator" },
    });
  } catch (error) {
    return { text: `/${name} failed: ${String(error?.message ?? error).slice(0, 2000)}` };
  }
  const text = String(result.exitCode === 0 ? result.stdout : (result.stderr || result.stdout)).trim();
  return { text: text.slice(0, 256 * 1024) || `/${name} done.` };
}

async function stop($) {
  active = false; generation++;
  timer?.cancel(); timer = undefined;
  pending = undefined; prepared = undefined; nudge = undefined; offer = undefined;
  await $.env.set("PRIVATEER_CLAUDE_MAIL_UNTIL", undefined);
}

async function recover($, e, next) {
  await stop($);
  await status($, "⚓ attended mail inactive; native hooks retained");
  return next(e);
}

async function bind($, interactive) {
  if (interactive === false) return;
  if (!config) {
    const role = await $.env.get("PRIVATEER_ROLE");
    const harness = await $.env.get("PRIVATEER_ATTENDED_HARNESS");
    const job = await $.env.get("PRIVATEER_JOB");
    config = { home: await $.env.get("PRIVATEER_HOME"), cli: await $.env.get("PRIVATEER_CONTROL_CLI"),
      ship: await $.env.get("PRIVATEER_ACTOR"), holder: await $.env.get("PRIVATEER_PRESENCE_HOLDER"),
      hosted: await $.env.get("PRIVATEER_HOSTED") === "1" };
    if (job || role !== "agent" || harness !== "claude" || !config.home?.startsWith("/")
        || !config.cli?.startsWith("/") || !config.ship || !config.holder) { config = undefined; return; }
  }
  if (interactive === undefined && !(await $.session.surfaces()).length) return;
  const id = await $.session.id();
  if (active && session === id) { await healthy($); return; }
  await stop($);
  session = id;
  key = PREFIX + JSON.stringify([config.home, config.ship, session]);
  const stored = await $.store.get(key);
  seen = Number.isSafeInteger(stored?.through) && stored.through >= 0 ? stored.through : 0;
  paused = stored?.paused === true;
  busy = stored?.holder === config.holder && stored?.busy === true;
  if (stored?.holder === config.holder && typeof stored?.pending?.text === "string"
      && stored.pending.text.length <= 65536 && Number.isSafeInteger(stored.pending.through)
      && stored.pending.through > seen) pending = { ...stored.pending, gen: generation };
  // Recover accepted wake receipts even if the process died before store.set.
  for (const row of (await $.session.messages()).slice(-128)) {
    if (row.role !== "user") continue;
    for (const line of row.text.slice(0, 8192).split("\n")) {
      const at = line.indexOf(PREFIX);
      if (at < 0) continue;
      try {
        const data = JSON.parse(line.slice(at + PREFIX.length));
        if (data.home === config.home && data.ship === config.ship && data.session === session
            && Number.isSafeInteger(data.through) && data.through >= 0) seen = Math.max(seen, data.through);
      } catch { /* Not a receipt. Never interpret arbitrary conversation text. */ }
    }
  }
  active = true;
  if (!await owned($, generation)) { await stop($); return; }
  await healthy($);
  // Bound this plugin's persistent receipt index; mail remains in SQLite.
  const keys = (await $.store.keys()).filter(k => k.startsWith(PREFIX) && k !== key);
  for (const old of keys.slice(0, Math.max(0, keys.length - 127))) await $.store.delete(old);
  await $.command.register({ name: "privateer-mail", description: "Pause, resume or inspect ship mail", argumentHint: "pause|resume|status", immediate: true });
  for (const [name, command] of Object.entries(CAPTAIN)) await $.command.register({ name, description: command.description });
  timer = $.clock.every(5000, async () => { await poll($); });
}

export function register(on) {
  on("session.start", async ($, e, next) => {
    await bind($, e.isInteractive);
    const result = await next(e);
    if (active) void poll($);
    return result;
  }).catch(($, e, next) => recover($, e, next));
  on("classic.SessionStart", async ($, e, next) => {
    await bind($);
    return next(e);
  }).catch(($, e, next) => recover($, e, next));
  on("session.end", async ($, e, next) => { await stop($); return next(e); });
  on("prompt.submit", async ($, e, next) => {
    if (!config) return next(e);
    await bind($);
    if (e.origin?.kind === "plugin") {
      const ours = [pending, nudge, offer].find(item => item?.gen === generation && e.text.includes(item.text));
      if (e.origin.name === "privateer-mail" && (!active || paused || !ours
          || !await owned($, generation))) return { drop: "Privateer mail wake superseded or paused; inbox preserved." };
      return next(e);
    }
    if (!active) return next(e);
    if (!["composer", "bridge"].includes(e.origin?.kind)) return next(e);
    // An actual captain prompt permits recovery; never add mail to a queued
    // mid-turn submission or change the person's text or draft.
    paused = false; resumeAt = undefined;
    await save($);
    if (busy || e.turnId || flight || pending) return next(e);
    const gen = generation;
    const value = await snapshot($, gen);
    if (!value?.count || !await owned($, gen)) return next(e);
    const item = { gen, text: e.text, through: value.through_seq };
    prepared = item;
    const result = await next({ ...e, context: [...(e.context ?? []), message(value)] });
    if (result.drop && prepared === item) { prepared = undefined; paused = true; await save($); }
    return result;
  }).catch(($, e, next) => recover($, e, next));
  on("turn.start", async ($, e, next) => {
    if (active) {
      busy = true;
      const item = pending && e.text.includes(pending.text) ? pending : prepared?.text === e.text ? prepared : undefined;
      if (item && item.gen === generation) {
        seen = Math.max(seen, item.through);
        if (item === pending) pending = undefined;
        if (item === prepared) prepared = undefined;
      }
      if (nudge && e.text.includes(nudge.text)) nudge = undefined;
      if (offer && !offer.started && e.text.includes(offer.text)) {
        offer.started = true;
        // Do not invent acceptance on failure; the runtime keeps the claim.
        void call($, "work.started", { ship: config.ship, holder: config.holder, offer_id: offer.id }).catch(() => {});
      }
      await save($);
    }
    return next(e);
  }).catch(($, e, next) => recover($, e, next));
  on("turn.complete", async ($, e, next) => {
    const main = active && !e.agentId;
    const completed = main && !e.isAborted && e.reason === "answer";
    if (main) {
      busy = false;
      if (!completed) await pause($);
      await save($);
      if (offer?.started) {
        const done = offer;
        offer = undefined;
        // An ambiguous native stop is reconciled by the durable deadline.
        void call($, "work.settled", { ship: config.ship, holder: config.holder, offer_id: done.id }).catch(() => {});
      }
    }
    const result = await next(e);
    if (main) void attention($, "turn");
    if (completed) void follow($);
    return result;
  }).catch(($, e, next) => recover($, e, next));
  on("session.compact", async ($, e, next) => {
    const result = await next(e);
    if (active && !e.agentId && !result?.skip && Number.isFinite(result?.tokensBefore)) {
      void attention($, "compaction", result.tokensBefore);
    }
    return result;
  });
  on("command.run", async ($, e, next) => {
    if (CAPTAIN[e.command]) {
      if (!config) return next(e);
      if (!["composer", "bridge"].includes(e.origin?.kind)) return { text: `/${e.command} runs only from the captain's keyboard.` };
      return captain($, e.command, e.args);
    }
    if (e.command !== "privateer-mail") return next(e);
    if (!active) return { text: "Privateer attended mail is inactive in this session." };
    const arg = e.args.trim();
    if (arg === "pause") paused = true;
    else if (arg === "resume") paused = false;
    await save($);
    if (arg === "resume") void poll($);
    await status($, paused ? "⚓ mail paused; /privateer-mail resume" : "⚓ mail enabled");
    return { text: paused ? "Ship mail paused; messages remain in the inbox." : "Ship mail enabled." };
  });
}
