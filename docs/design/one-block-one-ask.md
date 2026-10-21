# Design: one block, one ask

Status: revised after the Opus design review of ff2ce0b (approve with eight changes, all folded in below), and being implemented. Task `privateer-one-block-one-ask` (captain finding 13, notice d8789f3d). It also covers finding 7 (`privateer-permission-decision-detail`, a decision that names the denied tool and input, the narrowest rule, and the job). Read with [crew park and resume](crew-park-resume.md), [cross-machine fleet](cross-machine-fleet.md) and [tool permissions](../tool-permissions.md).

## Problem

One evening, one harness block reached the captain's Telegram three or more times:

1. The blocked ship's first mate is woken by its crew's failed job and mails the captain in free text ("Apify step denied, please allow").
2. The ship that asked for the work (storefront) sees the stall and mails the captain the same block.
3. Liaison sometimes relays storefront's relay.
4. Each retry or "still blocked" wake adds another message.

Before this change the runtime had a narrow fix, but most of these paths went around it:

- **Headless Claude only.** `plugins/native_launch.py` hands a `--print` stream-json Claude run to `plugins/claude_turn.py`. `ClaudeTurn.observe` sets `self.denied` when the final `result` has a non-empty `permission_denials`. A denied run ends `native.turn.failed` (never `turn.completed`), and `report_denial` ran `permissions.blocked {"job_id"}` as a subprocess, then printed `native.permission_blocked`. No field of a denial was read; only "the array is not empty".
- **`permissions.blocked`** opened one captain `request` with `request_kind` `tool-permission` and `payload {type: tool_permission, policy_hash}`, keyed by (owner, task, `policy_hash`, episode), returning `duplicate: true` while one was open. It copied no denied input into mail; the test `native_permission_denial_becomes_one_question_and_pauses_automatic_retries` asserted that `PRIVATE_DO_NOT_MAIL` was absent.
- **`permissions::paused`** paused the *whole ship* while that request was open under the current policy and no "fresh" mail had arrived. It is checked by `consider` (`src/activation.rs`), the scheduling `blocked` gate (`src/scheduling/delivery.rs`), hosted launch (`src/host.rs`), `sleep` (`src/host/sleep.rs`), upgrades and Herdr. `alarm` (`src/host/sleep.rs`) did not check it, so an asleep hosted pane still woke on any mail that was not a low notice.
- **The crew outcome still woke the first mate.** A crew job ended by a denial is `failed`, and `crew::outcome` wrote it at `high` urgency, so the first mate woke, read "failed", and mailed the captain itself (path 1).
- **The task was not parked**, so the first mate could relaunch a crew into the same denial.
- **Hosted and attended Claude are blind.** `plugins/attended-claude/hooks/register.js` handles session, prompt, turn and compact events only. Those ships report blocks in free text.
- **The captain side had no topic merging.** `mail.send` dedupes only an identical explicit `id`. "Thread" is the request that `correlation` names. The Telegram door dedupes by mail id (`sent.json`), never edited a card's text (only `editMessageReplyMarkup` after a tap, to clear the buttons), and showed a tool-permission request as an ordinary ❓ request with no buttons.

## What Claude reports (Q2, captured)

Captured from the installed Claude Code 2.1.300 in a scratch git repository with no settings changes, scrubbed, and checked in under `tests/fixtures/claude/` with a README:

- **A denial** (`--permission-mode acceptEdits` or `dontAsk`, a `curl` that is not allowed): the final `result` carries `permission_denials: [{tool_name, tool_use_id, tool_input}]` with the full input. The run still ends `subtype: success`, `is_error: false`; the tool result reads "This command requires approval".
- **Auto mode** (`--permission-mode auto`): two explicitly requested commands, a force push to a local bare remote and `sudo -n rm -rf` of a decoy directory, were both allowed. No auto-mode refusal was captured, so its shape stays unverified. The `result` line has a `safety_stops` count, which is the likely field. Round 2 ships the `denial` source only.
- **Hook events** (a session-only `--settings` file logging every event, in `dontAsk`): a denial fires `PreToolUse` and nothing after it. There is no `PostToolUse`, `PostToolUseFailure` or `PermissionDenied`. An attended hook could only infer a denial from a `PreToolUse` with no matching `PostToolUse`. The automatic attended hook stays a later round.

