# Optional trusted adapters

Privateer core knows tasks, charters, jobs and events. Harnesses and personal provider routes live here, outside the core. Installing an adapter does not enable it: select its manifest explicitly when planning or starting an agent. A charter's optional `provider_plugin` pins `{"manifest":"/absolute/manifest.json","config":"/optional/private/config.json"}`. Omitted call arguments use those paths; different supplied paths are refused. Manifest and private config contents remain trusted operator-controlled files; this is not a cryptographic or network boundary.

The version 1 manifest names an executable argv, bounded timeout and combined stdout/stderr limit. A single request and response use JSON lines. The request contains only the task id, title, repository, owner, effective actions, local control CLI address and lean brief, plus optional adapter options and a private config **path**. Briefs include concrete version 1 task, inbox, message, peer, delegation and handoff command shapes, with current task/owner/home bound. The launched harness runs with `PRIVATEER_ROLE=agent` and `PRIVATEER_ACTOR` set to the task owner, so operator commands are refused to it (see [architecture](../docs/architecture.md#operators-and-agents)). Responses must echo request id and version. `validate` returns `{"valid":true}`. `command` returns `{"argv":[...],"cwd":"...","env_names":[]}`. Environment values and credentials are never protocol fields. Names describe inherited native environment dependencies; Privateer does not copy credentials.

Native adapters plan argv without shell interpolation. They use the user's installed harness and its existing authentication. Validation checks the command plan, not account login or model availability. No model is hardcoded. A plugin is trusted executable code, not a sandbox, and a native harness retains the privileges of its user.

## Native harnesses

Select `plugins/<id>/manifest.json` through the existing plugin interface. Core
task, mailbox, job and activation APIs do not change. The adapter defaults to a
headless run, with native structured output where available. `mode: interactive`
plans a terminal command; it is for a terminal backend, not the current
stdin-detached `agent.start` job path.

| ID | Native CLI | Explicit resume selector | Interactive command |
|---|---|---|---|
| `codex` | `codex` | Full session UUID | Yes |
| `claude` | `claude` | Full session UUID | Yes |
| `pi` | `pi` | Full session UUID or existing absolute session file | Yes |
| `pi-signed` | `pi-signed` | Same as Pi; requires the selected wrapper | Yes |
| `omp` | `omp` | Full session UUID or existing absolute session file | Yes |
| `opencode` | `opencode` | Native `ses_…` ID | Yes |
| `gemini` | `gemini` | Full session UUID | Yes |
| `cursor` | `cursor-agent` | Full chat UUID | Yes |
| `grok` | `grok` | Full session UUID | Yes |
| `agy` | `agy` | Native conversation ID | Yes |
| `muse` | `muse` | Headless resume not qualified | Yes, fresh |
| `devin` | `devin` | Native session ID | Yes |
| `kimi` | `kimi` (current Kimi Code 2.x) | Native session ID | Readiness-gated initial delivery still needed |
| `rovo` | `rovo acp` | Native ACP session ID, requires advertised load support | No |

Common options are `model`, `effort`, `resume` and `mode` (`headless` or
`interactive`). Unsupported options are refused rather than silently dropped.
`effort` uses native flags; OpenCode variants and Codex/Grok effort values are
passed to the native CLI for model-specific validation. Gemini, Cursor, Devin
and Kimi have no independent qualified effort flag here. Pi-family adapters
also accept `provider` with an explicit `model`, plus boolean `ephemeral`.
`resume` never selects the latest session or opens a picker. It supplies the
explicit selector to the native CLI; this alone does not prove that a session
exists, belongs to a particular ship, or has been resumed successfully.
The runtime chooses whether to resume or start fresh and binds sessions to a
focus; an adapter does not choose that policy or share sessions across tasks.

Claude/Grok accept native `permission_mode`; Gemini maps it to `approval-mode`.
No permission override is supplied by default. Claude's optional `bare: true`
selects its native minimal launch mode for an isolated pilot. Pi and signed Pi
also accept `bare: true`: disable extensions, MCP, skills, templates, context
files and themes for this invocation, keeping native provider/authentication
configuration. Cursor accepts
native `sandbox` (`enabled`/`disabled`), boolean `trust` and boolean `force`.
`force: true` maps to native `--force`: allow commands unless the native policy
explicitly denies them. It is omitted by default. Explicit `sandbox: enabled`
adds the current Privateer operational home and the runtime-declared shared
resource ledger through `--add-dir`, allowing durable task/mail writes and job
admission alongside repository work. Gemini's boolean
`trust` selects per-run `--skip-trust`, without modifying the trust store. Devin
accepts native `permission_mode`, boolean `sandbox` and boolean `trust`; its
`autonomous` permission mode requires its sandbox. Devin headless output is
native text, not invented JSON events. Kimi's native `--prompt` selects its
unattended mode; no conflicting `--auto` flag is appended.

The direct native launcher replaces itself with the selected CLI in the same process group.
It clears inherited foreign harness identity markers while preserving
`PRIVATEER_ROLE`, `PRIVATEER_ACTOR`, authentication and provider configuration.
A missing CLI fails without substituting another harness or model.

Unattended Pi/signed Pi JSON runs use an event-checking bridge in that same
job group. Pi may exit zero after exhausted provider retries, so process status
alone cannot confirm a run. The bridge requires an unchanged session identity,
a final assistant `stop`, a non-aborted `agent_settled`, a complete JSONL stream
and a zero native exit. Error, aborted, truncated and incomplete runs fail the
job; native retries may recover before the final settled event. Explicit resume
must match the requested UUID or the existing session file's header. No extra
model retries or provider fallback are introduced. Native events are preserved;
one final summary counts only authoritative completed-message and compaction
usage, excluding repeated streaming snapshots. This confirms native run
completion, not the task's acceptance criteria, test results or mail delivery.

Rovo uses its native ACP interface rather than a guessed positional prompt or
the older `acli rovodev` CLI. Kimi and Devin optionally accept `transport: acp`;
their default remains the native print interface. ACP runs one correlated turn
in the owning job's process group, with bounded messages, queues and deadlines.
Only the exact requested session can be loaded, and only when the server
advertises that capability. `end_turn` is required before reporting completion.
Filesystem and terminal client methods are not advertised or executed. Native
tools still run with the native harness's own privileges.

ACP permission requests default to cancellation. An explicit
`acp_permissions: allow_once` may select an offered one-time grant; it never
selects a persistent grant. Native permission/sandbox options are refused in ACP
mode rather than reinterpreted. Explicit model selection requires advertised
session configuration (or the older advertised models API), and configuration
changes must be confirmed. Unsupported choices fail before the prompt.
Usage remains absent when the server does not report it; no token counts are
estimated. The core remains responsible for cancellation and descendant cleanup.

Inspect supported command options and check installed parsers without inference:

```sh
python3 plugins/native.py --describe
python3 scripts/harness-probe.py
```

The probe invokes help only; it does not establish authentication, live model
availability, workspace readiness, session continuity or working supervision.
Live repair/message qualification is opt-in and uses an isolated home/repository:

```sh
python3 scripts/native-canary.py --binary target/debug/privateer \
  --manifest plugins/codex/manifest.json --check-resume
```

Other adapters can use the same canary with their manifest and native
`--options-json`. It independently checks the repaired tests and task-linked
message. With `--check-resume`, it also requires native startup session evidence,
the same session ID on the second run, and recall of prior conversation context.
The isolated two-job test qualifies native continuity, not runtime focus routing.
`--check-job` additionally requires the tiny fixture's tests to finish in a
recorded light job under the same task. It checks the job's argv, cwd and result;
this does not exercise admission of a heavy build. The canary creates its own
ledger before planning sandbox grants, and rejects changed acceptance tests.
Session-start decoding is currently implemented for Codex, Claude, Pi-family,
OpenCode, Gemini, Cursor, AGY and the Rovo/Kimi/Devin ACP transport; an unknown
format stays unqualified.

This is progress toward the harness set in
[First Mate's configuration reference](https://github.com/kunchenguid/firstmate/blob/08f0a069af2f88911858b44175d225b77362cffe/docs/configuration.md#harness-support),
not full lifecycle parity. The ACP adapter has real pipe/process fixtures for
session identity, permissions, deadlines, cancellation and the existing
activation/mail contract. Native Kimi and Devin initialization has been checked;
live authenticated ACP turns, including Rovo, still require qualification.
Interactive attachment/injection and per-harness supervision remain separate
qualification work; command plans and scripted protocol fixtures do not claim
a harness can already coordinate a ship autonomously. Provider routes remain
private configuration.

Codex options may explicitly choose `sandbox` (`read-only`, `workspace-write`, `danger-full-access`) and boolean `ephemeral`. They are omitted by default, preserving native user configuration. Choosing `workspace-write` adds only the local Privateer operational home with `--add-dir`, so the agent can write its durable task and message records as well as work in the repository. `ephemeral:true` suppresses native Codex session persistence while Privateer retains its job records. This does not change global harness preferences.

## Gateway Pi adapter

`gateway-pi` is an optional integration for people routing Pi through a private gateway. Copy `config.example.json` outside the source checkout, edit its endpoint/provider/model/Pi configuration path, and set mode `0600`. Both adapter config and Pi models file must be private and user-owned. Keep API keys in Pi's private native configuration, never in the gateway adapter config or command options.

The adapter verifies that the chosen Pi file is the active `PI_CODING_AGENT_DIR/models.json` (or `~/.pi/agent/models.json`), its provider uses the exact configured endpoint, and the model exists under that provider. Its launcher repeats these checks immediately before invoking Pi with explicit provider and model. It has no fallback. Endpoint URLs cannot contain credentials, query strings or fragments. Configuration and exceptions are not returned in diagnostic output.

Gateway Pi accepts the same Pi session/mode/effort options, while binding provider
and model to its private route. Conflicting requested selectors are refused;
resuming a session still repeats route validation immediately before launch.

This protects against accidental route selection and configuration changes between planning and invocation. It is **not network enforcement**: trusted Pi extensions, configuration mutation after validation, proxies and custom executables remain outside this guard. A hard egress guarantee requires an external network/credential boundary. Gateway availability and live inference are not established by a command plan.

The examples contain generic placeholder names, no user's endpoint, account, model or keys. Protocol tests use temporary private files and fake native executables.
