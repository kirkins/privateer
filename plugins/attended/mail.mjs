// Native Pi/OMP attended-mail adapter. No provider, credential or model settings.
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { isAbsolute } from "node:path";

const execute = promisify(execFile);
const TYPE = "privateer-mail";
const SLOT = Symbol.for("privateer.attended.mail");

export class AttendedMail {
  constructor(api, config, dependencies = {}) {
    this.api = api;
    this.config = config;
    this.every = dependencies.every ?? setInterval;
    this.cancel = dependencies.cancel ?? clearInterval;
    this.call = dependencies.call ?? (async (command, args) => {
      this.abort = new AbortController();
      const result = await execute(config.cli, ["--home", config.home, command, "--json", JSON.stringify(args)], {
        encoding: "utf8", timeout: 3000, killSignal: "SIGKILL", maxBuffer: 1024 * 1024,
        signal: this.abort.signal,
        env: { ...process.env, PRIVATEER_ROLE: "agent", PRIVATEER_ACTOR: config.ship },
      });
      return JSON.parse(result.stdout);
    });
    this.generation = 0;
    this.disposed = false;
  }

  status(text) {
    if (text === this.statusText) return;
    this.statusText = text;
    this.ctx?.ui?.setStatus?.(TYPE, text);
  }

  stop() {
    this.running = false;
    this.generation++;
    if (this.timer) this.cancel(this.timer);
    this.timer = undefined;
    this.abort?.abort();
    this.status(undefined);
  }

  dispose() { this.stop(); this.disposed = true; }

  start(ctx) {
    this.stop();
    if (this.disposed || !ctx.hasUI) return;
    this.ctx = ctx;
    this.running = true;
    this.seen = 0;
    this.paused = false;
    this.pending = false;
    this.busy = !ctx.isIdle();
    this.lastStop = undefined;
    // Native custom-message receipts survive resume/reload. A message that
    // never reached the session cannot advance this durable watermark.
    for (const entry of ctx.sessionManager.getBranch()) {
      const d = entry.details;
      if (entry.type === "custom_message" && entry.customType === TYPE
          && d?.home === this.config.home && d.ship === this.config.ship
          && Number.isSafeInteger(d.throughSeq) && d.throughSeq >= 0) {
        this.seen = Math.max(this.seen, d.throughSeq);
      }
    }
    this.timer = this.every(() => { void this.poll("wake"); }, 5000);
    this.timer.unref?.();
    void this.poll("wake");
  }

  async owned(generation) {
    const p = await this.call("ship.presence", { ship: this.config.ship });
    return this.running && generation === this.generation && p.attended === true
      && p.lease?.holder === this.config.holder;
  }

  message(snapshot) {
    const inbox = [this.config.cli, "--home", this.config.home, "mail.inbox", "--json",
      JSON.stringify({ address: this.config.ship, after_seq: snapshot.after_seq, limit: 40 })];
    return {
      customType: TYPE, display: true,
      content: `You are the first mate of ${this.config.ship}. ${snapshot.count} new mailbox arrivals through seq ${snapshot.through_seq}. `
        + `This is a notification, not an acknowledgement. Read message bodies with this argv: ${JSON.stringify(inbox)}. `
        + `Page the inbox as needed, handle requests and reply with their correlation IDs; acknowledge only handled mail. `
        + `Existing charter and production-deployment approval rules still apply. Headers: ${JSON.stringify(snapshot.headers)}`,
      details: { home: this.config.home, ship: this.config.ship, throughSeq: snapshot.through_seq, count: snapshot.count },
    };
  }

  async poll(mode = "wake") {
    if (!this.running || this.disposed) return;
    const generation = this.generation;
    if (this.flight) {
      if (mode !== "context") return;
      await this.flight;
    }
    if (!this.running || this.disposed || generation !== this.generation) return;
    const operation = this.deliver(mode, generation).catch(() => {
      if (this.running && generation === this.generation) this.status("⚓ mail unavailable; inbox preserved");
    });
    this.flight = operation;
    try { return await operation; }
    finally { if (this.flight === operation) this.flight = undefined; }
  }