## Model

There are three parts, one per item in the finding:

- **(a) Block ask.** The runtime turns a harness denial into one typed **decision** to the captain and, for a crew, parks the task on it.
- **(b) Attach.** Captain mail on the same topic as an open ask is *attached* to it: recorded, but not shown as a new item. Answering the ask answers its attachments.
- **(c) Quiet park.** While the ask is open, nothing about the block wakes its ship. Only the answer, a covering policy change, or genuinely new mail does.

The code lives in `src/block_ask.rs`; `src/permissions.rs` keeps the policy itself.

### (a) Block ask

**Our own denial line.** `claude_turn.py` keeps failing a denied run, and now prints `native.permission_blocked` with `denials`: at most 4 entries of `{tool_name, tool_input}`, each input clipped to about 4 KB. Settlement reads that line from the last 64 KiB of the incarnation's `stdout.log`, never Claude's `result` line: the job log keeps a 256 KiB head and a rolling tail cut at any byte (`LOG_LIMIT`, `src/jobs.rs`), so a denied `Write` of a large file can lose its result line, while our short line is the last thing the plugin prints. Nothing new is exposed: the full result is already echoed into the same log. The `report_denial` subprocess is gone; nothing denied crosses argv.

**Detection at settlement.** Every job ending passes through `jobs::alter`, which writes `crew::outcome` in the same `BEGIN IMMEDIATE` transaction. `block_ask::settle` runs there first, on the change to a terminal status (`succeeded`, `failed`, `cancelled`, `timed_out`; not `orphaned` or `resumed`), for wake jobs, crew jobs and errands alike.

- It must not open a nested transaction: `mail.send` and `task.defer` each call `write_transaction`, which fails inside `alter` and would roll back the job's own settlement. So it writes the ask, the park and the notices with in-transaction helpers, as `record_crew` does.
- It runs inside a `SAVEPOINT`. Any error rolls back to the savepoint and settlement carries on with today's `high` failed outcome; `alter` never fails because of a block.
- A dispatcher restart between the job ending and settlement is recovered by `reconcile`, which settles through `alter` too.
- It keeps `permissions.blocked`'s owner logic (the ask is from the task's owner) but not its caller and `authorize` checks: the runtime is settling its own job.

**Denial record**, one per distinct rule (or tool, when no rule can be derived), at most 4 per ask; the rest are counted (`more`):

| field | from |
|---|---|
| `tool` | `tool_name` |
| `rule` | the narrowest allow rule (below), or null when none can be derived |
| `preview` | the input, masked, at most 160 characters |
| `source` | `denial` (auto-mode stays unverified, see Q2) |
| `reason` | the refusal's reason, masked, when one was read (a hosted pane's transcript: `[Credential Materialization]`) |
| `authorize` | for a Bash denial with no rule, or one the auto-mode classifier refused: the standing authorization the card offers instead of `allow` (first mates only) |
| `unidentified` | true, with an empty `tool`, when the denied tool use could not be read; no rule is offered |

**Narrowest rule**, `block_ask::narrow_rule`, in the fleet's own rule style (`Bash(git *)`):

| tool | rule |
|---|---|
| `Bash` | `Bash(<first> <second> *)`, or `Bash(<first> *)` when the second word is missing, starts with `-`, or is a path or file name. None for a compound or expanded command (`;`, `&`, `|`, redirection, `$`, quotes, globs, a leading `VAR=`), because the rule cannot say which part was denied |
| `Edit`, `Write`, `Read`, `NotebookEdit` | `<Tool>(./<path>)` inside the job's folder, `<Tool>(//<absolute path>)` outside it; None for `..` or globs |
| `WebFetch` | `WebFetch(domain:<host>)` |
| `mcp__*`, other tools | the tool name alone |

