// A hosted crew member runs interactively in its own pane. Its job ends when
// the harness exits, so the session shuts down once its work settles: with
// status 1 when the last turn failed. An interrupted run (Esc) stays open for
// the captain to steer, and ends at the next settle.
export class HostedCrew {
  constructor(dependencies = {}) {
    this.fail = dependencies.fail ?? (() => process.once("exit", () => process.exit(1)));
    this.lastStop = undefined;
  }

  messageEnded(event) {
    if (event.message?.role === "assistant") this.lastStop = event.message.stopReason;
  }

  settled(event, ctx) {
    const stop = this.lastStop;
    this.lastStop = undefined;
    if (event.aborted === true || stop === "aborted") return;
    if (stop === "error") this.fail();
    ctx.shutdown();
  }
}

export default function crew(api) {
  if (process.env.PRIVATEER_HOSTED_CREW !== "1") return;
  const crew = new HostedCrew();
  api.on("message_end", event => crew.messageEnded(event));
  api.on("agent_settled", (event, ctx) => crew.settled(event, ctx));
}
