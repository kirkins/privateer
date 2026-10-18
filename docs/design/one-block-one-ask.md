# Design: one block, one ask

Status: design only, for review before implementation. Task `privateer-one-block-one-ask` (captain finding 13, notice d8789f3d). It also covers finding 7 (`privateer-permission-decision-detail`, a decision that names the denied tool and input, the narrowest rule, and the job). Read with [crew park and resume](crew-park-resume.md), [cross-machine fleet](cross-machine-fleet.md) and [tool permissions](../tool-permissions.md).

## Problem

One evening, one harness block reached the captain's Telegram three or more times:

1. The blocked ship's first mate is woken by its crew's failed job and mails the captain in free text ("Apify step denied, please allow").
2. The ship that asked for the work (storefront) sees the stall and mails the captain the same block.
3. Liaison sometimes relays storefront's relay.
4. Each retry or "still blocked" wake adds another message.

The runtime already has a narrow fix, but most of these paths go around it:

- **Headless Claude only.** `plugins/native_launch.py` hands a `--print` stream-json Claude run to `plugins/claude_turn.py`. `ClaudeTurn.observe` sets `self.denied` when the final `result` has a non-empty `permission_denials`. A denied run ends `native.turn.failed` (never `turn.completed`), and `report_denial` runs `permissions.blocked {"job_id"}` as a subprocess, then prints `native.permission_blocked`. No field of a denial is read; only "the array is not empty".
- **`permissions.blocked`** (`blocked`, `src/permissions.rs:148`) opens one captain `request` with `request_kind` `tool-permission` and `payload {type: tool_permission, policy_hash}`. It is keyed by (owner, task, `policy_hash`, episode) and returns `duplicate: true` while one is open. On purpose, it copies no denied input into mail. The test `native_permission_denial_becomes_one_question_and_pauses_automatic_retries` asserts that `PRIVATE_DO_NOT_MAIL` is absent.
- **`permissions::paused`** (`src/permissions.rs:192`) pauses the *whole ship* while that request is open under the current policy and no "fresh" mail has arrived. It is checked by `consider` (`src/activation.rs:626`), the scheduling `blocked` gate (`src/scheduling/delivery.rs`), hosted launch (`src/host.rs`), `sleep` (`src/host/sleep.rs:62`), upgrades (`src/host/upgrade.rs`) and Herdr (`src/watch.rs`). `alarm` (`src/host/sleep.rs:106`) does not check it, so an asleep hosted pane still wakes on any mail that is not a low notice.
- **The crew outcome still wakes the first mate.** A crew job ended by a denial is `failed`, and `crew::outcome` (`src/crew.rs:98`) writes it at `high` urgency, so the first mate wakes, reads "failed", and mails the captain itself (path 1).
- **The task is not parked.** `task.defer` already supports `wait.kind: "request"` on the task's own open request (`defer`, `src/scheduling/commands.rs:66`; resolved in `src/scheduling/mod.rs` around 213). The permission path does not use it, so the first mate can relaunch a crew into the same denial.
- **Hosted and attended Claude are blind.** `plugins/attended-claude/hooks/register.js` handles session, prompt, turn and compact events only. Nothing in `src/host/` sees a denial or an auto-mode refusal. Those ships report blocks in free text.
- **The captain side has no topic merging.** `mail.send` (`send`, `src/mail.rs:802`) dedupes only an identical explicit `id`. There is no `thread` column; "thread" is the request that `correlation` names. The Telegram door (`plugins/telegram/door.py`) dedupes by mail id (`sent.json`), never edits a card's text (only `editMessageReplyMarkup` after a tap), and shows a tool-permission request as an ordinary ❓ request with no buttons.

## Model

There are three parts, one per item in the finding:

- **(a) Block ask.** The runtime turns a harness denial into one typed **decision** to the captain and parks the task on it.
- **(b) Attach.** Captain mail on the same topic as an open ask is *attached* to it: recorded, but not shown as a new item. Answering the ask answers its attachments.
- **(c) Quiet park.** While the ask is open, nothing about the block wakes its ship. Only the answer, a policy change, or genuinely new mail does.

### (a) Block ask

