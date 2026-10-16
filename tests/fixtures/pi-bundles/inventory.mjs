// Test-only inspector, explicitly approved in a disposable fixture registry.
// No provider registration, model request, ambient config read or live service.
export default function (pi) {
  // A recognized slash command must never start an agent/model turn.
  pi.on("agent_start", () => process.exit(73));
  pi.registerCommand("bundle-inventory", {
    description: "Inspect native resources without inference (fixture only)",
    handler: async (_args, ctx) => {
      const script = process.env.PRIVATEER_FIXTURE_CLIENT;
      const endpoint = process.env.PRIVATEER_FIXTURE_ENDPOINT;
      const result = script ? await pi.exec("python3", [script, endpoint], { timeout: 3000 }) : null;
      const prompt = ctx.getSystemPrompt();
      console.log(JSON.stringify({
        fixture: "bundle-inventory",
        tools: pi.getAllTools().map(t => t.name).sort(),
        commands: pi.getCommands().map(c => c.name).sort(),
        selected: prompt.includes("selected-fixture"),
        unselected: prompt.includes("unselected-fixture"),
        ambient: prompt.includes("ambient-fixture") || prompt.includes("AMBIENT_CONTEXT_SENTINEL"),
        instructionsEager: prompt.includes("Read client.py and run it only when needed."),
        promptChars: prompt.length,
        script: result ? { code: result.code, stdout: result.stdout, stderr: result.stderr } : null,
      }));
    },
  });
}
