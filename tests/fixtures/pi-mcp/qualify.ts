// Qualification fixture only. Uses the installed native SDK/factories, not a connector.
import {
  createAgentSession, createCodemodeExtension, createMcpExtension,
  createToolSearchExtension, DefaultResourceLoader, ModelRuntime,
  SessionManager, SettingsManager,
} from "@earendil-works/pi-coding-agent";
import fs from "node:fs";
import assert from "node:assert/strict";

const sleep = (ms) => new Promise(resolve => setTimeout(resolve, ms));

async function qualify() {
  const input = JSON.parse(fs.readFileSync(process.env.PRIVATEER_MCP_INPUT, "utf8"));
  const errors = [], calls = [], results = [];
  let loads = 0, callSeq = 0, conflictRejected = false, factoryAliasesAccepted = false;
  const settingsManager = SettingsManager.inMemory({
    packages: [], defaultTools: ["codemode", "tool_search"],
    compaction: {enabled: false}, retry: {enabled: false}, cacheWarming: "off",
  });
  const factory = createMcpExtension({
    // This callback is synchronous and returns loaded entries, NOT mcp.json.
    loadConfig: (ctx) => {
      loads++;
      assert.equal(ctx.cwd, process.cwd());
      if (input.operation === "load-failure") throw new Error("fixture-load-failed");
      return {servers: input.servers, errors: [], autoEnableCodemode: true};
    },
    logPath: input.logPath,
    updateConfig: () => { throw new Error("fixture-config-write-forbidden"); },
    openUrl: () => { throw new Error("fixture-browser-forbidden"); },
  });
  const loader = new DefaultResourceLoader({
    cwd: process.cwd(), agentDir: process.env.PI_CODING_AGENT_DIR, settingsManager,
    noExtensions: true, noSkills: true, noPromptTemplates: true,
    noContextFiles: true, noThemes: true,
    extensionFactories: [
      (pi) => {
        if (input.operation === "factory-server-conflict") {
          // Pi defers factory-time registry changes: alias checks do not see
          // each other yet. Qualify this limitation without starting either fake.
          pi.registerMcpServer("conflict-a", input.conflictConfig);
          pi.registerMcpServer("conflict_a", input.conflictConfig);
        }
        pi.on("session_start", () => {
          if (input.operation === "factory-server-conflict") {
            assert.equal(pi.getMcpServers().length, 2);
            pi.unregisterMcpServer("conflict-a");
            pi.unregisterMcpServer("conflict_a");
            factoryAliasesAccepted = true;
          } else if (input.operation === "server-conflict") {
            pi.registerMcpServer("conflict-a", input.conflictConfig);
            assert.throws(() => pi.registerMcpServer("conflict_a", input.conflictConfig), /conflicts/);
            pi.unregisterMcpServer("conflict-a");
            conflictRejected = true;
          }
          assert.deepEqual(pi.getMcpServers(), []);
        });
        pi.on("agent_start", () => process.exit(73));
        pi.on("tool_call", event => {
          calls.push(event.toolName);
          if (event.input?.value === "deny") return {block: true, reason: "fixture-policy-denied"};
        });
        pi.on("tool_result", event => { results.push(event.toolName); });
      },
      createCodemodeExtension({models: false}), createToolSearchExtension(), factory,
    ],
  });
  await loader.reload();
  assert.equal(loads, 0, "factory load must not start MCP");
  assert.deepEqual(loader.getExtensions().errors, []);
  const modelRuntime = await ModelRuntime.create({
    authPath: input.authPath, modelsPath: input.modelsPath,
  });
  const {session} = await createAgentSession({
    cwd: process.cwd(), agentDir: process.env.PI_CODING_AGENT_DIR,
    settingsManager, modelRuntime, resourceLoader: loader,
    sessionManager: SessionManager.inMemory(process.cwd()),
  });
  const inventory = () => session.getAllTools().filter(t => t.name.startsWith("mcp__"))
    .map(t => ({name:t.name, exposure:t.exposure, parameters:t.parameters, annotations:t.annotations}));
  const execute = async (name, args, signal) => {
    const tool = session.agent.state.tools.find(t => t.name === name);
    assert.ok(tool, `native active tool ${name}`);
    // Native nested execution requires an issuing assistant message. Seed a
    // synthetic zero-usage message through SessionManager (not a provider), then
    // invoke Pi's bound tool wrapper. No prompt, model route or stream is used.
    const id = `offline-fixture-${++callSeq}`;
    session.sessionManager.appendMessage({
      role:"assistant", content:[{type:"toolCall", id, name, arguments:args}],
      api:"fixture", provider:"fixture", model:"no-inference",
      usage:{input:0, output:0, cacheRead:0, cacheWrite:0, totalTokens:0,
        cost:{input:0, output:0, cacheRead:0, cacheWrite:0, total:0}},
      stopReason:"toolUse", timestamp:Date.now(),
    });
    session.refreshContext();
    return tool.execute(id, args, signal);
  };
  const script = async (code, signal) => {
    const result = await execute("codemode", {code}, signal);
    const text = result.content.filter(c => c.type === "text").map(c => c.text).join("\n");
    const match = /QUALIFIED:([^\n]+)/.exec(text);
    assert.ok(match, text);
    return JSON.parse(match[1]);
  };
  let report;
  try {
    assert.deepEqual(inventory(), []);
    await session.bindExtensions({onError: e => errors.push({event:e.event, error:e.error})});
    assert.equal(loads, 1);
    if (input.operation === "load-failure") {
      assert.ok(errors.some(e => e.error.includes("fixture-load-failed")));
      assert.deepEqual(inventory(), []);
      report = {loadFailureReported: true, bindReturned: true};
    } else if (input.operation === "shutdown-starting") {
      // Give native transport time to start, but not time to finish initialization.
      await sleep(250);
      report = {starting: true};
    } else if (input.servers.length === 0) {
      await sleep(150);
      assert.deepEqual(inventory(), []);
      report = {empty: true, conflictRejected, factoryAliasesAccepted};
    } else {
      const deadline = Date.now() + 5000;
      while (inventory().length === 0 && Date.now() < deadline) await sleep(20);
      if (input.operation === "disconnected") {
        assert.deepEqual(inventory(), []);
        report = {disconnected: true};
      } else {
        const tools = inventory();
        assert.equal(tools.length, 4);
        assert.equal(new Set(tools.map(t => t.name)).size, 4, "collisions must be disambiguated");
        assert.ok(tools.every(t => t.exposure !== "direct"));
        assert.ok(tools.every(t => t.annotations.readOnlyHint === true));
        const name = tools.find(t => t.name.endsWith("__echo")).name;
        const slow = tools.find(t => t.name.endsWith("__slow")).name;
        const declaration = session.agent.state.tools.find(t => t.name === "codemode").description;
        assert.ok(!declaration.includes("SCHEMA_SENTINEL_9381"));
        assert.ok(!session.getActiveToolNames().includes(name));
        const echo = await script(`
          const schema = await describeTool(${JSON.stringify(name)});
          const r = await tools.${name}({value:"hello"});
          text("QUALIFIED:" + JSON.stringify({schema: schema.includes("SCHEMA_SENTINEL_9381"), data:r.structuredContent}));
        `);
        assert.deepEqual(echo, {schema:true, data:{ok:true, identity:input.identity, value:"hello"}});
        const refused = await script(`
          let blocked=false, invalid=false;
          try { await tools.${name}({value:"deny"}); } catch(e) { blocked=String(e).includes("fixture-policy-denied"); }
          try { await tools.${name}({}); } catch(e) { invalid=true; }
          text("QUALIFIED:" + JSON.stringify({blocked,invalid}));
        `);
        assert.deepEqual(refused, {blocked:true, invalid:true});
        let interrupted;
        if (input.operation === "timeout" || input.operation === "cancel") {
          const controller = new AbortController();
          const timer = input.operation === "cancel" ? setTimeout(() => controller.abort(), 100) : undefined;
          const started = Date.now();
          try {
            interrupted = await script(`
              let failed=false, error="";
              try { await tools.${slow}({value:"wait"}); } catch(e) { failed=true; error=String(e); }
              text("QUALIFIED:" + JSON.stringify({failed,error}));
            `, controller.signal);
            assert.equal(interrupted.failed, true);
            if (input.operation === "timeout") assert.match(interrupted.error, /timeout|timed out/i);
          } catch (error) {
            assert.equal(input.operation, "cancel");
            interrupted = {cancelled:true};
          } finally { clearTimeout(timer); }
          assert.ok(Date.now() - started < 3000, "bounded interrupt completion");
        }
        await execute("tool_search", {query:"echo", limit:8});
        assert.ok(session.getActiveToolNames().includes(name), "search must load deferred schema");
        assert.ok(calls.includes(name) && results.includes(name), "nested MCP pipeline hooks");
        report = {echo, refused, interrupted, lazySchema:true, inventory:tools,
                  activeAfterSearch:session.getActiveToolNames().filter(n => n.startsWith("mcp__")),
                  calls, results};
      }
    }
  } finally {
    // SDK dispose invalidates contexts but does not itself emit session_shutdown.
    // Emit the real native lifecycle event, twice to test idempotency, then dispose.
    await session.extensionRunner.emit({type:"session_shutdown", reason:"exit"});
    await session.extensionRunner.emit({type:"session_shutdown", reason:"exit"});
    session.dispose();
  }
  console.log(JSON.stringify({fixture:"mcp-qualify", version:"1.2.0", loads, errors,
    factory:typeof factory, cleanupTwice:true, ...report}));
}

export default function (pi) {
  pi.on("agent_start", () => process.exit(73));
  pi.registerCommand("mcp-qualify", {
    description:"Inspect/call authless MCP through installed Pi without inference",
    handler: qualify,
  });
}