**Detection moves to settlement.** Every job ending passes through `jobs::alter` (`src/jobs.rs:366`), which writes `crew::outcome` in the same transaction. A new `permissions::settle_block(store, &job)` runs there first, for wake jobs, crew jobs and errands alike. It reads the last `result` line of the incarnation's `stdout.log` (the job's own log; a stream line is at most 1 MiB, `MAX_LINE` in `plugins/native_turn.py`) and extracts the denials. Two consequences:

- A dispatcher restart between the job ending and the ask being written is recovered by `reconcile` (`src/jobs.rs:3088`), which settles through `alter` too.
- No denied input crosses argv: today's subprocess call passes only `job_id`, and the new path passes nothing.

`claude_turn.py` keeps failing a denied run and printing `native.permission_blocked`, and adds `"denials": <count>`. Its `report_denial` subprocess becomes a fallback only for runs that are not jobs; every headless run today is a job, so it can be removed once settlement is proven.

**Denial record**, one per distinct rule, at most 4 per ask:

| field | from |
|---|---|
| `tool` | `permission_denials[].tool_name` |
| `rule` | the narrowest allow rule (below), or null when none can be derived |
| `preview` | clipped, masked input (see open question 1) |
| `source` | `denial` (`permission_denials`) or `auto-mode` (open question 2) |

**Narrowest rule**, computed in Rust (`permissions::narrow_rule`) so one function is tested:

| tool | rule |
|---|---|
| `Bash` | `Bash(<first word> <second word>:*)`, or `Bash(<first word>:*)` when the second word starts with `-` or is a path or a quoted string |
| `Edit`, `Write`, `Read` | `<Tool>(<path>)`, made relative to the ship's folder when it is inside it |
| `WebFetch` | `WebFetch(domain:<host>)` |
| `mcp__*`, other tools | the tool name alone |

A rule is refused (null) if it would fail `permissions::validate`, or if it is broader than the tool name (for example `Bash(*)`).

