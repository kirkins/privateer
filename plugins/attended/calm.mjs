import { isAbsolute } from "node:path";
import { readPreference, writePreference } from "./calm-policy.mjs";
import { WorkingBoat } from "./calm-boat.mjs";
import { installLayouts } from "./calm-layout.mjs";

const SLOT = Symbol.for("privateer.pi.calm");

export function registerCalm(api, host, env = process.env, dependencies = {}) {
  if (env.PRIVATEER_ROLE !== "agent" || !isAbsolute(env.PRIVATEER_HOME ?? "")
      || !(env.PRIVATEER_HOSTED_CREW === "1" || ["pi", "pi-signed"].includes(env.PRIVATEER_ATTENDED_HARNESS))) return;
  globalThis[SLOT]?.dispose();
  const warnings = new Set();
  const state = { home: env.PRIVATEER_HOME, enabled: false, disposed: false, epoch: 0,
    warn(adapter) {
      if (warnings.has(adapter)) return;
      warnings.add(adapter);
      const text = `Privateer /calm: unavailable ${adapter}; that adapter stays stock.`;
      (dependencies.warn ?? console.warn)(text);
      if (state.ctx?.hasUI) state.ctx.ui.notify?.(text, "warning");
    } };
  try { state.enabled = (dependencies.read ?? readPreference)(state.home); }
  catch { state.warn("preference read"); }
  let layouts = installLayouts(host, state);
  let boat = new WorkingBoat(dependencies), running = false, disposed = false;
  const subscriptions = [];
  const on = (event, handler) => subscriptions.push(api.on(event, handler));
  const apply = () => {
    if (!state.disposed && state.enabled && running && state.ctx?.mode === "tui") {
      if (boat.start(state.ctx) === false) state.warn("working boat");
    } else boat.stop();
  };
  const controller = {
    state,
    dispose() {
      if (disposed) return;
      disposed = true;
      state.epoch++;
      running = false;
      boat.stop();
      layouts.dispose();
      for (const unsubscribe of subscriptions) unsubscribe?.();
    },
  };
  globalThis[SLOT] = controller;
  on("session_start", (_event, ctx) => {
    if (disposed) return;
    state.ctx = ctx;
    state.epoch++;
    // Hosts may rebind a retained runtime or create a new extension factory.
    // Both paths must rebuild after session-scoped shutdown cleanup.
    if (state.disposed) { state.disposed = false; layouts = installLayouts(host, state); }
    try { state.enabled = (dependencies.read ?? readPreference)(state.home); }
    catch { state.warn("preference read"); }
    boat.stop(); boat = new WorkingBoat(dependencies);
    running = !ctx.isIdle(); state.ctx = ctx; apply();
  });
  on("agent_start", (_event, ctx) => { state.ctx = ctx; running = true; apply(); });
  // agent_end can be followed by automatic work. Only settle is final, also
  // for abort and provider errors. No command, tool or model execution hook.
  on("agent_settled", () => { running = false; apply(); });
  on("session_shutdown", () => {
    state.epoch++;
    running = false;
    boat.stop();
    layouts.dispose();
  });
  api.registerCommand("calm", { description: "Toggle the home-local conversation-only Pi view",
    handler: async (_args, ctx) => {
      if (state.disposed) return;
      const next = !state.enabled;
      try { (dependencies.write ?? writePreference)(state.home, next); }
      catch { ctx.ui.notify?.("Cannot save /calm preference; choice unchanged.", "error"); return; }
      state.ctx = ctx; state.enabled = next;
      apply(); layouts.redraw();
    } });
  return controller;
}
