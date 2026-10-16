// Follow-through for a live first mate. A Pi turn ends as soon as the model
// replies without a tool call, so "I'll carry on with X" can be the last thing
// it does, leaving X marked running with nothing on it. Before the run
// settles, this asks Privateer for running tasks that nothing is moving and,
// when there are some, gives the first mate one more step to continue them or
// say what they wait on. It nudges once per distinct set, so it cannot loop.
const TYPE = "privateer-follow-through";

export class FollowThrough {
  /// `owned` reports whether this session holds the ship's presence lease;
  /// `call` runs a Privateer command as the ship.
  constructor(config, owned, call) {
    this.config = config;
    this.owned = owned;
    this.call = call;
    this.nudged = "";
  }

  async beforeSettle(event) {
    // An interrupted or failed run is the captain's or the provider's call.
    if (event.outcome !== "completed") return undefined;
    let idle;
    try {
      if (!await this.owned()) return undefined;
      idle = (await this.call("ship.bearings", { ship: this.config.ship })).idle_tasks;
    } catch { return undefined; }
    if (!Array.isArray(idle) || idle.length === 0) { this.nudged = ""; return undefined; }
    const key = idle.map(t => t.id).sort().join(" ");
    if (key === this.nudged) return undefined;
    this.nudged = key;
    const list = idle.map(t => `- ${t.id}: ${t.title}`).join("\n");
    return {
      entries: [...(event.entries ?? []), {
        type: "custom_message", customType: TYPE, display: true,
        details: { ship: this.config.ship, tasks: idle.map(t => t.id) },
        content: `Before you stop: these tasks are marked running, but no job, crew or open request is moving them:\n${list}\n`
          + "Continue each one now (start its crew, its check or its next step), or record why it waits: "
          + "set it blocked with task.update, or pin what wakes it with note.pin "
          + `{"ship":"${this.config.ship}","focus":"task:<id>","key":"waiting","value":"..."}. `
          + "Do not end on a promise to do it later.",
      }],
      continue: true,
    };
  }
}

export function renderFollowThrough(message) {
  return {
    render(width) {
      const tasks = message.details?.tasks ?? [];
      const text = `⚓ follow-through: ${tasks.length} running task${tasks.length === 1 ? "" : "s"} had nothing moving them`;
      return [Array.from(text).slice(0, Math.max(0, width - 2)).join("")];
    }, invalidate() {},
  };
}

export { TYPE as FOLLOW_TYPE };
