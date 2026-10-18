# Fleet tool permissions

Ship charters authorize work; native harness permissions decide whether its
tools can run. Claude launches now explicitly allow the Privateer CLI, `git`,
and `gh`, with `acceptEdits` unless a profile selects another mode. Launch flags
work even in folders where Claude has not accepted the project-trust dialog.
Native deny rules and managed restrictions still apply.

Every headless Claude launch also names the ship's home and the resource
ledger with `--add-dir`, and removes the tools whose work would outlive the job
(`CronCreate`, `ScheduleWakeup`, `RemoteTrigger`). First mates (wakes, errands
and attended sessions) additionally lose Claude's own delegation tools (`Task`
or `Agent`, `Workflow`, `EnterWorktree`): they hand work to crew jobs, which the
fleet tracks, rather than to agents it cannot see. A ship whose first mate runs
unattended on Claude can select `"permission_mode": "auto"` in its profile, so
Claude's classifier approves routine commands instead of denying every command
outside the allow list; this depends on the account offering auto mode. The
first interactive session in a folder shows auto mode's one-time setup screen;
before hosting such a ship, run `privateer start claude` in its folder once,
accept the folder trust dialog and finish that screen.

Configure the whole local fleet from any directory:

```sh
privateer permissions show
privateer permissions allow 'Bash(forge-helper *)'
privateer permissions yolo
privateer permissions standard
```

Telegram supports `/yolo on`, `/yolo off`, and `/yolo status` through the existing
authorized chat/user checks. This is a direct captain command, without inference
or asking an agent to edit its own settings.

The policy applies on the next launch to existing and future ships: background
wakes, exact-session resumes, crew using bundled adapters, and attended
`privateer start`/dashboard Board. Running sessions and already-planned queued
jobs keep their launch permissions.
No reinitialization is required. Each fleet has its own private atomic
`fleet.permissions.json` beside its registry; an alternate `PRIVATEER_FLEET`
selects a separate adjacent policy. Other hosts/fleets are configured separately.

JSON API: `permissions.show {}`; captain-only `permissions.set` accepts:

```json
{"mode":"standard","claude_allow":["Bash(forge-helper *)"]}
```

`claude_allow` replaces additional rules; `claude_allow_add` appends one under
the fleet write lock. Standard mode stops forcing YOLO and restores native
profile/settings choices; it does not revoke custom rules or a profile that
already requests permissive access. No native settings files, hooks, credentials,
provider routes, model choices, or charter actions are rewritten.

## When a tool is denied

A headless Claude run that is denied a tool fails, and its last line,
`native.permission_blocked`, lists up to four denials. When the job settles, the
runtime turns them into one captain **decision** from the task's owner (see
[one block, one ask](design/one-block-one-ask.md)):

- The card names the ship, the task, the job, the narrowest rule that would have
  allowed each denial (`Bash(apify run *)`, `Edit(./src/a.rs)`,
  `WebFetch(domain:example.com)`, or the tool's name) and a preview of at most 160
  characters with secret-looking values masked. The full input stays in the job log.
- It offers **Allow** (adds the rules to this machine's fleet policy), **Continue
  without it** (the task's first mate is told; a later denial of the same rules asks
  nothing new) and **Stop the task**. Allow is applied only for the operator's own
  answer: a liaison's `captain.relay`, or a reply from a machine that does not hold
  the captain's desk, cannot grant it.
- A crew's denial holds its task: another `agent.start` on it is refused until the
  captain answers or the policy covers the rules (`replace: true` starts a crew that
  works another way). A scheduled task waits on the decision. The crew's outcome is
  a low notice, so the first mate is not woken to ask again. A first mate's own
  denial pauses its ship's automatic wakes, as before. An errand's denial asks but
  holds nothing.
- A policy change ends the decisions it covers (`yolo`, or every rule allowed); an
  unrelated change keeps them open.
- Mail to the captain about an open decision (citing `about`, correlated to the
  blocked ship's notice, or the same denial on another ship) is added to it rather
  than shown again, and gets the captain's answer.
- A hosted or attended first mate, whose harness result the runtime cannot read,
  reports a denial with `permissions.blocked {"task_id","tool","input"}`.

## YOLO translations

| Harness | Behavior |
| --- | --- |
| Claude | `--permission-mode bypassPermissions` |
| Codex | `--dangerously-bypass-approvals-and-sandbox`, replacing the profile sandbox selector |
| Pi / signed Pi | Native tools already run without approval prompts; extensions keep their own behavior |
| OMP | `--approval-mode yolo`, including RPC |
| Gemini | `--approval-mode yolo --skip-trust` |
| Cursor | `--force --sandbox disabled --trust` |
| AGY | `--dangerously-skip-permissions` |
| Grok | Native interactive bypass; headless ACP grants offered one-time approvals |
| Rovo / Kimi / Devin ACP | Grant offered one-time approvals |
| Muse | Native approval `never`, workspace trust and disabled sandbox; MSP selects `allowAll` and grants offered one-time approvals |
| Devin native | `dangerous` permission mode, without its sandbox |
| OpenCode V1 | Process-local `OPENCODE_PERMISSION={"*":"allow"}` |
| OpenCode V2 | Native `--auto` |

Some flags disable sandboxing as well as prompts. Native hard denials, hooks,
account restrictions and managed policies may still refuse tools. Kimi native
and unknown custom harnesses have no qualified YOLO mapping and fail explicitly
instead of silently using standard mode. Kimi ACP is supported. Custom harnesses
retain their attended behavior in standard mode; third-party adapters must
implement the context's `tool_permissions` policy.

Charter, custody, production approval and resource checks remain unchanged.
As with existing arbitrary shell jobs, these are workflow controls under one OS
identity, not credential isolation.

## Permission blocks

Claude's native final `permission_denials` array is inspected without interpreting
assistant prose. A nonempty array fails the job even after native exit zero.
Supervised runs record a task-linked question in the captain's durable mailbox;
repeated blocks for that task and policy coalesce. Tool inputs/commands are not
copied into the question. Original native logs retain the existing job bounds.

Repeated automatic attempts pause until a reply or policy change. Fresh
captain/peer mail and other-task outcomes can still wake the first mate; the same
newly observed mail cannot repeatedly bypass the pause. Batching, backoff and
hourly budgets still apply. A reply does not itself grant access: the captain can
instead ask the ship to stop or take another approach.

This reporting path currently covers Claude structured denials. It does not
claim detection of arbitrary prompts in every CLI; other failures retain normal
job reporting. Tests use fake native processes, real supervised jobs and fleet
state, and a local fake Telegram API, without paid model calls or live changes.

Native references: [Claude permissions](https://code.claude.com/docs/en/permissions),
[programmatic runs](https://code.claude.com/docs/en/headless),
[OpenCode V1 CLI](https://docs.opencode.ai/docs/cli/), and the installed OpenCode
V2 `--help` contract for `--auto`.
