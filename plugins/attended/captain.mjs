import { execFile } from "node:child_process";
import { dirname, isAbsolute } from "node:path";

// The captain's fleet commands in a first mate's live Pi session, like the
// ones a front door offers. Only the person at the keyboard runs a slash
// command; the first mate's model cannot, so these run the Privateer CLI as
// the captain rather than under the ship's agent identity.
const COMMANDS = {
  stow: {
    description: "Stow this machine before an update: /stow, /stow status, /stow resume, /stow resume all",
    argv: rest => ({ "": ["stow"], status: ["stow", "status"], resume: ["stow", "resume"],
      "resume all": ["stow", "resume", "--all"] })[rest],
  },
  yolo: {
    description: "Fleet tool approvals for every first mate: /yolo on, off or status",
    argv: rest => ({ "": ["permissions", "show"], status: ["permissions", "show"],
      on: ["permissions", "yolo"], off: ["permissions", "standard"] })[rest],
  },
  afk: {
    description: "Away: hold the fleet's mail except emergencies, /afk [until 8am] [instructions]",
    argv: rest => (rest === "" ? ["afk"] : ["afk", rest]),
  },
  back: {
    description: "Back from away: a digest now, the liaison's summary next",
    argv: rest => (rest === "" ? ["back"] : undefined),
  },
  ships: {
    description: "The fleet's ships and what each handles",
    argv: rest => (rest === "" ? ["ships"] : undefined),
  },
};

const OUTPUT_BYTES = 256 * 1024;

/// The session's environment without the agent identity it was launched under.
function captain(env) {
  const out = { ...env };
  for (const name of ["PRIVATEER_ROLE", "PRIVATEER_ACTOR", "PRIVATEER_JOB"]) delete out[name];
  return out;
}

export function registerCaptain(api, env = process.env, dependencies = {}) {
  const home = env.PRIVATEER_HOME, cli = env.PRIVATEER_CONTROL_CLI;
  if (env.PRIVATEER_ROLE !== "agent" || !["pi", "pi-signed", "omp"].includes(env.PRIVATEER_ATTENDED_HARNESS)
      || !home || !isAbsolute(home) || !cli || !isAbsolute(cli)) return [];
  const run = dependencies.run ?? (argv => new Promise(resolve => {
    execFile(cli, ["--home", home, ...argv],
      { cwd: dirname(home), env: captain(env), timeout: 30_000, maxBuffer: OUTPUT_BYTES },
      (error, stdout, stderr) => resolve({ ok: !error,
        text: String(error ? (stderr || error.message) : stdout).trim() }));
  }));
  for (const [name, command] of Object.entries(COMMANDS)) {
    api.registerCommand(name, { description: command.description,
      handler: async (args, ctx) => {
        const argv = command.argv(String(args ?? "").trim());
        if (!argv) { ctx.ui.notify?.(`Usage: ${command.description}`, "warning"); return; }
        const result = await run(argv);
        ctx.ui.notify?.(result.text || `/${name} done.`, result.ok ? "info" : "error");
      } });
  }
  return Object.keys(COMMANDS);
}
