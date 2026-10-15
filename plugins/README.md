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
| `omp` | `omp` | Full session UUID or existing absolute session file, verified through RPC | Yes, fresh |
| `opencode` | `opencode` | Native `ses_…` ID | Yes |
| `gemini` | `gemini` | Full session UUID | Yes |
| `cursor` | `cursor-agent` | Full chat UUID | Yes |
| `grok` | `grok agent --no-leader stdio` (ACP for headless) | Full session UUID | Yes, native TUI plan |
| `agy` | `agy` | Native conversation ID | Yes |
| `muse` | `muse` | Full session UUID with `transport: msp` | Yes, fresh |
| `devin` | `devin` | Native session ID | Yes |
| `kimi` | `kimi` (current Kimi Code 2.x) | Native session ID | Readiness-gated initial delivery still needed |
| `rovo` | `rovo acp` | Native ACP session ID, requires advertised load support | No |

Common options are `model`, `effort`, `resume` and `mode` (`headless` or
`interactive`). Unsupported options are refused rather than silently dropped.
`effort` uses native flags; OpenCode variants and Codex effort values are
passed to the native CLI for model-specific validation. Grok headless choices
require advertised ACP configuration. Gemini, Cursor, Devin
and Kimi have no independent qualified effort flag here. Pi-family adapters
also accept `provider` with an explicit `model`, plus boolean `ephemeral`.
`resume` never selects the latest session or opens a picker. It supplies the
explicit selector to the native CLI; this alone does not prove that a session
exists, belongs to a particular ship, or has been resumed successfully.
The runtime chooses whether to resume or start fresh and binds sessions to a
focus; an adapter does not choose that policy or share sessions across tasks.

Claude and Grok's interactive plans accept native `permission_mode`; Gemini maps it to `approval-mode`.
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

AGY print runs also use the bounded event-checking bridge. Native 1.3.4 uses
`event: init` with `conversation_id`, followed by `step_update` and a terminal
`result`. The bridge verifies conversation, cwd and any explicit model, requires
the matching result's `SUCCESS`, complete JSONL and native exit zero, and counts
only final aggregate usage. Failed, cancelled, mixed-conversation and incomplete
runs cannot become successful jobs. AGY options include `permission_mode`
(`accept-edits`, `plan`), boolean `skip_permissions`, boolean `sandbox` and
headless-only `disable_slash_commands`. Existing operational home and ledger
directories are passed with `--add-dir`; no trust/provider store is edited.

OMP headless runs use its native RPC interface (`--mode rpc --no-ui`) by
default. The owned host is checked before the assignment prompt: an exact
session, no queued/background work or active goal, and confirmed explicit
model/provider/fixed-effort selections. Native `effort: auto` remains adaptive:
the CLI startup flag is required, while RPC reports a current concrete tier.
Live adaptive behavior still requires model qualification. Resume also verifies the native file's header
identity and recorded repository. OMP can otherwise switch to a resumed
conversation's repository; passing a working directory alone does not prevent
that. The header reader accepts the current native title-slot prefix with a
bounded two-record read. It never searches arbitrary transcript entries.