**Payload.** The ask is `kind: "decision"` (PR 219's typed decision), not `request` with `request_kind`; a decision cannot carry `request_kind`. The queries in `blocked` and `paused` change from `kind='request'` to `kind IN ('request','decision')`, so asks opened before the upgrade still pause and resolve.

```json
{"type": "tool_permission", "policy_hash": "<sha256 of the policy>",
 "block": {"key": "<hex>", "fleet_key": "<hex>", "ship": "browser-mac", "task_id": "...",
           "job_id": "...", "harness": "claude", "denials": [{"tool": "Bash",
           "rule": "Bash(apify run:*)", "preview": "apify run actor/x --input …", "source": "denial"}]},
 "decision": {"question": "browser-mac: Claude was denied Bash(apify run:*)",
   "why": "Its crew for <task title> stopped at this tool. Retries are paused until you answer or the fleet policy changes (/yolo on also clears it).",
   "blocked": ["<task id>: <task title>"], "free_text_ok": true, "group": "tool-permission",
   "options": [
     {"id": "allow", "label": "Allow Bash(apify run:*)", "what_happens": "Adds the rule to this machine's fleet policy for every Claude ship; the task resumes."},
     {"id": "without", "label": "Continue without it", "what_happens": "The task resumes; its first mate is told the tool stays denied."},
     {"id": "stop", "label": "Stop the task", "what_happens": "The task is held (task.defer manual); its owner is told."}]}}
```

The ask follows the decision schema in `validate_decision`: question at most 200 characters, why at most 500, 2 to 8 options. When no rule can be derived, `allow` is left out and the card has two options. Several rules make a single `allow` that adds all of them, listed in `what_happens`. No option is `recommended`; the runtime cannot judge the tool.

**Keys and idempotence.**

- `key` = sha256(ship, task, sorted rules or tool names, `policy_hash`). The ask id stays `permissions-<sha256(ship\0task\0policy_hash\0episode)>` as today. That way, one task under one policy has one ask, whatever denial a retry or a resumed crew hits.
- The open check and the insert run in one `BEGIN IMMEDIATE` transaction (the one `alter` already holds). The body is built only from key fields, so a replay is the same message and `send`'s explicit-id rule returns the existing row.
- After the captain answers `without`, the denied rules are recorded as `declined` for that task (new table `permission_declined(ship, task_id, rule, policy_hash, ms)`). A later denial of declined rules only opens no new ask; its outcome tells the first mate "the captain declined <rule>; do it without".
- `fleet_key` = sha256(sorted rules, `policy_hash`): the same denial on two ships. The second ship still gets its own ask row, so its task can wait on it and its closure is local. That row is attached (part b) to the first one, so the captain sees one card.

**Park.** In the same transaction:

- The task gets a scheduling wait `{"kind": "request", "ref": <ask id>, "reason": "tool permission: <rules>"}`. A scheduled task is deferred through `defer`'s logic; an unscheduled one gets a scheduling row created in state `waiting`, so `reopen` delivers it back to its owner when the wait resolves. The task's status becomes `blocked`, as `task.defer` sets it.
- `agent.start` on a task with an open block ask is refused, naming the ask and its rules. `replace: true` lets a crew start anyway to try another way; the ask stays open. This mirrors the refusal on parked crews.
- The pause becomes per task when the denial came from a crew or errand: `paused` filters on the ask's `task_id`, and scheduling's `blocked` checks the task. A denial in the first mate's own wake job still pauses the ship, as today, because that wake would hit it again. See open question 4.

**Resolution.** Each answer is applied where the ask lives (its home, and its machine):

- `allow`:
  - The runtime adds the rule with `permissions::set` as operator (`claude_allow_add`), which changes `policy_hash`. The decision is `answered`, and the request wait reopens the task.
  - The rule is applied **only** for a reply written by the operator. A reply carrying `payload.relayed_by` (`captain.relay`, `src/captain.rs:109`) is a liaison passing the captain's words on, so the rule is not added. The ship is told to ask the captain to use `privateer permissions allow` or the card directly. An agent must never widen fleet permissions by relay.
- `without`: the task reopens, and `declined` is recorded. The first mate wakes with the answer as its mail.
- `stop`: the scheduling row goes to `stopped` (the same state `task.defer` gives a manual wait), and the owner gets the answer at `normal` urgency.
- Free text: the task reopens, and the first mate reads the text.
- **Policy change.** `permissions::set`, and each dispatcher tick in every home, end open block asks whose `policy_hash` no longer matches. They end through `end_request` with state `withdrawn` and reason `policy changed`. The request wait treats a block ask withdrawn with that reason as ready (`reopen`), not as `fault`. `push_closures` (`src/machines.rs:1641`) closes the desk copy as it does any other withdrawal.
- **Task closed.** `close_task` (`src/mail.rs:393`) already ends the task's open requests.

**Hosted and attended ships.** Their harness result is not visible, so round 1 gives them a structured way to report instead of free text:

- `permissions.blocked` accepts `{"task_id", "tool", "input"}` from the task owner when there is no `job_id`. It builds the same denial record, ask and park.
- The first-mate brief says: "If a tool is denied, call permissions.blocked, then stop; do not mail the captain about it."
- A hook in `plugins/attended-claude` that reports denials and auto-mode refusals automatically is round 2. It needs the event that the installed Claude Code emits for a refusal (open question 2).

### (b) Attach: one card per topic

**What "same topic" means.** It is resolved in the **sender's** home at `mail.send` (and `deliver`), and written to `payload.about = <ask id>` by the runtime. Rules, in order:

1. **Explicit:** `mail.send` accepts `about: <captain item id>`. The id must name an item in this home, or a mail this home holds whose `payload.about` or `payload.block` names it.
2. **Correlation chain:** follow `correlation` through mail in this home, at most 8 hops. If a hop is the ask itself, or carries `payload.about` or `payload.block` naming an ask, that ask is the topic. This is what links storefront's relay (correlated to browser-mac's notice) and liaison's relay of it (correlated to storefront's mail).
3. **Task link:** the message's `task_id`, or that task's parent chain within this home, has an open block ask.
4. **Same denial:** a new block ask whose `fleet_key` matches an open one.

`about` is stamped on **every** kind of mail, not only captain mail, so the chain survives hops between ships. Task ids do not cross homes (`send` refuses a foreign task id and points at `correlation`), so across ships only rules 1 and 2 apply. The brief adds one sentence for every role: "When you pass on another ship's message, set correlation to it."

**Telling the requesters.** When an ask opens, the runtime sends a `low` notice to the senders of open requests to the blocked ship (at most 5 senders, never `crew` or the captain):