A rule is refused (null) if it would fail the policy's own validation or allow every use of a tool (`Bash(*)`).

**Preview and masking** (captain decision, with finding 7). The card shows the matched rule and a preview of at most 160 characters: the Bash command, the file path or the URL, else the input as JSON. Secret-shaped values are masked as `•••`: long hex or base64 runs mixing letters and digits, `NAME=value` where the name sounds sensitive (key, token, secret, pass, auth, cred, cookie, session), the word after `Bearer`, `token`, `Basic` or `Authorization:`, and URL credentials. The full input stays in the job log. The old `PRIVATE_DO_NOT_MAIL` assertion becomes "a planted secret is masked".

**Payload.** The ask is `kind: "decision"`, not `request` with `request_kind`. Queries that find an open ask accept `kind IN ('request','decision')`, so asks opened before the upgrade still pause and resolve.

```json
{"type": "tool_permission", "policy_hash": "<sha256 of the policy>",
 "block": {"key": "<hex>", "fleet_key": "<hex>", "ship": "browser-mac", "task_id": "...",
           "job_id": "...", "scope": "crew", "harness": "claude", "more": 0,
           "denials": [{"tool": "Bash", "rule": "Bash(apify run *)",
                        "preview": "apify run actor/x --input …", "source": "denial"}]},
 "decision": {"question": "browser-mac: Claude was denied Bash(apify run *)",
   "why": "Its crew for <task title> stopped at this tool. The task waits until you answer or the fleet policy covers it (/yolo on also clears it).",
   "blocked": ["<task id>: <task title>"], "free_text_ok": true, "group": "tool-permission",
   "options": [
     {"id": "allow", "label": "Allow Bash(apify run *)", "what_happens": "Adds the rule to this machine's fleet policy for every Claude ship; every task blocked on it resumes."},
     {"id": "without", "label": "Continue without it", "what_happens": "The task resumes; its first mate is told the tool stays denied."},
     {"id": "stop", "label": "Stop the task", "what_happens": "The task is held; its owner is told."}]}}
```

