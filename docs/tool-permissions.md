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
privateer permissions authorize storefront "Making storefront's own Apify actors public in the Apify Store."
privateer permissions unauthorize storefront "Making storefront's own Apify actors public in the Apify Store."
privateer permissions yolo
privateer permissions standard
```

## Standing authorizations for auto mode

Claude's auto mode asks a classifier about each action. It trusts its
settings, not what the agent reads, so a grant the captain gave a ship in mail
or a note does not count: the classifier refuses the action (making a listing
public, deploying, changing an account) and the ship asks again. A tool rule
cannot fix this when the refused command is a script, since no narrower rule
exists.

`privateer permissions authorize SHIP TEXT` records one sentence the captain
allows, for that ship only. Every Claude launch of that ship's first mate
(wakes, hosted and attended sessions) adds it to `autoMode.allow` after
Claude's defaults, prefixed "The captain has authorized this:". Crew and
errands do not receive it. Write what is allowed and where, as you would to
a colleague. `unauthorize` with the same text removes it. Each ship holds up to
16 sentences of at most 500 characters. The classifier still judges each
action against them, and they change nothing for ships that do not run in
auto mode. A decision card whose denial has no narrower rule names this
command.

A first mate's Bash denial that is a script (a compound, multi-line or piped
command no prefix rule covers), or that the auto-mode classifier refused, has
no Allow option. Its card offers **Authorize** instead, with a suggested
sentence built from the programs the command runs and the classifier's reason,
for example "Running cp and echo commands its assigned work needs, even when
the auto-mode check flags them as Credential Materialization." Tapping it
records that sentence for the ship; an answer with its own text records the
captain's wording instead, which must be one line of at most 500 characters
(a longer or multi-line answer is refused when sent, and the card stays open).
The suggestion names only the programs on the command's first line, up to any
heredoc, so a heredoc's contents never reach it. The card's body also gives the
matching `privateer permissions authorize SHIP '...'` command to edit and run.
Only the captain's own answer authorizes, as with Allow. An authorization is
that ship's alone: another ship's card with the same suggestion stays its own
card, is never answered by it, and stays open until its own ship is
authorized. Crew and errand cards never offer it, including a crew's job its
first mate reports blocked, since they do not receive authorizations.

When a hosted pane is stopped by auto mode, its card names the refused tool
uses since the last incident and each one's reason, read from the pane's own
Claude transcript (`<Claude config>/projects/<folder>/*.jsonl`, never through a
symlink). Each refusal names its tool use by `tool_use_id`, and the card takes
the tool and input from that tool use. Claude 2.1.300 also writes every API
request into the transcript, so a busy session writes many MiB an hour: the
end is read first, then four times as much each time, up to 64 MiB, until the
read reaches the incident's start and every refusal's tool use. If the
transcript cannot be read, or a refusal's tool use is not in what was read,
the card says "denied tool not identified" and offers no Allow.

The host sees a pane's denial when its screen shows Claude's footer notice
(`bash denied by auto mode · [reason] · /permissions`). That notice shows for a
few seconds, and Claude holds it back while the pane holds notices or a diff
panel is open, so the block can be seen long after the refusal. A card filed
more than five minutes after the refusal says when it was refused. A refusal
the same session got past is not filed: a later successful call of the same
tool with an equivalent input (the same file for `Edit`, `MultiEdit` or
`Write`, the same command for Bash, the same input for any other tool) means
the session is no longer blocked. An open pane card whose refusals were all
identified is watched: the transcript is read on from where the incident's
read ended, and once the session gets past every refusal on it, the card is
withdrawn with "the session got past this block" and the time of the retry,
and its task no longer waits on the captain. Another ship's card joined to it
is detached and stays open. A card that names an unidentified refusal is
never withdrawn this way.

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
{"mode":"standard","claude_allow":["Bash(forge-helper *)"],
 "authorized":{"storefront":["Making storefront's own Apify actors public in the Apify Store."]}}
```

`claude_allow` replaces additional rules; `claude_allow_add` appends one under
the fleet write lock. `authorized` replaces every ship's authorizations;
`authorize` and `unauthorize` take `{"ship","text"}` and add or remove one. Standard mode stops forcing YOLO and restores native
profile/settings choices; it does not revoke custom rules or a profile that
already requests permissive access. No native settings files, hooks, credentials,
provider routes, model choices, or charter actions are rewritten.

## Captain approvals

A captain approval is one step the captain said yes to, for one ship and one
of its open tasks. It reaches `autoMode.allow` as one sentence and nothing
else: it never overrides a hard deny rule, `permissions.deny` or a managed
restriction. A production deployment still needs `production_approval` and
`approval.grant`, and a privileged command still needs `privileged.stage`;
neither can ride an approval.

### How a ship asks

A first mate asks once, on a `request` or a `decision` to the captain, with
`payload.approves` and its own open `task_id`:

```json
{"action":"Submit build 55 to TestFlight","command_prefix":"make release","ttl_secs":86400}
```

- `action` is one line of at most 300 characters: exactly what the ship will do.
- `command_prefix` is optional, at most 120 characters: the plain words the
  command starts with (see the prefix rules below).
- `ttl_secs` is optional: 1 second to 7 days, 24 h by default.
- A ship holds at most 8 live approvals.
- The send is refused when the action or prefix names another ship, mentions
  policy or settings words (`permission`, `.claude`, `charter`, `automode`),
  credentials (`.ssh`, `token`, `password`), reads as broad (`anything`,
  `always`, `from now on`), or restates the approval's own framing
  (`authorized`, `captain`, `expire`).
- On a decision, the approving options carry `"approves":true`. A request
  that carries approves cannot also carry buttons.

The stored body gains a line saying what a yes grants, such as "Approving
lets apps do this for 24 h: Submit build 55 to TestFlight".

### What counts as the captain's yes

Only the captain's own reply, in the home that holds the request, records an
approval:

- On a decision, choosing an option marked `"approves":true`. Text beside it
  may hold only plain words or the option's own label words.
- On a request, a plain free-text yes: "yes", "ok", "go ahead", or the
  action's own words.

A hedge ("yes but", "wait", "only staging"), a question mark, an emoji or
symbol, or a reply that narrows the action ("deploy to staging" for "Deploy
to staging and prod") approves nothing. The reply's `payload.approval` says
why, and the ship is told to ask again with a decision. A relay, a bridge
copy, another ship, a crew and an errand never create one. A request whose
task closed before the reply approves nothing.

### How it reaches auto mode

Each launch of the ship's first mate adds one sentence per live approval to
`autoMode.allow`:

> The captain approved this in reply R to request Q, for ship S only, on task
> T, until 2026-10-22 14:00 UTC: Submit build 55 to TestFlight Only commands
> that begin exactly with: make release

Crew and errands never carry approvals. A headless wake carries it from its
next launch. A hosted Claude pane refreshes its session at a safe point: idle
for 15 s with an empty prompt, not paused on a permission question, at least
5 minutes since its last refresh, and with a conversation to continue, which
the restart keeps. A pane sitting at auto mode's stop prompt counts as safe.
The ship's thread then gets "Captain approval applied in your restarted
session: retry <action> once."

### Block cards

When auto mode's classifier refuses a first mate's Bash command in a hosted
pane or a wake, its card may offer two kinds of option.

**Approve for 24 h: Run `<command>`** appears only when the card shows each
refused command whole and each is one simple command whose words pass as a
prefix. Compound commands (`;`, `&&`, pipes), heredocs, clipped, masked or
flattened commands, quoted or globbed words, wrapper-led commands and
interpreter inline code never offer it. The tap records the step for 24 h,
with the command's words as its prefix when there is one command.

**As approved: <action>** offers up to two approvals created in the last 24 h
that the refused session does not already carry. One is recommended when its
prefix covers the refused command. A live one is applied at the session's
next safe point; one that expired is renewed for its own length. Anything
older than 24 h is never one tap away: ask again.

Crew and errand cards, and cards naming any other denial, offer neither.

### Prefix rules

A prefix is plain words of one simple command, split on spaces and tabs:

- None of ``; & | < > ( ) $ ` \ ' " * ? [ ] { } ~ #``, and printable ASCII
  only.
- No assignment first (`FOO=1 make`).
- No wrapper first: `env`, `sudo`, `xargs`, `eval`, `doas`, `su`, `exec`,
  `command`, `builtin`, `nohup`, `timeout`, `nice`, `time`, `watch`, `ssh`,
  `coproc`, `!`.
- An interpreter (`bash`, `sh`, `zsh`, `dash`, `ksh`, `fish`, `node`,
  `perl`, `ruby`, or any `python*`) names its script first, with no option
  before it and no inline-code flag after it (`-c`, `-e`, `-lc`, `--eval`,
  `node -p`, `python -m`): `python3 scripts/release.py`, never
  `python3 -c`.

Matching is by whole words, with no basename or case folding: `make release`
covers `make release --dry-run`, never `make releasex` or `sudo make
release`.

### Seeing and revoking

```sh
privateer permissions approvals              # every ship's, newest first
privateer permissions approvals list apps
privateer permissions approvals show approval-0123
privateer permissions approvals revoke approval-0123
```

The JSON commands are `captain_approval.list {"ship"?, "live_only"?}`,
`captain_approval.show {"id"}` and `captain_approval.revoke {"id"}`. Each row
has a `state`: `live`, `expired`, `revoked`, or `closed` once its task is
done, cancelled or handed to another owner, or its charter is inactive. A
live row also has the `sentence` a launch carries. `show` adds `applies_at`
and `carried_by`, the hosted panes whose launch carried it. Launches, list,
show and the dashboard all read one rule for this. The operator may list
any ship; an agent lists and shows only its own ship's.

Revoking is the captain's alone, from the CLI, the JSON command or the
dashboard's `x` key. It never stops a session mid-turn. The reply's `note`
says when it stops applying: at the ship's next idle point (`relaunch`), at
its next wake, or at its next launch. A second revoke answers
`"already":true`.

Every ship runs as the same OS user, so this is a workflow control that keeps
a confused agent from approving itself, not isolation.

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
  denial pauses its ship's automatic wakes, as before; a low notice such as another
  ship's "still blocked?" does not lift that pause. An errand's denial asks but
  holds nothing, and its failed verdict is a low notice naming the decision.
- A policy change ends the decisions it covers (`yolo`, or every rule allowed); an
  unrelated change keeps them open. A decision whose denial names no rule ends on
  any change for one retry, and its notice says plainly that the runtime can't
  tell whether the new policy covers it and nothing was granted (`granted:
  false`): the ship retries the step once and, denied again, reports it with
  `permissions.blocked`. A decision offering a standing authorization ends when
  its own ship's authorizations change, but the runtime can't tell whether the new
  wording covers it, so it is never told the policy allows it (`granted:
  "unknown"`): it asks for `privateer host relaunch <ship>`, since auto mode reads
  authorizations only at launch, retries once, and if denied again reports it with
  `permissions.blocked`.
- Mail to the captain about an open decision (citing `about`, correlated to the
  blocked ship's notice, or the same derived rule denied on another ship) is added
  to it rather than shown again, and gets the captain's answer. A denial no rule
  narrows never joins another ship's decision. If the decision ends without an
  answer (withdrawn, or its task closed), what was added to it ends with it; when
  a policy change ends it, another ship's decision the change does not cover is
  detached and shown on its own instead.
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