  async deliver(mode, generation) {
    if (!await this.owned(generation)) {
      if (this.running && generation === this.generation) this.status("⚓ mail held by another session");
      return;
    }
    const snapshot = await this.call("mail.pending", { address: this.config.ship, after_seq: this.seen });
    if (!this.running || generation !== this.generation) return;
    if (![snapshot.count, snapshot.actionable, snapshot.through_seq, snapshot.after_seq, snapshot.acknowledged_through]
        .every(n => Number.isSafeInteger(n) && n >= 0) || snapshot.address !== this.config.ship
        || snapshot.through_seq < snapshot.after_seq || snapshot.actionable > snapshot.count
        || !Array.isArray(snapshot.headers) || snapshot.headers.length > Math.min(6, snapshot.count)) {
      throw new Error("invalid mailbox snapshot");
    }
    this.seen = Math.max(this.seen, snapshot.acknowledged_through);
    if (snapshot.count === 0) {
      this.status(this.paused ? "⚓ mail paused; /privateer-mail resume" : "⚓ no new mail");
      return;
    }
    this.status(`⚓ ${snapshot.count} new mail${this.paused ? " • paused" : ""}`);
    if (mode === "wake" && (this.paused || this.busy || this.pending || !this.ctx.isIdle() || snapshot.actionable === 0)) return;
    if (!await this.owned(generation)) return;
    const message = this.message(snapshot);
    if (mode === "context") {
      this.seen = snapshot.through_seq;
      return { message };
    }
    this.pending = true;
    try {
      this.api.sendMessage(message, { triggerTurn: true, deliverAs: "followUp" });
      this.seen = snapshot.through_seq;
      this.status("⚓ mail queued");
    } catch (error) { this.pending = false; throw error; }
  }

  began() { if (!this.running || this.disposed) return; this.busy = true; this.pending = false; this.lastStop = undefined; }
  messageEnded(event) {
    if (!this.running || this.disposed) return;
    if (event.message?.role === "assistant") this.lastStop = event.message.stopReason;
  }
  settled(event) {
    if (!this.running || this.disposed) return;
    this.busy = false;
    this.pending = false;
    if (event.aborted === true || ["aborted", "error"].includes(this.lastStop)) {
      this.paused = true;
      this.status("⚓ mail paused after native stop; /privateer-mail resume");
    }
  }
  ompEnded(event) {
    if (!this.running || this.disposed) return;
    if (event.isTerminal === false || event.yielded === false || event.willContinue === true) return;
    const assistant = event.messages?.findLast?.(m => m.role === "assistant");
    if (assistant) this.lastStop = assistant.stopReason;
    this.settled({});
  }
}

export default function attended(api) {
  const env = process.env;
  const config = { home: env.PRIVATEER_HOME, cli: env.PRIVATEER_CONTROL_CLI,
    ship: env.PRIVATEER_ACTOR, holder: env.PRIVATEER_PRESENCE_HOLDER, harness: env.PRIVATEER_ATTENDED_HARNESS };
  if (env.PRIVATEER_ROLE !== "agent" || !["pi", "pi-signed", "omp"].includes(config.harness)
      || !config.home || !isAbsolute(config.home) || !config.cli || !isAbsolute(config.cli)
      || !config.ship || !config.holder) return;
  globalThis[SLOT]?.dispose();
  const mail = new AttendedMail(api, config);
  globalThis[SLOT] = mail;
  api.on("session_start", (_event, ctx) => mail.start(ctx));
  // Pi emits session_start for replacements. OMP has separate switch/branch
  // events; tree navigation in either host can change the active receipts.
  const rebase = (_event, ctx) => {
    const paused = mail.paused;
    mail.start(ctx);
    mail.paused = paused;
  };
  api.on("session_tree", rebase);
  if (config.harness === "omp") {
    api.on("session_switch", rebase);
    api.on("session_branch", rebase);
  }
  api.on("session_shutdown", () => mail.stop());
  api.on("agent_start", () => mail.began());
  api.on("message_end", event => mail.messageEnded(event));
  api.on(config.harness === "omp" ? "agent_end" : "agent_settled", event => {
    if (config.harness === "omp") mail.ompEnded(event); else mail.settled(event);
  });
  api.on("before_agent_start", async () => { mail.paused = false; return mail.poll("context"); });
  api.registerCommand("privateer-mail", { description: "Pause, resume or inspect attended mail delivery",
    handler: async args => {
      if (args.trim() === "pause") { mail.paused = true; mail.status("⚓ mail paused; /privateer-mail resume"); }
      else if (args.trim() === "resume") { mail.paused = false; await mail.poll(); }
      else mail.status(mail.paused ? "⚓ mail paused; /privateer-mail resume" : "⚓ mail enabled");
    } });
  api.registerMessageRenderer(TYPE, message => ({
    render(width) {
      const text = `⚓ ${message.details?.ship ?? "ship"}: ${message.details?.count ?? "new"} new mail`;
      return [Array.from(text).slice(0, Math.max(0, width - 2)).join("")];
    }, invalidate() {},
  }));
}