> browser-mac is blocked on captain decision `<id>` (Bash(apify run:*)); the captain has it. If you add anything, cite `about: <id>`.

The notice carries `correlation` = their request and `payload.block` = the ask id. It does not wake them (`routine`, `src/activation.rs:311`), and it gives their later relay a chain to follow. There is no link today from an inbound request to the task it became, so this may notify a requester whose request is unrelated. See open question 5.

**Attach, not refuse.**

- Refusing makes agents rephrase and retry, which is the loop we are closing. The extra text may also carry something new.
- So captain mail of kind `request`, `decision` or `notice` whose topic is an open ask gets `payload.attached_to = <ask id>`, and its row is stored normally:
  - a request or decision stays `open`, so its sender's own waits and closures still work;
  - a notice has no state.
- `production_approval`, `needs_hands` steps and replies are never attached.
- If the ask has already closed, `about` is kept but nothing is attached, and the mail shows as an ordinary item.

The sender's `mail.send` result gains:

```json
{"attached_to": "<ask id>",
 "note": "The captain already has this as <ask id> (browser-mac, Bash(apify run:*)). Your message was added to it; you will get the captain's answer. Do not resend."}
```

**Fan-out.** When the primary ask is answered, the runtime sends the same answer to each attachment that is still open, in the home that holds it:

- typed decisions with the same option ids get `payload.answer`;
- plain requests get the answer's text and option label;
- every fan-out reply carries `payload.fanned_from`.

`captain::relay` already finds a request in any fleet home and writes a captain reply there, so the same lookup is used. Ending the primary for a policy change or a closed task ends open attachments with state `withdrawn`, reason `merged ask <id> ended: <reason>`. An attachment answered directly (from `/waiting`) closes alone; the primary stays open.

**Across homes and machines.**

- *Same machine.* At send time, the open-item lookup that `captain::waiting` (`src/captain.rs:59`) uses finds the ask in any fleet home. `attached_to` is set on the row in the sender's home, which is the row the door pages.
- *Desk on this machine, ship elsewhere.* Mail collected by `collect` (`src/machines.rs:1304`) and placed by `captain_destination` (`src/machines.rs:1468`) carries `payload.about` from its origin. When `mail.deliver` writes the desk copy, it runs the same open-item lookup and sets `attached_to` on the desk copy.
- *Desk elsewhere.* `push_captain` (`src/machines.rs:1559`) → `fleet.captain` delivers to the desk, which does the same at its `deliver`.
- In each case the origin row keeps no `attached_to`, and the sender is not told at send time. It receives the fan-out answer, or the closure, through the existing reply and closure paths (`push_closures`, `apply_closure`, `close_captain_request`). Fan-out replies to a ship on another machine travel as captain replies over the link, the same way `a_captain_reply_reaches_the_home_that_asked_across_the_link` already does.
- `fleet_key` attaches across machines too. An `allow` fanned out to another machine's attached ask adds the rule there as well, because it is applied where each ask lives. That partly covers finding 8 (`/yolo` across machines) for this one case.

### (c) Quiet park: wake rules

While a block ask is open:

| event | wakes the ship? |
|---|---|
| captain answer to the ask (direct or fanned out) | yes: the reply is normal mail, and the request wait reopens the task |
| policy change (`permissions.set`, `/yolo`) | yes, once: the ask ends `policy changed`, and the task reopens through scheduling |
| new captain mail, or mail from another ship with no `about` this ask | yes: the existing "fresh mail earns one attempt" rule in `paused` |
| crew outcome of a job that raised or re-hit the ask | **no**: `crew::outcome` writes it `low` with `payload.block`, and the body names the ask |
| mail to the blocked ship with `about` = the open ask ("still blocked?") | **no**: stored as a `low` notice at `deliver` or `send` time |
| requester notices, attachments, desk receipts, closures | **no**: they are addressed to others or to the captain |
| scheduler retry of the parked task | **no**: the task waits on the request |
| hosted pane asleep (`alarm`) | only the "yes" rows: `alarm` adds the same `paused` and `about` checks |

A ship that is woken by something else while the ask is open sees it in its brief, as one line under bearings: `Waiting on captain decision <id> (<rules>) for <task>`. Then it does not ask again.

### Telegram: one card per ask