`scope` is `crew`, `wake` (the first mate's own wake) or `errand`. The ask follows `validate_decision`: question at most 200 characters, why at most 500, 2 to 8 options. When no rule can be derived, `allow` is left out and the card has two options. Several rules make a single `allow` that adds all of them. No option is `recommended`. When several tasks are attached, the options say the answer applies to all of them.

**Keys and idempotence.**

- `key` = sha256(ship, task, sorted rules or tool names, `policy_hash`). The ask id stays `permissions-<sha256(ship\0task\0policy_hash\0episode)>`, so one task under one policy has one ask, whatever denial a retry or a resumed crew hits.
- **Known limit:** the id leaves out the rules. A second, different denial of the same task under the same policy is a duplicate of the open ask, so the captain sees the second rule only after answering the first. Accepted for this round.
- The open check and the insert run in `alter`'s transaction, and the row is inserted with `ON CONFLICT(id) DO NOTHING`, so a replay writes nothing.
- After the captain answers `without`, the denied rules are recorded in `permission_declined(ship, task_id, rule, policy_hash, ms)`. A later denial of declined rules only opens no new ask; its outcome tells the first mate "the captain declined <rule>; do it without".
- `fleet_key` = sha256(sorted rules, `policy_hash`): the same denial on two ships. The second ship still gets its own ask row, so its task can wait on it and its closure is local. That row is attached (part b) to the first one, so the captain sees one card.

**Park.** In the same transaction, for a crew denial only (Q4, Q7, change 7):

- A task that is already **scheduled** gets the wait `{"kind": "request", "ref": <ask id>, "reason": "tool permission: <rules>"}`, state `waiting`, and status `blocked`, as `task.defer` sets them.
- An **unscheduled** task gains no scheduling row (Q7): `task.list` and bearings show it as before. The captain's reply is normal mail and wakes the owner; a policy change that ends the ask sends the owner a runtime notice at `normal` urgency.
- `agent.start` on a task with an open crew block ask is refused, naming the ask and its rules. `replace: true` starts a crew anyway to try another way; the ask stays open. This mirrors the refusal on parked crews.
- A denial in the first mate's own wake job (`scope: wake`) pauses the ship, as before, because that wake would hit it again.
- An **errand** denial (`scope: errand`) raises the ask but parks nothing and refuses nothing: nothing retries a finished errand, and a denied review errand must not hold the main task.

**Resolution.** Each answer is applied where the ask lives (its home, and its machine), after the reply commits:

- `allow`:
  - granted **only on a positive operator marker**. `mail.send` stamps `payload.operator: true` on a captain reply when the caller is the operator and the reply is the operator's own (the door, the CLI). The field is refused from anyone else, and from any caller who sets it by hand. `captain.relay` writes through a path that does not stamp it, so a liaison passing the captain's words on cannot grant. Fan-out keeps the original reply's marker.
  - Across machines, a delivered reply keeps the marker only when the runtime-stamped machine that delivered it (`via`) is the one holding this machine's captain desk; otherwise `deliver` drops it. The machine is stamped by the collector when it collects, and checked against the link (`reaching_here`) by the gateway when the other side delivers.
  - The runtime adds the rules with `permissions::set` as operator (`claude_allow_add`). The reply has answered the ask, and the request wait reopens a scheduled task.
  - An `allow` answer without the marker is refused at send time with "Allow must come from the captain directly; ask the captain to tap Allow or run `privateer permissions allow '<rule>'`".
- `without`: `declined` is recorded, and the first mate wakes with the answer as its mail; a scheduled task reopens.
- `stop`: a scheduled task's row goes to `stopped` with a manual wait, as `task.defer` gives it. The owner gets the answer as normal mail.
- Free text: the first mate reads the text; a scheduled task reopens.
- **Policy change** (change 4). Adding an unrelated rule changes `policy_hash` too, and ending every open ask on any change would reopen tasks into the same denial and send fresh cards. So `permissions::set`, and each dispatcher tick in every home, sweep open block asks:
  - the ask ends, `withdrawn` with reason `policy changed`, when the new policy **grants** it: `yolo`, or every derived rule present exactly or by tool name in `claude_allow`;
  - an ask offering a standing authorization (with any derived rule it also names allowed) ends when its own ship's authorizations change, never another ship's. The runtime cannot tell whether the new wording covers it, and auto mode reads it only from the next launch, so this grants nothing it can confirm. Its reason says so: `policy changed: this ship's standing authorizations changed, but whether they cover this ask cannot be told; relaunch, then retry once`;
  - an ask with a denial no rule names (and no recorded authorization) ends on any policy change for one retry, since the runtime cannot tell what the change covers. Its reason says so: `policy changed, but nothing this ask named was granted; retry once`;
  - otherwise its `policy_hash` is updated in place and it stays open.
  
  The request wait treats a block ask withdrawn for any of these policy reasons as ready, not `fault`, and the resumed work's prompt says which it was. An unscheduled task's owner gets a `normal` runtime notice whose `granted` is `true`, `"unknown"` or `false`. Only `true` says the policy now allows what it asked for and to carry on. `"unknown"` says its ship's standing authorizations changed and the runtime can't tell whether they cover the request: ask for `privateer host relaunch <ship>`, retry once, and if denied again report it with `permissions.blocked`. `false` says the runtime can't tell whether the new policy covers it and nothing was granted: retry the step once and, denied again, report it with `permissions.blocked`. `paused` uses the same coverage test, so a stale hash between a change and the next sweep cannot unpause a ship by accident.
- **Task closed.** `close_task` already ends the task's open requests and decisions.

**Hosted and attended ships.** Their harness result is not visible, so they get a structured way to report instead of free text:

- `permissions.blocked` accepts `{"task_id", "tool", "input"}` from the task owner when there is no `job_id`. It builds the same denial record and ask (`scope: wake`).
- The first-mate brief says: "If a tool is denied, call permissions.blocked, then stop; do not mail the captain about it."

### (b) Attach: one card per topic

**What "same topic" means.** It is resolved in the **sender's** home at `mail.send` (and `deliver`), and written to `payload.about = <ask id>` by the runtime. Rules, in order:

1. **Explicit:** `mail.send` accepts `about: <captain item id>`. The id must name an open ask in this machine's fleet, or a mail this home holds whose `payload.about` or `payload.block` names it.
2. **Correlation chain:** follow `correlation` through mail in this home, at most 8 hops. If a hop is the ask itself, or carries `payload.about` or `payload.block` naming an ask, that ask is the topic. This links storefront's relay (correlated to the notice browser-mac's home sent it) and liaison's relay of that.
3. **Task link, notices only** (change 5): a notice whose `task_id`, or that task's parent chain within this home, has an open block ask. Requests and decisions attach only by rule 1, rule 2 or rule 4, so fan-out can never close an unrelated question with "Allow Bash(...)".
4. **Same denial:** a new block ask whose `fleet_key` matches an open one from another ship, when every denial of the ask is a derived allow rule. A denial no rule narrows (a compound command, an unknown or unidentified tool) is named by its tool alone, which says nothing about what either ship ran, so it never joins another ship's ask; nor does an ask that counted denials beyond the ones it lists. The desk applies the same test to a delivered copy, ignoring any `about` an older origin set on it.

