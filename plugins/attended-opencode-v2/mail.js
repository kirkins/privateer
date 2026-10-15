// OpenCode V2 TUI-only adapter. Mail custody and subprocess control stay in Rust.
import { execFile } from "node:child_process";
import { realpathSync } from "node:fs";
import { dirname, isAbsolute } from "node:path";
import { randomBytes } from "node:crypto";

const PREFIX = "privateer-mail-v2";
const sequence = n => Number.isSafeInteger(n) && n >= 0;
const sessionID = id => typeof id === "string" && /^ses_[A-Za-z0-9]{1,508}$/.test(id);

function configuration(ctx) {
  const e = process.env;
  if (e.PRIVATEER_ROLE !== "agent" || e.PRIVATEER_JOB || e.PRIVATEER_ATTENDED_HARNESS !== "opencode-v2"
      || !/^2\./.test(ctx.app.version) || !isAbsolute(e.PRIVATEER_HOME ?? "")
      || !isAbsolute(e.PRIVATEER_CONTROL_CLI ?? "") || !e.PRIVATEER_ACTOR
      || !e.PRIVATEER_PRESENCE_HOLDER?.startsWith("start:")) return;
  const home = realpathSync(e.PRIVATEER_HOME), cli = realpathSync(e.PRIVATEER_CONTROL_CLI);
  const folder = dirname(home);
  if (!ctx.location || realpathSync(ctx.location.directory) !== folder) return;
  return { home, cli, folder, ship: e.PRIVATEER_ACTOR, holder: e.PRIVATEER_PRESENCE_HOLDER };
}

