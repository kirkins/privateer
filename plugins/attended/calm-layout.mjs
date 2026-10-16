// Narrow, independently probed adapters over Pi's exported presentation classes.
// Scoped to mounted interactive components; export renderers are never mounted.
import { assistantView, operational, OWNED_TOOLS } from "./calm-policy.mjs";
import { installQueueRetention } from "./calm-queue.mjs";

export function installLayouts(host, state) {
  const undo = [];
  const mounted = new WeakSet();
  const rawThinking = new WeakMap();
  const modes = new Set();
  const patch = (prototype, name, factory, adapter) => {
    const original = prototype?.[name];
    if (typeof original !== "function") { state.warn(adapter); return false; }
    const wrapper = factory(original);
    prototype[name] = wrapper;
    undo.push(() => { if (prototype[name] === wrapper) prototype[name] = original; });
    return true;
  };
  const active = () => !state.disposed && state.enabled;
  const touch = mode => {
    if (!mode.chatContainer?.addChild || !mode.ui?.requestRender || modes.has(mode)) return;
    modes.add(mode);
    const container = mode.chatContainer;
    const original = container.addChild;
    const wrapper = function(child) {
      mounted.add(child);
      const result = original.call(this, child);
      if (host.AssistantMessageComponent && child instanceof host.AssistantMessageComponent) child.invalidate();
      return result;
    };
    container.addChild = wrapper;
    undo.push(() => { if (container.addChild === wrapper) container.addChild = original; });
  };
  const interactive = host.InteractiveMode?.prototype;
  patch(interactive, "handleEvent", original => function(...args) {
    touch(this);
    return original.apply(this, args);
  }, "mounted transcript");
  patch(interactive, "addMessageToChat", original => function(message, ...args) {
    touch(this);
    if (!operational(message, state.home) || !Array.isArray(this.chatContainer?.children)) {
      return original.call(this, message, ...args);
    }
    const start = this.chatContainer.children.length;
    const result = original.call(this, message, ...args);
    const children = this.chatContainer.children.splice(start);
    // Include any leading spacer, leaving exactly zero rows while hidden.
    this.chatContainer.addChild({
      render: width => active() ? [] : children.flatMap(c => c.render(width)),
      invalidate: () => { for (const child of children) child.invalidate(); },
      setExpanded: expanded => { for (const child of children) child.setExpanded?.(expanded); },
    });
    return result;
  }, "operational rows");
  patch(host.AssistantMessageComponent?.prototype, "updateContent", original => function(message, streaming = this.isStreaming) {
    if (!mounted.has(this)) return original.call(this, message, streaming);
    if (!rawThinking.has(this)) {
      if (!(this.thinkingVisibilityOverrides instanceof Map)) {
        state.warn("assistant thinking state");
        return original.call(this, message, streaming);
      }
      rawThinking.set(this, this.thinkingVisibilityOverrides);
    }
    const overrides = rawThinking.get(this);
    if (!active()) {
      this.thinkingVisibilityOverrides = overrides;
      return original.call(this, message, streaming);
    }
    const view = assistantView(message, streaming, this.hideThinkingBlock, overrides);
    // Pi's click callbacks address presentation run indices. Translate those
    // indices back to the original runs, retaining per-run expansion history.
    const index = i => view.thinkingRuns[i] ?? i;
    this.thinkingVisibilityOverrides = {
      get: i => overrides.get(index(i)),
      set: (i, value) => overrides.set(index(i), value),
      clear: () => overrides.clear(),
    };
    try { return original.call(this, view.message, streaming); }
    finally { this.lastMessage = message; }
  }, "assistant layout");
  patch(host.ToolExecutionComponent?.prototype, "render", original => function(width) {
    if (!mounted.has(this) || !active() || !OWNED_TOOLS.has(this.toolName)) return original.call(this, width);
    if (!Array.isArray(this.imageComponents) || !Array.isArray(this.imageSpacers)) {
      state.warn("tool image layout"); return original.call(this, width);
    }
    return this.imageComponents.flatMap((image, i) => [
      ...(this.imageSpacers[i]?.render(width) ?? []), ...image.render(width),
    ]);
  }, "owned tool shells");
  installQueueRetention(interactive, state, patch, touch);
  return {
    redraw() {
      for (const mode of modes) {
        mode.chatContainer.invalidate();
        mode.updatePendingMessagesDisplay?.();
        mode.ui.requestRender();
      }
    },
    dispose() {
      // Rebuild originals before removing the scoped adapters.
      state.disposed = true;
      for (const mode of modes) mode.chatContainer.invalidate();
      for (const restore of undo.reverse()) restore();
      modes.clear();
    },
  };
}