`about` is stamped on every kind of mail, not only captain mail, so the chain survives hops between ships. Task ids do not cross homes, so across ships only rules 1 and 2 apply. The brief adds one sentence for every role: "When you pass on another ship's message, set correlation to it."

**Telling the requesters.** When an ask opens, the runtime sends a `low` notice to the senders of open requests to the blocked ship (at most 5 senders, never `crew` or the captain; Q5 accepts the noise):

> browser-mac is blocked on captain decision `<id>` (Bash(apify run *)); the captain has it. If you add anything, cite `about: <id>`.

The notice carries `correlation` = their request and `payload.block` = the ask id. It does not wake them, and it gives their later relay a chain to follow.

**Attach, not refuse.**

- Refusing makes agents rephrase and retry, which is the loop we are closing. The extra text may also carry something new.
- So captain mail of kind `request`, `decision` or `notice` whose topic is an open ask gets `payload.attached_to = <ask id>` (and `payload.attached_ship`, the ask's ship), and its row is stored normally:
  - a request or decision stays `open`, so its sender's own waits and closures still work;
  - a notice has no state.
- `production_approval`, `needs_hands` steps and replies are never attached.
- If the ask has already closed, `about` is kept but nothing is attached, and the mail shows as an ordinary item.

The sender's `mail.send` result gains:

```json
{"attached_to": "<ask id>",
 "note": "The captain already has this as <ask id> (browser-mac, Bash(apify run *)). Your message was added to it; you will get the captain's answer. Do not resend."}
```

**Fan-out** (Q6: all three answers). When the primary ask is answered, the runtime sends the same answer to each attachment that is still open, in the home that holds it:

- typed decisions with the same option ids get `payload.answer`;
- plain requests get the answer's text and option label;
- every fan-out reply carries `payload.fanned_from` and keeps the original's `operator` marker, so an attached block ask on another ship applies its `allow` in its own home.

Ending the primary for a policy change first settles each other ship's block ask attached to it on its own: one the policy covers in its own home ends as that home's sweep would end it, and one it does not cover is detached (`permissions.detached`), so the captain sees it as its own card. Ending the primary then ends the remaining open attachments (relays, and any ending for a closed task) with state `withdrawn`, reason `merged ask <id> ended: <reason>`. An attachment answered directly (from `/waiting`) closes alone; the primary stays open.

**Across homes and machines.**

- *Same machine.* At send time, the open-ask lookup searches every fleet home on this machine. `attached_to` is set on the row in the sender's home, which is the row the door pages.
- *Desk on another machine.* Mail collected or pushed to the desk carries `payload.about` from its origin. When `mail.deliver` writes the desk copy, it runs the same lookup there and sets `attached_to` on the desk copy. The origin row keeps no `attached_to`, and the sender is not told at send time; it receives the fan-out answer or the closure through the existing reply and closure paths.

### (c) Quiet park: wake rules

While a block ask is open:

| event | wakes the ship? |
|---|---|
| captain answer to the ask (direct or fanned out) | yes: the reply is normal mail, and the request wait reopens a scheduled task |
| a covering policy change (`permissions.set`, `/yolo`) | yes, once: the ask ends `policy changed`; a scheduled task reopens, an unscheduled task's owner gets a `normal` notice |
| an unrelated policy change | no: the ask's hash is updated and it stays open |
| new captain mail, or mail from another ship with no `about` this ask | yes: the existing "fresh mail earns one attempt" rule in `paused` |
| a request `about` the open ask | yes, once (change 6): requests keep their kind and urgency, so their sender's waits work; the fresh-mail rule bounds it |
| crew outcome of a job that raised or re-hit the ask | **no**: `crew::outcome` writes it `low` with `payload.block`, and the body names the ask |
| a notice to the blocked ship `about` the open ask ("still blocked?") | **no**: stored at `low` urgency |
| requester notices, attachments, desk receipts, closures | **no**: they are addressed to others or to the captain |
| scheduler retry of the parked task | **no**: the task waits on the request |
| hosted pane asleep (`alarm`) | only the "yes" rows: `alarm` honours `paused` |

A ship that is woken by something else while the ask is open sees it in its bearings, as one line: `Waiting on captain decision <id> (<rules>) for <task>; do not ask again.`

### Telegram: one card per ask

- **Primary ask.** One decision card, the existing `decision_text` and `decision_markup`, with Allow, Without and Stop buttons. The header names the ship; "Blocked:" lists the task.
- **Attachment** (`payload.attached_to` set). No new message. The door edits the primary card's text with `editMessageText`, adding "Also: storefront (relay), liaison (relay)", and **resends the card's `reply_markup`** (change 8), because an edit without it removes the buttons. A `fleet_key` attachment adds the other ship's task to "Blocked:". The door keeps a map from the primary's mail id to its message id, text and markup (`cards.json`). If the primary was pushed before the door kept that map, the attachment is skipped: it is stored, and `/waiting` counts it.
- **Closure.** A `request_end` notice for a card the door showed edits that card ("Resolved: policy changed") and clears its buttons instead of being pushed as a new message. A tap on a closed card keeps today's "already answered" fallback.
- **`/waiting`** lists primaries only, with "+N attached". `captain::waiting` gains `attached` counts and drops attached items from its list.

## Failure modes

- **Two denials of one task at the same time.** Both settle in `alter`'s immediate transaction. The second sees the first's open row, or hits the same deterministic id, and is a duplicate.
- **Two ships, one denial, at the same instant, in different homes.** Both may open as primaries before either commits. The desk re-checks `fleet_key` at `deliver`, so the later desk copy is attached, but the door may already have pushed both cards if it polled in between. Accepted; the card count is bounded at two.
- **A relay that arrives before the ask.** It has nothing to attach to and shows as its own card. Accepted.
- **An agent that does not set correlation when relaying.** Its message shows as a new card, as it does today. The brief line and the requester notice make this rare, not impossible.
- **Desk unreachable.** Attachments on the desk wait with the rest of the captain mail. Origin rows are unchanged, so nothing is lost.
- **Allow by relay or by a non-operator path.** Refused: only a reply carrying the runtime's operator marker adds a rule, and across machines only one delivered by the desk's machine.
- **A policy change while the ask is collected but not yet closed at the desk.** The withdrawal closes the desk copy through the closure paths. A captain tap in that window lands on a closed ask and gets "already answered".
- **Log missing, unreadable, or without our line at settlement.** No denial is found, so the outcome is today's `high` failed notice. That is a visible fallback, not a silent stall. The same holds for any error while writing the ask.
- **Cap.** At most 4 rules per ask, 5 requester notices, and an 8-hop chain. An ask past the cap lists the first four rules and counts the rest.

## Changes by file

- `src/block_ask.rs` (new): the denial line, `narrow_rule`, masking and preview, `settle`, the park, the declined table, the policy sweep and coverage test, answer application, topic resolution and attachment, fan-out, the bearings line.
- `src/permissions.rs`: `blocked` accepts agent-reported denials and writes a decision; `paused` accepts `decision`, uses the coverage test, and ignores crew and errand asks; `set` runs the sweep.
- `src/jobs.rs`: `alter` calls `block_ask::settle` in a savepoint before `crew::outcome`.
- `src/crew.rs`: a `low` outcome with `payload.block` for a job that raised or re-hit an ask.
- `src/scheduling/mod.rs`: a block ask withdrawn `policy changed` counts as ready; an in-transaction park for scheduled tasks.
- `src/lib.rs`: `agent.start` refuses a task with an open crew block ask unless `replace`.
- `src/mail.rs`: `about` and `attached_to` at `send` and `deliver`; the result `note`; the operator marker; `low` storage of notices `about` an open ask; hooks for answer application and fan-out.
- `src/machines.rs`: the runtime-stamped `via` machine on both link paths.
- `src/captain.rs`: `relay` writes without the marker; `waiting` counts attachments.
- `src/host/sleep.rs`: `alarm` honours `paused`.
- `src/context.rs`, `src/plugins.rs`: the bearings line; the brief lines for relays and blocks.
- `plugins/claude_turn.py`: the bounded denial line; `report_denial` removed.
- `plugins/telegram/door.py`: edit the card for attachments (keeping its buttons) and closures; `/waiting` counts.
- `docs/tool-permissions.md`: describe the decision, the park and the options.

## Test plan

Fake harnesses as in `tests/permissions.rs` (`FAKE`), which replays the shape of the captured fixture. Isolated homes throughout.

`tests/permissions.rs`:

1. A crew's denial becomes one typed decision with tool, rule, masked preview and `job_id`. The task waits on it, and the first mate is not woken by the crew's outcome.
2. The same denial on a retry is one ask (`duplicate`).
3. An `allow` from a relayed or non-operator reply is refused; the operator's own `allow` adds the rule.
4. An unrelated policy change keeps the ask open; a covering one ends it as `policy changed`.
5. `agent.start` is refused on the parked task; `replace: true` starts a crew and leaves the ask open.
6. An errand's denial raises an ask without parking the task.
7. The existing first-mate denial test keeps its pause, with the planted secret masked.

`tests/mail.rs`: a relay correlated to the blocked ship's notice is attached, its sender gets the `note` ("added to <id>"), and `captain::waiting` shows one item with `+1`; answering the primary fans out with `fanned_from`; a second ship's same denial attaches by `fleet_key`; a parked ship is not woken by its own still-blocked notices.

`tests/machines.rs`: a relay from a ship on the other machine is attached on the desk copy, and a marker that did not come from the desk's machine is dropped.

`tests/telegram_door.py`: one ask and a relay give one card that is edited with its buttons kept and no new message.

## Decisions taken

1. **Denied input in captain mail:** the matched rule plus a masked preview of at most 160 characters; never credentials (first mate, for the captain).
2. **Auto-mode refusals:** captured, not observed; the `denial` source ships alone.
3. **Finding 7 in this PR:** yes.
4. **Pause scope:** a crew denial parks only its task; a first mate's own wake denial pauses the ship; errands follow change 7.
5. **Requester notices:** accepted, capped at 5, `low`; the request-to-task link is left for later.
6. **Fan-out of `stop` and `without`:** all three answers fan out; the card lists every blocked task and the options say the answer applies to all.
7. **Unscheduled tasks:** no scheduling row; the reply wakes the owner, and a covering policy change sends a `normal` notice.