export default {
  id: "privateer.attended-mail-v2",
  async setup(ctx, renderStatus) {
    let config;
    try { config = configuration(ctx); } catch { return; }
    if (!config) return;
    const [memory, mutate] = ctx.storage.memory(PREFIX, { initial: { epoch: 0, status: "⚓ ship mail" } });
    const epoch = memory.epoch + 1;
    mutate(d => { d.epoch = epoch; });
    const [durable, persist] = ctx.storage.store(PREFIX, { initial: { sessions: {} } });
    const abort = new AbortController(), children = new Set(), cleanups = [];
    let active = true, flight, current, key, state, timer, writes = Promise.resolve();
    const live = () => active && memory.epoch === epoch;
    const status = text => { if (live()) mutate(d => { d.status = text; }); };
    const validRecord = value => value && sequence(value.seen) && typeof value.paused === "boolean";

    function call(command, args) {
      return new Promise((resolve, reject) => {
        if (!live()) return reject(Error("inactive adapter"));
        const child = execFile(config.cli, ["--home", config.home, command, "--json", JSON.stringify(args)], {
          cwd: config.folder, timeout: 3000, maxBuffer: 65536, killSignal: "SIGKILL",
          env: { ...process.env, PRIVATEER_ROLE: "agent", PRIVATEER_ACTOR: config.ship },
        }, (error, stdout) => {
          children.delete(child);
          if (error || !live()) return reject(Error("mail probe unavailable"));
          try { resolve(JSON.parse(stdout)); } catch { reject(Error("invalid mail probe")); }
        });
        children.add(child);
      });
    }

    function save() {
      if (!live() || !state || !key) return;
      const record = state, name = key;
      writes = writes.catch(() => {}).then(async () => {
        if (!live()) return;
        const value = structuredClone(record);
        if (JSON.stringify(durable.sessions?.[name]) === JSON.stringify(value)) return;
        await persist(d => {
          if (!live()) return;
          const entries = Object.entries(d.sessions ?? {}).filter(([k, v]) => k !== name
            && k.length <= 4096 && validRecord(v)).slice(-127);
          entries.push([name, value]);
          while (entries.length > 1 && Buffer.byteLength(JSON.stringify(entries)) > 65536) entries.shift();
          d.sessions = Object.fromEntries(entries);
        });
      });
      return writes;
    }

    function selected() {
      const route = ctx.ui.router.current();
      if (route.type !== "session" || !sessionID(route.sessionID)) return;
      const info = ctx.data.session.get(route.sessionID);
      if (!info || info.parentID || !info.location || !isAbsolute(info.location.directory)) return;
      try { if (realpathSync(info.location.directory) !== config.folder) return; } catch { return; }
      return route.sessionID;
    }

    async function owned(id) {
      if (!live() || selected() !== id) return false;
      const presence = await call("ship.presence", { ship: config.ship });
      return live() && selected() === id && presence.attended === true
        && presence.lease?.holder === config.holder && presence.lease?.harness === "opencode-v2";
    }

    function receipt(row, id, through) {
      const value = row?.metadata?.privateer_mail;
      return row?.type === "user" && value?.version === 1 && value.home === config.home
        && value.ship === config.ship && value.session === id
        && sequence(value.through) && value.through <= through ? value : undefined;
    }

    async function snapshot(id, after = 0) {
      const value = await call("mail.pending", { address: config.ship, after_seq: after });
      if (!live() || selected() !== id) return;
      if (value.address !== config.ship || ![value.count, value.actionable, value.through_seq,
          value.after_seq, value.acknowledged_through].every(sequence)
          || value.actionable > value.count || value.through_seq < value.after_seq
          || !Array.isArray(value.headers) || value.headers.length > Math.min(6, value.count))
        throw Error("invalid mailbox snapshot");
      return value;
    }

    async function refresh(id) {
      const mailbox = await snapshot(id);
      if (!mailbox) return;
      const name = JSON.stringify([config.home, config.ship, id]);
      if (current !== id) {
        current = id; key = name;
        const record = durable.sessions?.[name];
        state = validRecord(record) ? { seen: record.seen, paused: record.paused, retry: record.retry === true,
          pending: record.pending && { id: record.pending.id, through: record.pending.through } }
          : { seen: 0, paused: false };
        if (["failed", "interrupted"].includes(ctx.data.session.get(id)?.outcome)) state.paused = true;
        // Corrupt or future watermarks must never hide real messages.
        if (state.seen > mailbox.through_seq) state = { seen: 0, paused: true };
      }
      state.seen = Math.max(state.seen, mailbox.acknowledged_through);
      for (const row of ctx.data.session.message.list(id).slice(-128)) {
        const value = receipt(row, id, mailbox.through_seq);
        if (value) state.seen = Math.max(state.seen, value.through);
      }
      const queued = ctx.data.session.pending.list(id);
      for (const row of queued.slice(-128)) {
        const value = receipt({ type: row.type, metadata: row.payload?.metadata }, id, mailbox.through_seq);
        if (value && value.through > state.seen) {
          state.pending = { id: row.id, through: value.through }; delete state.retry;
        }
      }
      if (state.pending && (!sequence(state.pending.through) || typeof state.pending.id !== "string"
          || state.pending.id.length > 512 || state.pending.through > mailbox.through_seq)) {
        delete state.pending; state.paused = true;
      }
      if (state.pending && state.pending.through > state.seen
          && !queued.some(row => row.id === state.pending.id)) {
        // The promoted receipt may be outside the TUI's current message page.
        // Our own input is bounded header text; inspect that exact ID only.
        const requested = { ...state.pending };
        try {
          const row = await ctx.client.message.get({ sessionID: id, messageID: requested.id }, {
            signal: AbortSignal.any([abort.signal, AbortSignal.timeout(3000)]),
          });
          if (!live() || selected() !== id) return;
          const value = receipt(row, id, mailbox.through_seq);
          if (state.pending?.id === requested.id && row.id === requested.id && value?.through === requested.through)
            state.seen = Math.max(state.seen, value.through);
        } catch { /* A missing/uncertain receipt never consumes mail. */ }
        if (state.pending?.through > state.seen && !state.retry) state.paused = true;
      }
      if (state.pending?.through <= state.seen) { delete state.pending; delete state.retry; }
      await save();
      return { mailbox, queued };
    }

    function notification(value, id) {
      const inbox = [config.cli, "--home", config.home, "mail.inbox", "--json",
        JSON.stringify({ address: config.ship, after_seq: state.seen, limit: 40 })];
      return { id: `msg_${Date.now().toString(16)}${randomBytes(12).toString("hex")}`,
        sessionID: id, delivery: "queue",
        metadata: { privateer_mail: { version: 1, home: config.home, ship: config.ship,
          session: id, through: value.through_seq } },
        text: `${value.count} new ship mailbox arrivals. Read bodies with this argv: ${JSON.stringify(inbox)}. `
          + `Page as needed; reply with correlation IDs and acknowledge only handled messages. `
          + `This notification is not an acknowledgement. Existing charter and production deployment approval rules apply. `
          + `Headers: ${JSON.stringify(value.headers)}` };
    }

    async function deliver() {
      const id = selected();
      if (!id || !await owned(id)) return;
      const refreshed = await refresh(id);
      if (!refreshed || !live() || selected() !== id) return;
      if (state.paused) { status("⚓ mail paused; /privateer-mail resume"); return; }
      if (state.pending && !state.retry) { status("⚓ ship mail queued"); return; }
      if (refreshed.queued.length || ctx.data.session.status(id) !== "idle" || ctx.keymap.mode.current() !== "base") {
        status("⚓ ship mail awaiting idle"); return;
      }
      // Probe after the recovered presentation watermark. A low notice alone
      // never wakes the model; its body remains available in the durable inbox.
      const value = await snapshot(id, state.seen);
      if (value) status(`⚓ ${value.count} new ship mail`);
      if (!value || !live() || selected() !== id || !value.actionable || !await owned(id)
          || state.paused || (state.pending && !state.retry) || ctx.data.session.status(id) !== "idle"
          || ctx.data.session.pending.list(id).length || ctx.keymap.mode.current() !== "base") return;
      const retry = state.retry && state.pending;
      const input = notification(retry ? { ...value, through_seq: retry.through,
        headers: value.headers.filter(header => header.seq <= retry.through) } : value, id), record = state;
      // V2 admission is idempotent for a session + input ID. An uncertain or
      // cancelled offer must reuse its ID and watermark, even if more mail
      // arrived meanwhile; a late old request cannot create a second wake.
      if (retry) input.id = retry.id;
      record.pending = { id: input.id, through: input.metadata.privateer_mail.through };
      delete record.retry;
      try { await save(); } catch {
        if (!retry) delete record.pending; // Nothing was offered to native yet.
        else record.paused = true;
        throw Error("cannot persist native offer");
      }
      if (!live() || selected() !== id || !await owned(id) || record.paused) return;
      try {
        const admitted = await ctx.client.session.prompt(input, {
          signal: AbortSignal.any([abort.signal, AbortSignal.timeout(3000)]),
        });
        if (admitted?.id !== input.id || admitted.sessionID !== id)
          throw Error("unconfirmed native admission");
        // Admission is not a receipt. Only promoted native user messages (or
        // their exact delivered event) advance the presentation watermark.
      } catch {
        if (live() && current === id) { record.paused = true; await save(); }
        status("⚓ native mail delivery uncertain; /privateer-mail resume");
      }
    }

    async function poll() {
      if (!live() || flight) return;
      flight = deliver().catch(() => status("⚓ mail unavailable; inbox preserved"));
      try { await flight; } finally { flight = undefined; }
    }

    if (renderStatus) ctx.ui.slot({ append: "prompt.footer.status", render: () => renderStatus(() => memory.status) });
    ctx.keymap.layer(() => ({ commands: [{
      id: "privateer.mail", title: "Ship mail", palette: true,
      slash: { name: "privateer-mail", arguments: true },
      async run(input = "") {
        if (!live()) return;
        while (live() && flight) await flight;
        if (!live()) return;
        const arg = input.trim().replace(/^\/privateer-mail\s*/, "");
        const operation = (async () => {
          const id = selected();
          if (!id || !await owned(id) || !await refresh(id)) return;
          if (arg === "pause") state.paused = true;
          else if (arg === "resume") {
            state.paused = false;
            // Explicit captain intent may retry an uncertain native offer.
            if (state.pending && !ctx.data.session.pending.list(id).some(row => row.id === state.pending.id))
              state.retry = true;
          }
          await save();
          status(state.paused ? "⚓ mail paused; /privateer-mail resume" : "⚓ ship mail enabled");
        })().catch(() => status("⚓ mail unavailable; inbox preserved"));
        flight = operation;
        try { await operation; } finally { if (flight === operation) flight = undefined; }
        if (arg === "resume") await poll();
      },
    }] }));
    const on = (type, handler) => cleanups.push(ctx.data.on(type, handler));
    for (const type of ["session.execution.failed", "session.execution.interrupted"]) on(type, event => {
      if (!live() || event.data.sessionID !== current || !state) return;
      state.paused = true; void save().catch(() => {});
      status("⚓ mail paused after native stop; /privateer-mail resume");
    });
    on("session.inbox.delivered", event => {
      if (!live() || event.data.sessionID !== current || !state?.pending
          || event.data.inboxID !== state.pending.id) return;
      state.seen = Math.max(state.seen, state.pending.through); delete state.pending;
      void save().catch(() => {});
    });
    timer = setInterval(poll, 5000);
    timer.unref?.();
    return () => {
      active = false; abort.abort(); clearInterval(timer);
      for (const dispose of cleanups) dispose();
      for (const child of children) child.kill("SIGKILL");
    };
  },
};
