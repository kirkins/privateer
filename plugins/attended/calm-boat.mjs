// Fresh Pi drawing using Privateer's dashboard boat geometry. One clock,
// quarter-cell water ticks (220ms), whole-cell hull steps (880ms).
const paint = (code, text) => text ? `\x1b[${code}m${text}\x1b[39m` : "";
const smooth = x => x * x * (3 - 2 * x);

export class Boat {
  column = 0;
  direction = 1;
  tick = 0;
  width = 0;

  advance() {
    this.tick++;
    if (this.tick % 4 || this.width < 5) return;
    const span = this.width - 5;
    if (this.column >= span) this.direction = -1;
    if (this.column <= 0) this.direction = 1;
    this.column = Math.max(0, Math.min(span, this.column + this.direction));
    if (this.column === span) this.direction = -1;
    if (this.column === 0) this.direction = 1;
  }

  freeze() {
    if (this.rendered) Object.assign(this, this.rendered);
  }

  frame(width) {
    this.width = Math.max(0, Math.floor(width));
    this.column = Math.min(this.column, Math.max(0, this.width - 5));
    this.rendered = { width: this.width, column: this.column, direction: this.direction, tick: this.tick };
    if (this.width < 5) {
      const sprite = "◿│◣".slice(0, this.width);
      return [paint(33, sprite) + paint(34, "▁".repeat(this.width - sprite.length))];
    }
    const center = this.column + 2;
    // Deterministically varied 9..13-cell half waves beyond a broad trough.
    const ends = [0];
    for (let n = 0; ends.at(-1) < this.width + 16; n++) {
      const hash = Math.imul(n + 17, 1103515245) >>> 0;
      ends.push(ends.at(-1) + 9 + hash % 5);
    }
    const wave = Array.from({ length: this.width }, (_, i) => {
      const distance = Math.max(0, Math.abs(i - center - this.direction * (this.tick % 4) / 4) - 5);
      let half = 0;
      while (ends[half + 1] < distance) half++;
      const phase = (distance - ends[half]) / (ends[half + 1] - ends[half]);
      const height = smooth(half % 2 ? 1 - phase : phase);
      return "▁▂▃▄"[Math.min(3, Math.floor(height * 4))];
    });
    return [
      " ".repeat(this.column + 1) + paint(33, "◿│◣") + " ".repeat(this.width - this.column - 4),
      paint(34, wave.slice(0, this.column).join("")) + paint(33, "╲▁▁▁╱")
        + paint(34, wave.slice(this.column + 5).join("")),
    ];
  }
}

export class WorkingBoat {
  constructor(dependencies = {}) {
    this.boat = new Boat();
    this.every = dependencies.every ?? setInterval;
    this.cancel = dependencies.cancel ?? clearInterval;
  }
  start(ctx) {
    if (this.component || ctx.mode !== "tui") return;
    if (typeof ctx.ui.setWorkingVisible !== "function" || typeof ctx.ui.setWidget !== "function") return false;
    this.ctx = ctx;
    const component = { render: width => this.boat.frame(width), invalidate() {},
      dispose: () => {
        if (this.timer !== undefined) this.cancel(this.timer);
        this.timer = undefined;
        this.boat.freeze();
        if (this.component === component) this.component = undefined;
      } };
    this.component = component;
    try {
      ctx.ui.setWidget("calm-working-ship", tui => {
        this.timer = this.every(() => { this.boat.advance(); tui.requestRender(); }, 220);
        this.timer.unref?.();
        return component;
      });
      ctx.ui.setWorkingVisible(false);
      this.hiddenStock = true;
    } catch (error) { this.stop(); throw error; }
    return true;
  }
  stop() {
    const component = this.component;
    if (component) {
      // A replacement under the same widget key disposes us first. Do not
      // clear a different extension's replacement during later cleanup.
      component.dispose();
      this.ctx.ui.setWidget("calm-working-ship", undefined);
    }
    if (this.hiddenStock) this.ctx.ui.setWorkingVisible(true);
    this.hiddenStock = false;
  }
}
