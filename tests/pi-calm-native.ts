// Loaded by an isolated installed Pi, not a separately installed npm copy.
import * as host from "@earendil-works/pi-coding-agent";
import * as tui from "@earendil-works/pi-tui";
import calm from "../plugins/attended/calm.ts";
import { layoutBehavior, queueBehavior } from "./pi-calm.mjs";

export default function probe(api) {
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
      await queueBehavior(host, session);
      console.log(`ok - installed Pi ${host.VERSION}: entry point and real session seams; zero model requests`);
    } catch (error) {
      console.error(error.stack ?? error); process.exitCode = 1;
    } finally { session?.dispose(); ctx.shutdown(); }
  });
}