- **Primary ask.** One decision card, the existing `decision_text` and `decision_markup` (`plugins/telegram/door.py:740`, `:256`), with Allow, Without and Stop buttons. The header names the ship; "Blocked:" lists the task.
- **Attachment** (`payload.attached_to` set). No new message. The door edits the primary card's text with `editMessageText` (new in `plugins/telegram/tg.py`), adding "Also: storefront (relay), liaison (relay)". It does the same for a `fleet_key` attachment, adding the other ship's task to "Blocked:". The door keeps a map from mail id to message id beside `remember_target` (`plugins/telegram/tg.py:501`). If the primary was pushed before the door started, the attachment is skipped: it is stored, and `/waiting` counts it.
- **Closure.** A `request_end` notice (`payload.request_end`, `correlation` = the ended request) for a card the door showed edits that card ("Resolved: policy changed", "Answered: Allow …") instead of being pushed as a new message. A tap on a closed card keeps today's "already answered" fallback.
- **`/waiting`** (`cmd_waiting`, `plugins/telegram/door.py:610`) lists primaries only, with "+N attached". `captain::waiting` gains `attached` counts and drops attached items from its list.

## Failure modes

- **Two denials of one task at the same time.** Both settle in `alter`'s immediate transaction. The second sees the first's open row, or hits the same deterministic id, and is a duplicate.
- **Two ships, one denial, at the same instant, in different homes.** Both may open as primaries before either commits. The desk re-checks `fleet_key` at `deliver`, so the later desk copy is attached, but the door may already have pushed both cards if it polled in between. Accepted; the card count is bounded at two.
- **A relay that arrives before the ask.** It has nothing to attach to and shows as its own card. The ask then attaches to it only through rule 2, if the relay correlates to the ship's mail. Accepted.
- **An agent that does not set correlation when relaying.** Its message shows as a new card, as it does today. The brief line and the requester notice make this rare, not impossible.
- **Desk unreachable.** Attachments on the desk wait with the rest of the captain mail. Origin rows are unchanged, so nothing is lost.
- **Allow by relay.** Refused, as described under Resolution. Only an operator-written reply adds a rule.
- **A policy change while the ask is collected but not yet closed at the desk.** The withdrawal closes the desk copy through `push_closures`. A captain tap in that window lands on a closed ask and gets "already answered".
- **Log missing or unreadable at settlement.** No denial is found, so the outcome is today's `high` failed notice. That is a visible fallback, not a silent stall.
- **Cap.** At most 4 rules per ask, 5 requester notices, and an 8-hop chain. An ask past the cap lists the first four rules and counts the rest.

## Changes by file

- `src/permissions.rs`:
  - add `settle_block`, `narrow_rule`, the declined table, the policy-change sweep, and the `allow` application;
  - change the `blocked` and `paused` queries to accept `decision`;
  - add the `task_id` scope to `paused`;
  - accept agent-reported denials.
- `src/jobs.rs`: call `settle_block` in `alter` before `crew::outcome`.
- `src/crew.rs`: write `low` outcomes for a job that raised or re-hit an ask, with `payload.block`.
- `src/scheduling/commands.rs`, `src/scheduling/mod.rs`:
  - a runtime park on a request for unscheduled tasks;
  - `policy changed` counts as ready.
- `src/mail.rs`:
  - the `about` field and its resolution (explicit, chain, task);
  - `attached_to` at `send` and `deliver`;
  - the result `note`;
  - fan-out on reply;
  - ending attachments with the primary;
  - storing mail `about` an open ask as `low` for its blocked ship.
- `src/machines.rs`: the desk-side attach check where `captain_destination`'s delivery writes the copy.
- `src/captain.rs`: `waiting` counts attachments; relayed answers carry `relayed_by` (already).
- `src/host/sleep.rs`: `alarm` honours the pause and `about`.
- `src/activation.rs`, `src/context.rs`: the bearings line; the brief lines for relays and blocks.
- `src/door.rs`: no change expected; `poll` already passes the `request_end` notice that `end_request` writes, whose `correlation` is the ended request's id.
- `plugins/claude_turn.py`: the denial count; `report_denial` becomes a fallback.
- `plugins/telegram/door.py`, `plugins/telegram/tg.py`: edit the card for attachments and closures; `/waiting` counts.
- `docs/tool-permissions.md`: describe the decision, the park and the options.

