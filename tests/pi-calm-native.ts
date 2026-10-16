// Loaded by an isolated installed Pi, not a separately installed npm copy.
import * as host from "@earendil-works/pi-coding-agent";
import * as tui from "@earendil-works/pi-tui";
import calm from "../plugins/attended/calm.ts";
import { layoutBehavior, queueBehavior } from "./pi-calm.mjs";

export default function probe(api) {
  // Independent inference refusal, not merely an assertion that this fixture
  // happened not to prompt. Applies to the CLI session and local probe session;
  // its queue-delivery behavior deliberately replaces sendCustomMessage.
  let requests = 0;
  const refuse = () => { requests++; throw new Error("fixture inference refused"); };
  if (typeof host.AgentSession?.prototype?.prompt !== "function"
      || typeof host.AgentSession?.prototype?.sendCustomMessage !== "function") {
    throw new Error("cannot install fixture inference guard");
  }
  host.AgentSession.prototype.prompt = refuse;
  host.AgentSession.prototype.sendCustomMessage = refuse;
  api.on("before_agent_start", refuse);
  api.on("cache_warming_decision", () => ({ action: "stop" }));
  if (process.env.PRIVATEER_CALM_LAUNCH) {
    api.on("session_start", (_event, ctx) => {
      try {
        const commands = api.getCommands().map(command => command.name);
        const expected = process.env.PRIVATEER_CALM_LAUNCH !== "headless";
        if (commands.includes("calm") !== expected) throw new Error("launch Calm inventory changed");
        if (requests !== 0) throw new Error("launch attempted inference");
        console.log(JSON.stringify({ launch: process.env.PRIVATEER_CALM_LAUNCH,
          calm: commands.includes("calm"), home: process.env.PRIVATEER_HOME,
          actor: process.env.PRIVATEER_ACTOR, role: process.env.PRIVATEER_ROLE,
          agentDir: process.env.PI_CODING_AGENT_DIR, cwd: ctx.cwd }));
      } catch (error) { console.error(error.stack ?? error); process.exitCode = 1; }
      finally { ctx.shutdown(); }
    });
    return;
  }
  // Verify the actual shipped entry-point/import seam without retaining its
  // adapters alongside the behavioral fixtures below.
  const controller = calm(api);
  if (!controller || controller.state.enabled) throw new Error("Shipped /calm entry must register default-off");
  controller.dispose();
  api.on("session_start", async (_event, ctx) => {
    let session;
    try {
      await layoutBehavior(host, tui);
      const resourceLoader = {
        getExtensions: () => ({ extensions: [], errors: [], runtime: host.createExtensionRuntime() }),
        getSkills: () => ({ skills: [], diagnostics: [] }),
        getPrompts: () => ({ prompts: [], diagnostics: [] }),
        getThemes: () => ({ themes: [], diagnostics: [] }),
        getAgentsFiles: () => ({ agentsFiles: [] }),
        getSystemPrompt: () => "Local queue fixture, never prompted.",
        getSystemPromptSource: () => undefined,
        getAppendSystemPrompt: () => [], getAppendSystemPromptSources: () => [],
        extendResources() {}, async reload() {},
      };
      ({ session } = await host.createAgentSession({
        cwd: process.cwd(), agentDir: process.env.PI_CODING_AGENT_DIR,
        resourceLoader, noTools: true,
        sessionManager: host.SessionManager.inMemory(process.cwd()),
        settingsManager: host.SettingsManager.inMemory({ compaction: { enabled: false } }),
      }));
      session.agent.streamFn = refuse;
      await queueBehavior(host, session);
      if (requests !== 0) throw new Error(`fixture attempted ${requests} model requests`);
      console.log(`ok - installed Pi ${host.VERSION}: entry point and real session seams; zero model requests; transport refused`);
    } catch (error) {
      console.error(error.stack ?? error); process.exitCode = 1;
    } finally { session?.dispose(); ctx.shutdown(); }
  });
}
