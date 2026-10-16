// Quiet compaction for a live first mate. A first mate's session continues on
// one thread, so it eventually compacts; Pi does that when the context is full,
// often in the middle of work. This compacts earlier, at a moment that is
// cheap and safe: the session has been idle past the prompt cache's life, so
// the next turn would pay for the whole context anyway, and nobody is working
// in it. Pi keeps the most recent turns verbatim; the summary is told to keep
// every open captain question; and the ship's own records are attached after.
const TYPE = "privateer-records";
const CHECK_MS = 60_000;
const RECORD_CHARS = 6_000;

export const INSTRUCTIONS = "This is a quiet-period compaction of a Privateer first mate's session. "
  + "In the summary, quote verbatim every question or request from the captain that has not been fully answered, "
  + "and every promise you made that is not yet kept. Name open tasks, threads, pull requests and jobs by their ids. "
  + "Keep decisions and the reasons for them. Leave out tool output and logs: Privateer's records keep them, "
  + "and the ship's bearings and notes are attached after this summary.";

export class QuietCompaction {
  /// `owned` reports whether this session holds the ship's presence lease;
  /// `call` runs a Privateer command as the ship.
  constructor(api, config, owned, call, dependencies = {}) {
    this.api = api;
    this.config = config;
    this.owned = owned;
    this.call = call;
    this.every = dependencies.every ?? setInterval;
    this.cancel = dependencies.cancel ?? clearInterval;
    this.now = dependencies.now ?? Date.now;
  }

  start(ctx) {
    this.stop();
    if (!(this.config.afterMs > 0) || !ctx.hasUI || typeof ctx.compact !== "function"
        || typeof ctx.getContextUsage !== "function") return;
    this.ctx = ctx;
    this.running = true;
    this.busy = !ctx.isIdle();
    this.active = this.now();
    this.compacted = undefined;
    this.timer = this.every(() => { void this.check(); }, CHECK_MS);
    this.timer.unref?.();
  }

  stop() {
    this.running = false;
    if (this.timer) this.cancel(this.timer);
    this.timer = undefined;
  }

  began() { this.busy = true; this.active = this.now(); }
  settled() { this.busy = false; this.active = this.now(); }

  /// Why the session should not compact now, or undefined when it should.
  reason() {
    if (!this.running) return "stopped";
    if (this.flight) return "compacting";
    if (this.busy || !this.ctx.isIdle()) return "working";
    if (this.now() - this.active < this.config.afterMs) return "recently active";
    // Nothing has happened since the last quiet compaction.
    if (this.compacted !== undefined && this.compacted >= this.active) return "already compacted";
    const usage = this.ctx.getContextUsage();
    if (!usage || typeof usage.percent !== "number") return "context size unknown";
    if (usage.percent < this.config.minPercent) return "context small";
    // The captain is part-way through writing to this first mate.
    if (this.ctx.ui?.getEditorText?.()?.trim()) return "draft in editor";
    return undefined;
  }

  async check() {
    if (this.reason() !== undefined) return;
    const before = this.ctx.getContextUsage().percent;
    if (!await this.owned() || this.reason() !== undefined) return;
    const ctx = this.ctx;
    this.flight = true;
    ctx.compact({
      customInstructions: INSTRUCTIONS,
      onComplete: () => {
        this.flight = false;
        this.compacted = this.now();
        void this.attach(ctx, before);
      },
      onError: error => {
        this.flight = false;
        // Wait for new activity before trying again.
        this.compacted = this.now();
        ctx.ui?.notify?.(`Quiet compaction failed: ${error?.message ?? error}`, "warning");
      },
    });
  }

  /// The ship's records, so a vague summary cannot lose an open task,
  /// request or note. Built from records only: no model call.
  async attach(ctx, before) {
    let records = "";
    try {
      const bearings = await this.call("ship.bearings", { ship: this.config.ship });
      records = bearings.text ?? "";
      const captain = await this.call("note.get", { ship: this.config.ship, focus: "captain" });
      const note = captain.notes?.[0]?.body;
      if (note) records += `\nYour captain note:\n${note}\n`;
    } catch {
      records = "Privateer's records could not be read; run ship.bearings before relying on the summary.";
    }
    if (records.length > RECORD_CHARS) records = `${records.slice(0, RECORD_CHARS)}…`;
    const minutes = Math.round((this.now() - this.active) / 60_000);
    this.api.sendMessage({
      customType: TYPE, display: true,
      content: `This session was compacted after ${minutes} quiet minutes. The summary above replaces older turns; `
        + `recent turns are kept as they were. These are ${this.config.ship}'s records now, which outrank the summary `
        + `where they differ:\n${records}`,
      details: { ship: this.config.ship, before, minutes },
    }, { triggerTurn: false });
  }
}

export function renderRecords(message) {
  return {
    render(width) {
      const d = message.details ?? {};
      const text = `⚓ compacted after ${d.minutes ?? "?"} quiet min (was ${Math.round(d.before ?? 0)}% full); ${d.ship ?? "ship"} records attached`;
      return [Array.from(text).slice(0, Math.max(0, width - 2)).join("")];
    }, invalidate() {},
  };
}

export { TYPE as RECORDS_TYPE };
