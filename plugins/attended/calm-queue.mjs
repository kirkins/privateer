import { operational } from "./calm-policy.mjs";

// Current Pi keeps custom follow-ups in Agent queues, separate from the
// string-only listing. Escape's clearQueue nevertheless clears BOTH. Keep
// exactly Privateer's custom notifications, never reinterpret human text.
export function queueCapability(mode) {
  const s = mode.session, agent = s?.agent;
  try {
    return !!s && typeof mode.updatePendingMessagesDisplay === "function"
      && typeof mode.restoreQueuedMessagesToEditor === "function"
      && typeof s.waitForIdle === "function" && typeof s.isIdle === "boolean"
      && typeof s.isCompacting === "boolean" && typeof mode.showStatus === "function"
      && typeof s.sendCustomMessage === "function" && typeof s.clearQueue === "function"
      && typeof s.abort === "function" && typeof s.sessionManager?.getBranch === "function"
      && Array.isArray(s.sessionManager.getBranch())
      && Array.isArray(mode.compactionQueuedMessages)
      && [agent?.steeringQueue, agent?.followUpQueue].every(q =>
        Array.isArray(q?.messages) && typeof q.enqueue === "function")
      && typeof mode.clearAllQueues === "function";
  } catch { return false; }
}

export function installQueueRetention(prototype, state, patch, touch) {
  const checked = new WeakMap(), flights = new WeakSet();
  const capable = mode => {
    const s = mode.session;
    if (!s || (typeof s !== "object" && typeof s !== "function")) return false;
    if (!checked.has(s)) {
      checked.set(s, queueCapability(mode));
      if (!checked.get(s)) state.warn("queued operational retention (session)");
    }
    return checked.get(s);
  };
  const queues = mode => [mode.session.agent.steeringQueue, mode.session.agent.followUpQueue];
  const hidden = message => operational(message, state.home);
  const resume = mode => {
    if (flights.has(mode.session)) return;
    const s = mode.session, epoch = state.epoch;
    flights.add(s);
    void (async () => {
      // Tree navigation resumes in the idle microtask. Yield a macrotask so
      // the continuation belongs to the navigated branch, not the old one.
      for (;;) {
        await s.waitForIdle();
        await new Promise(resolve => setImmediate(resolve));
        if (state.disposed || state.epoch !== epoch || mode.session !== s) return;
        if (s.isIdle) break;
      }
      if (s.isCompacting || mode.compactionQueuedMessages.length) return;
      const queue = queues(mode).find(q => q.messages.length > 0);
      const first = queue?.messages[0];
      if (!hidden(first)) return; // Native priority/order must not be bypassed.
      queue.messages.shift();
      try {
        const pending = s.sendCustomMessage(first, { triggerTurn: true });
        mode.showStatus?.("Privateer supervision continues in a new turn.");
        await pending;
      } catch (error) {
        // A preparation refusal hasn't delivered the input. Once persisted,
        // a later rendering/handler failure must not create a duplicate turn.
        const delivered = s.sessionManager?.getBranch?.().some(entry =>
          entry.type === "custom_message" && entry.customType === first.customType
          && entry.details?.home === first.details.home && entry.details?.ship === first.details.ship
          && entry.details?.throughSeq === first.details.throughSeq);
        if (!delivered) queue.messages.unshift(first);
        state.warn("queued operational continuation failed; queue preserved");
      }
    })().catch(() => state.warn("queued operational continuation unavailable; queue preserved"))
      .finally(() => flights.delete(s));
  };
  const available = ["updatePendingMessagesDisplay", "restoreQueuedMessagesToEditor", "clearAllQueues"]
    .every(name => typeof prototype?.[name] === "function");
  if (!available) { state.warn("queued operational rows"); return; }
  patch(prototype, "updatePendingMessagesDisplay", original => function(...args) {
    touch(this);
    if (state.enabled && !state.disposed) capable(this);
    return original.apply(this, args); // Custom notifications are not strings in Pi's listing.
  }, "queued operational rows");
  patch(prototype, "restoreQueuedMessagesToEditor", original => function(...args) {
    if (!state.enabled || state.disposed || !capable(this)) return original.apply(this, args);
    const compacting = this.session.isCompacting || this.compactionQueuedMessages.length > 0;
    const held = queues(this).map(q => q.messages.filter(hidden));
    if (!held.some(messages => messages.length)) return original.apply(this, args);
    // Only the restore path changes clearAllQueues. Other callers stay stock.
    const clear = this.clearAllQueues;
    this.clearAllQueues = (...clearArgs) => {
      try { return clear.apply(this, clearArgs); }
      finally {
        queues(this).forEach((q, i) => {
          const retained = new Set(held[i]);
          // queue_update listeners may enqueue synchronously while native
          // clear runs. Earlier retained inputs must stay ahead of arrivals.
          q.messages = [...held[i], ...q.messages.filter(message => !retained.has(message))];
        });
      }
    };
    try { return original.apply(this, args); }
    finally {
      this.clearAllQueues = clear;
      // Compaction owns delivery after its boundary, including when its
      // queued editor inputs have just been returned to the captain.
      if (!compacting) resume(this);
    }
  }, "queued operational retention");
}