The RPC bridge correlates admission and `prompt_result`, requires completed
agent work, waits for quiescence, checks final state and the last assistant
stop, then confirms clean host shutdown. A yielded foreground turn with pending
background work is insufficient. Failed, aborted, local-only, truncated,
mis-correlated and unconfirmed turns fail without adding provider retries.
Only authoritative completed-message and compaction usage is counted; repeated
snapshots stay nested events. Ephemeral turns do not advertise a durable session
for later focus resumes. See the [native OMP 18.8.9 protocol](https://github.com/can1357/oh-my-pi/blob/v18.8.9/docs/rpc.md).

OMP also accepts native `permission_mode` (`always-ask`, `write`, `yolo`),
omitted by default, and boolean `bare`. Bare disables extension, skill and rule
discovery, automatic titles, LSP and PTY tools for the invocation. Existing
provider/authentication settings remain native. A tracked per-run overlay
disables implicit latest-session resume, startup plan mode and automatic
prewalk model handoff. The repository and existing operational home/ledger
directories are explicit workspace roots. No personal config file is written.
`transport: native` explicitly selects a fresh print run; interactive plans are
fresh native commands. Resume requires the RPC preflight. Native extensions
remain trusted code and may have their own startup effects; preflight gates
the assignment prompt, not arbitrary extension behavior.

Rovo uses its native ACP interface rather than a guessed positional prompt or
the older `acli rovodev` CLI. Kimi and Devin optionally accept `transport: acp`;
their default remains the native print interface. ACP runs one correlated turn
in the owning job's process group, with bounded messages, queues and deadlines.
Only the exact requested session can be loaded, and only when the server
advertises that capability. `end_turn` is required before reporting completion.
Filesystem and terminal client methods are not advertised or executed. Native
tools still run with the native harness's own privileges.

Grok headless jobs use ACP through an owned `grok agent --no-leader stdio`
host. They never attach to or stop a shared leader. Native session loading and
reported cwd are checked, and `model`/`effort` require advertised choices and
confirmed configuration. Grok's native permission mode remains an interactive
option; ACP uses the one-time permission policy below. Explicit headless
`transport: native` is refused because shared-leader payload ownership is not
qualified. Closing an owned ACP host first sends stdin EOF, then gives graceful
signal teardown a bounded chance before killing an unresponsive child.

ACP permission requests default to cancellation. An explicit
`acp_permissions: allow_once` may select an offered one-time grant; it never
selects a persistent grant. Native permission/sandbox options are refused in ACP
mode rather than reinterpreted. Explicit model selection requires advertised
session configuration (or the older advertised models API), and configuration
changes must be confirmed. Unsupported choices fail before the prompt.
Usage remains absent when the server does not report it; no token counts are
estimated. The core remains responsible for cancellation and descendant cleanup.

Muse optionally accepts `transport: msp` for its native MSP v1 stdio host
(`muse serve`). The native exec interface remains the default. MSP requires
durable sessions, exact UUID resume, an idle session in the same repository,
and no pending interaction. A refused resume never creates a replacement.
The bridge waits for the accepted turn's `turn/completed` with terminal
`completed`, then confirms a clean host exit. An acceptance reply, foreground
completion, or a different turn cannot report success. Messages, startup
events, approval queues and deadlines are bounded; the host shares the owning
job's process group. The runtime's existing focus routing can bind the reported
Muse identity without changing its orchestration policy.

MSP options include native `provider` (`echo`, `meta`, `local`), `model`,
per-turn `effort`, boolean `trust`, boolean `disable_sandbox` and native
`sandbox_network` (`restricted`, `enabled`, `proxy-only`). Existing repository,
operational home and shared-ledger directories become explicit workspace
roots; these roots do not themselves grant project trust. Explicit model,
provider and approval-mode choices must be confirmed before submitting input.
`msp_approval_mode` selects a native preconfigured mode (`allowAll`,
`promptUnmatched`, `onRequest`, `denyUnmatched`) for this session. It is omitted
by default. Tool requests default to denial; `msp_permissions: allow_once`
selects only an offered one-time approval and sends its exact requirement
reference. The presentation receipt never grants permission. Persistent grants,
stale decisions and unsupported client tool requests do not become fallbacks.
Only terminal aggregate usage is counted, when present; cumulative snapshots
remain nested native events.

Muse's native exec/interactive plans accept `provider` (`echo`, `meta`),
`permission_mode` (`untrusted`, `on-request`, `never`), `trust`,
`disable_sandbox` and `sandbox_network`. Exec-only `bare: true` disables
foreign personal context, reminders and web tools for that run. These exec
controls are not reinterpreted as MSP approval options. No global settings,
provider endpoints or credentials are changed by either transport.

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
OpenCode, Gemini, Cursor, AGY, Muse MSP and the Rovo/Kimi/Devin/Grok ACP transport; an unknown
format stays unqualified.

This is progress toward the harness set in
[First Mate's configuration reference](https://github.com/kunchenguid/firstmate/blob/08f0a069af2f88911858b44175d225b77362cffe/docs/configuration.md#harness-support),
not full lifecycle parity. The ACP adapter has real pipe/process fixtures for
session identity, permissions, deadlines, cancellation and the existing
activation/mail contract. Native Kimi and Devin initialization has been checked;
live authenticated ACP turns, including Rovo, still require qualification.
Grok 1.0.50 passed native initialization, fresh session, exact load and confirmed
model/effort configuration with its owned host. The live repair attempt reached
HTTP 429; it was not retried. Live Grok repair, mail, recorded jobs and recall
remain unqualified. AGY 1.3.4 with an existing signed-in account passed live
repair, independent unchanged tests, task-linked mail, a recorded light test
job, exact conversation resume and recall with the result guard enabled.
The resumed tool trace was checked for prior-record searches. This qualifies
that headless path, not every model, sandbox profile or interactive attachment.
OMP 18.8.9 native RPC fresh-state, exact resume and cross-repository refusal
have passed without sending a prompt. Protocol fixtures cover quiescence,
usage, role enforcement, focused activation/mail and descendant cancellation.
The Command Code repair attempt selected the requested model but returned
insufficient credits; no further attempt was made on that account. Live OMP
repair, tool results, task mail, recorded jobs and recall remain unqualified.
Muse 1.4.4 has passed real native echo creation, exact resume and writer-lease
refusal without a replacement session, plus protocol
fixtures for focused activation, mail, role enforcement and descendant
cancellation. Echo does not prove model/tool autonomy. The authenticated Meta
repair canary currently fails with HTTP 402 billing verification; live Muse
repair, task mail, recorded jobs and conversation recall remain unqualified.
Interactive attachment/injection and per-harness supervision remain separate
qualification work; command plans and scripted protocol fixtures do not claim
a harness can already coordinate a ship autonomously. Provider routes remain
private configuration.

Codex options may explicitly choose `sandbox` (`read-only`, `workspace-write`, `danger-full-access`) and boolean `ephemeral`. They are omitted by default, preserving native user configuration. Choosing `workspace-write` adds only the local Privateer operational home with `--add-dir`, so the agent can write its durable task and message records as well as work in the repository. `ephemeral:true` suppresses native Codex session persistence while Privateer retains its job records. This does not change global harness preferences.

## Attended native sessions

`privateer start pi`, `privateer start pi-signed` and `privateer start omp`
explicitly load the optional `attended/mail.ts` extension. Bare `privateer`
does the same when that ship selects one of these harnesses. Native options
follow `--`, for example `privateer start pi -- --model provider/model`.
The selected CLI must be installed; signed Pi never falls back to ordinary Pi.
Native credentials, provider routes and extension configuration stay unchanged.
Starting the native CLI directly does not install this integration.

One compact notification covers a mailbox arrival burst, with at most six
headers and no bodies. Full mail stays in the durable inbox. Polls are bounded
and single flight; busy sessions show a footer count instead of queuing turns.
New actionable mail can wake an idle session; low-priority notices wait for
the next normal prompt. Native custom-message receipts suppress duplicate
notifications on resume. Notification is never acknowledgement: the mate must
read, handle and acknowledge the mail itself. `/privateer-mail pause`, `resume`
and `status` control automatic delivery. A native error or interruption pauses
automatic wakes until an explicit resume or the next normal prompt.

The launching wrapper registers presence before starting the CLI and refreshes
only its own holder. A replacement session cannot be reclaimed by an old
heartbeat. Claude hooks share the wrapper's holder when launched this way;
raw Claude hooks retain their existing lifecycle. While presence is live,
`serve` leaves work with the attended session.

Real CLI/extension fixtures cover a thousand-message burst, busy/idle delivery,
resume receipts, failed queues, native error boundaries, holder replacement and
shutdown. Pi 1.1.0 has also passed a live isolated interactive file/reply/ack
canary using existing OpenAI OAuth. These checks do not establish live OMP
model availability or signed-Pi qualification on a machine without that wrapper.
OMP 18.8.9 has loaded the extension and displayed a low-priority notice in its
native terminal without a model turn. Its live model/tool path remains
unqualified under the account limitation noted above.

## Gateway Pi adapter

`gateway-pi` is an optional integration for people routing Pi through a private gateway. Copy `config.example.json` outside the source checkout, edit its endpoint/provider/model/Pi configuration path, and set mode `0600`. Both adapter config and Pi models file must be private and user-owned. Keep API keys in Pi's private native configuration, never in the gateway adapter config or command options.

The adapter verifies that the chosen Pi file is the active `PI_CODING_AGENT_DIR/models.json` (or `~/.pi/agent/models.json`), its provider uses the exact configured endpoint, and the model exists under that provider. Its launcher repeats these checks immediately before invoking Pi with explicit provider and model. It has no fallback. Endpoint URLs cannot contain credentials, query strings or fragments. Configuration and exceptions are not returned in diagnostic output.

Gateway Pi accepts the same Pi session/mode/effort options, while binding provider
and model to its private route. Conflicting requested selectors are refused;
resuming a session still repeats route validation immediately before launch.

This protects against accidental route selection and configuration changes between planning and invocation. It is **not network enforcement**: trusted Pi extensions, configuration mutation after validation, proxies and custom executables remain outside this guard. A hard egress guarantee requires an external network/credential boundary. Gateway availability and live inference are not established by a command plan.

The examples contain generic placeholder names, no user's endpoint, account, model or keys. Protocol tests use temporary private files and fake native executables.