## Test plan

Fake harnesses as in `tests/permissions.rs` (`FAKE`), extended so that a denial is skipped only when argv's `--allowedTools` covers the rule. Isolated homes throughout.

`tests/permissions.rs`:

1. A crew's denial becomes one typed decision with tool, rule and `job_id`. The task waits on it, and the first mate is not woken by the crew's outcome.
2. The same denial on a retry and on a resumed crew is one ask (`duplicate`). After `without`, a repeat of the declined rule opens nothing.
3. Answering `allow` adds the rule, reopens the task, and the next crew run succeeds. A `captain.relay` answer choosing `allow` adds nothing and says why.
4. `permissions yolo` ends the ask as `policy changed`, and the ship is woken once.
5. A dispatcher restart between the job ending and settlement raises the ask once, at reconcile.
6. `agent.start` is refused on the parked task; `replace: true` starts a crew and leaves the ask open.
7. A hosted owner's `permissions.blocked {"task_id","tool","input"}` makes the same ask.
8. The existing `native_permission_denial_becomes_one_question_and_pauses_automatic_retries` is updated per open question 1.

`tests/mail.rs`:

1. A relay correlated to the blocked ship's notice is attached, its sender gets the `note`, and `captain::waiting` shows one item with `+1`. Liaison's relay of that relay attaches too.
2. Explicit `about` and the task link attach. An `about` naming a closed ask makes an ordinary item. `production_approval` never attaches.
3. Answering the primary fans the answer out to open attachments with `fanned_from`, and ending the primary ends them.
4. Two ships with the same rule: one card, and an `allow` reopens both tasks.

`tests/machines.rs`:

1. A relay from a ship on the other machine is attached on the desk copy, and the answer reaches that ship across the link.
2. A policy change on the origin machine closes the desk copy and its attachments.

`tests/activation.rs` and `tests/hosted.rs`: mail `about` the open ask and the `low` crew outcome do not wake a ship or an asleep pane; the answer does.

`tests/telegram_door.py`: one ask and two relays give one card that is edited twice and no new messages. An Allow tap is accepted. The closure edits the card. `/waiting` shows `+2`.

## Open questions

1. **Denied input in captain mail.** Finding 7 asks for "the exact denied input". Today's invariant, tested with `PRIVATE_DO_NOT_MAIL`, is that none reaches captain mail, and captain mail goes to Telegram. Even a rule leaks the command's first words. Proposal: the full input stays in the job log, and the card carries the rule and a 160-character preview with secret-shaped tokens masked (long hex or base64, `KEY=value`, `Bearer …`, URL credentials). The test then asserts a planted secret is masked rather than the whole command being absent. This reverses a captain-set invariant, so it needs the captain's decision. If the answer is no, the card carries only the tool name and a rule *kind* (for example "a Bash command prefix").
2. **Auto-mode refusals.** It is unverified whether Claude's auto mode reports its refusals in `permission_denials` for `--print` runs, and which hook event an interactive session emits for one. The implementer captures a real stream from the installed Claude and adds it as a fixture before writing the `auto-mode` source. Round 1 ships without it if the stream does not carry it.
3. **Finding 7 in this PR?** Recommended **yes**. The tool, rule, preview and `job_id` come from the same settlement parse, and the ask key depends on the rules. Folding it in avoids changing the payload twice. It depends on question 1; if that is undecided, ship the tool and rule-kind fields and leave the preview to finding 7.
4. **Pause scope.** Crew denials park only their task; a first mate's own denial keeps today's ship-wide pause. Confirm. The alternative is to keep the ship-wide pause for every denial, which is simpler and stops more unrelated work.
5. **Requester notices.** Without a link from an inbound request to the task it became, the notice goes to every open requester of the blocked ship (at most 5). Accept the noise (low notices, no wake), or add an explicit link: a reply or `task.update` field that records which request a task serves.
6. **Fan-out of `stop` and `without`.** A `fleet_key` attachment gets the same answer as the primary, so stopping one ship's task stops the other's. The card lists both tasks under "Blocked:". Confirm, or fan out only `allow`.
7. **Unscheduled tasks gain a scheduling row** when parked. That makes the reopen deliver them back to the owner, which is the wake we want, but it changes how `task.list` and bearings show them. Confirm.
