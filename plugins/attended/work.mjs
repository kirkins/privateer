// Transport for runtime-owned work offers. Eligibility, budgets and leases live
// in Rust; the adapter only reports native readiness and starts one offered turn.
export const WORK_TYPE = "privateer-work";

export class WorkDelivery {
  constructor(api, config, owned, call) {
    this.api = api; this.config = config; this.owned = owned; this.call = call;
  }
  stop() {
    this.ctx?.ui?.setStatus?.(WORK_TYPE, undefined);
    this.ctx = undefined; this.offer = undefined; this.started = false;
  }
  async poll(ctx, generation, idle, current) {
    if (!this.config.hosted || !current()) return;
    this.ctx = ctx;
    try {
      if (!await this.owned(generation) || !current()) return;
      const usage = ctx.getContextUsage?.();
      const window = usage?.contextWindow ?? ctx.model?.contextWindow;
      const attention = Number.isFinite(usage?.tokens) && window > 0
        ? { tokens: Math.round(usage.tokens), window } : null;
      const response = await this.call("work.poll", {
        ship: this.config.ship, holder: this.config.holder,
        idle: idle && !this.offer && !ctx.ui?.getEditorText?.()?.trim(), attention,
      });
      if (!current() || !await this.owned(generation)) return;
      const offer = response.offer;
      if (!offer) {
        if (response.state === "focused_handoff") ctx.ui?.setStatus?.(WORK_TYPE, "⚓ work handed to a fresh task context");
        else if (response.state === "idle" && !this.offer) ctx.ui?.setStatus?.(WORK_TYPE, undefined);
        return;
      }
      if (this.offer || !idle || !ctx.isIdle()) {
        await this.call("work.release", { ship:this.config.ship,holder:this.config.holder,offer_id:offer.id });
        return;
      }
      if (typeof offer.id !== "string" || typeof offer.task_id !== "string"
          || typeof offer.prompt !== "string" || offer.prompt.length > 24000) throw Error("invalid work offer");
      this.offer = offer;
      this.started = false;
      try {
        this.api.sendMessage({ customType: WORK_TYPE, display: true, content: offer.prompt,
          details: { home: this.config.home, ship: this.config.ship, offerId: offer.id, taskId: offer.task_id, revision: offer.revision },
        }, { triggerTurn:true, deliverAs:"followUp" });
        ctx.ui?.setStatus?.(WORK_TYPE, `⚓ work queued: ${offer.task_id}`);
      } catch (error) {
        this.offer = undefined;
        await this.call("work.release", { ship:this.config.ship,holder:this.config.holder,offer_id:offer.id });
        throw error;
      }
    } catch {
      if (current()) ctx.ui?.setStatus?.(WORK_TYPE, "⚓ work delivery unavailable; durable offer retained");
    }
  }
  async began() {
    if (!this.offer || this.started) return;
    this.started = true;
    this.ctx?.ui?.setStatus?.(WORK_TYPE, `⚓ work active: ${this.offer.task_id}`);
    try {
      await this.call("work.started", { ship:this.config.ship,holder:this.config.holder,offer_id:this.offer.id });
    } catch { /* Do not invent acceptance; the runtime exposes the retained claim. */ }
  }
  async settled() {
    const offer = this.offer;
    if (!offer) return;
    this.offer = undefined; this.started = false;
    this.ctx?.ui?.setStatus?.(WORK_TYPE, undefined);
    try {
      await this.call("work.settled", { ship:this.config.ship,holder:this.config.holder,offer_id:offer.id });
    } catch { /* An ambiguous native stop is reconciled by the durable deadline. */ }
  }
}

export function renderWork(message) {
  return { render(width) {
    return [Array.from(`⚓ scheduled work: ${message.details?.taskId ?? "task"}`).slice(0,Math.max(0,width-2)).join("")];
  }, invalidate() {} };
}
