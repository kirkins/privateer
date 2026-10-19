# Proposal: a first mate's attention

Status: historical proposal. Errands, backlog ordering and attention reports are implemented; [automatic prepared-work scheduling](../scheduling.md) adds durable pickup and resumption.

## What happened

After the update on 16 October 2026 the privateer ship worked without a break,
and its live Pi session hit Pi's compaction limit mid-task (258k of a 272k
window). Raising or lowering when it compacts would treat the symptom. The
session's own record shows the cause. In the 52 minutes that filled that
window:

| Into the first mate's context | Calls | Characters |
|---|---|---|
| Job status and logs of checks it had queued | 30 | 180k |
| Source files it read | 29 | 107k |
| Searches | 16 | 47k |
| Other tools | 8 | 47k |
| Its own words | | 3k |
| Mail | | 3k |

It touched 14 tasks. Crew did the implementing, but the first mate reviewed
by pulling their raw results into its own thread. Across the thread's eight
compactions Pi's summary grew every time (21k, 35k, 51k, 62k, 70k, 82k, 83k,
95k characters), because each compaction folds in the previous summary. A
thread that never ends can only start each window fuller than the last,
wherever the trigger sits.

Headless wakes do not have this problem: context follows the focus, so each
task's work happens in that task's own short session, started from its note
and pins, rotated after a few wakes. Hosting a live first mate (#95) put every
focus back into one conversation.

## Three scarce things, regulated by measurement

A fixed cap on active tasks fails the way it failed in First Mate: blocked
work holds slots, so the cap gets raised until it means nothing, and the right
number differs by model speed. The fleet has three scarce resources, and each
needs its own regulator, measured rather than counted:

| Resource | Regulated today | By |
|---|---|---|
| The machine: memory, CPU, later simulators and Xcode | Yes | Admission from live readings (#109) |
| The model: rate limits, latency, spend | No | Back off when the provider throttles or slows |
| The first mate's attention: its context | No | This proposal |

Blocked work must never hold a place. Privateer already mostly gets this
right: a blocked crew member ends and records its blocker (a `waiting` pin or
a blocked task), and the mail that clears the blocker wakes the ship again.

## Proposal

### 1. Errands: the first mate's thinking in a fresh context

An **errand** is a short, headless model session the first mate starts for one
focus, to think something through away from its own thread: review a crew's
result, investigate a failed check, catch up on a long request thread, prepare
a pull request. It is not crew: crew implement in a workspace, while an errand
runs as the ship under the ship's charter, mostly reads, runs anything heavy
through `job.start` admission, and returns a verdict.

- `errand.start {focus, brief}` starts one. Its first prompt is the focus's
  note, pins, assignment and the brief; it does not see the first mate's
  thread.
- When it ends, the verdict, a few paragraphs, arrives as mail from `crew` on
  that focus, and exact facts it found are pinned. The live thread receives
  only the verdict.
- A follow-up errand on the same focus resumes its session while the prompt
  cache is warm (review, fix, review again), and rotates after a few rounds,
  the rule headless wakes already use (`cache_window_secs`,
  `session_max_wakes`).
- It is visible without a pane: the ship's status in Herdr's sidebar and on
  the dashboard reads "reviewing dashboard-102", and `wake.list` lists it.

The brief changes with it: a first mate does its reviewing through errands,
and reads raw logs or large diffs in its own thread only for a quick check.

### 2. The live first mate keeps the portfolio

The hosted session becomes the conversation with the captain and the overview
of the ship's work: what is in flight, what waits, what comes next. Detailed
work happens in crew and errands, each in its own context, so the live thread
grows by verdicts, not by logs, and its compactions stop compounding.

When it does compact, the summary should be the portfolio, not a history:
quiet compaction's instructions (#116) already ask for open captain questions
and ids. They should also ask it to leave out detail that lives in notes,
pins and errands.

### 3. Attention pressure decides how much the ship takes on

The ship measures its first mate's attention instead of counting tasks:

- context added per hour of activity in the live session (Pi reports usage
  after every turn);
- compactions per hour, and the size of the last summary;
- errands and crew waiting for review.

While pressure is low, the first mate starts the next task from the backlog
when something finishes. While it is high, it starts nothing new, delegates
pending reviews to errands, and the bearings say so ("attention: high; 3
reviews pending"). A slow model fills its context more slowly and so takes on
more at once, and a fast one less, without any number to tune per model.

### 4. A backlog the captain can see

"Queue as many as you want" needs a queue: tasks marked `open` and not yet
started, in an order the captain and the first mate can see and change
(`task.priority`). The dashboard and `/status` show it next to what is
running and what waits. The first mate pulls from the top when attention
allows; a blocked task leaves the running set and records its blocker, so it
never holds a place.

### 5. A busy hosted Claude first mate acts at a context line

Quiet compaction (Pi only) needs the session idle for `quiet_compact_secs`, and
a busy first mate is never idle that long. A hosted Claude first mate has no
quiet compaction at all, so it grew to 60% of a 1M window with nothing reducing
it, and every turn re-read the whole of it.

The second trigger is a hard line, not an idle period:

- at `context_warn_percent` (default 40) the bearings' attention line says what
  the crossing will do: a resumable first mate keeps its notes current and is
  compacted, a bounded one writes handoff notes now because its session ends;
- a settled turn (its answer ended, no tool pending) reported by the session
  holding the lease, at or past `context_compact_percent` (default 50), acts:
  - a resumable first mate (no `tool_bundles`) is compacted at the turn's end
    through its adapter's session API (the call `/compact` makes between turns:
    no model turn, and not a prompt), and keeps its conversation;
  - a bounded first mate (`tool_bundles`, which cannot resume) gets a request in
    `hosted_relaunches`. The pane's supervisor takes it at the next safe point,
    busy or idle, and the bounded launch that follows is a fresh session seeded
    from the handoff notes;
- one crossing asks once per outcome. A session is armed by one of its own
  samples below the line; the first settled turn at or past the line while armed
  asks, and the crossing stays armed until the holder reports a compaction (the
  `compaction` kind), which disarms it. A sample below the line re-arms it. So a
  declined compaction (the pane was busy, had a draft or mail, or the session
  refused it) asks again at the next settled turn past the line, and a
  compaction that leaves the context above the line does not compact again.
  A bounded session starts unarmed, so a fresh launch past the line never loops;
  a resumable launch that starts past the line arms at its start, since a
  compaction is at most once per launch;
- a crossing is held back, and the session stays armed, while a relaunch request
  is pending, the captain has a tool question open, or a crew report is unread.
  A bounded relaunch request that expires unfulfilled asks again in the same
  session at the next settled turn past the line;
- only hosted first mates on Claude qualify. Attended (unhosted) first mates
  get neither the warning nor the action.

Audit records `host.context_compact` with the action (`compact` or `relaunch`),
the tokens, window and line. Limitation: a crew decision is held back only
through the unread crew reports it arrives as, not by a separate check for an
open decision. Limitation: the session-API compaction is verified only against a fake
session in `tests/claude-attended.mjs`, not a live Claude pane; a declined
compaction asks again at the next settled turn past the line.

## What this does not do

- No fixed cap on tasks, crew or errands.
- No change to admission: errands are light, and their heavy checks are
  admitted like any job.
- No new authority: an errand acts as its ship, within its charter.

## Order of work

1. Errands, with their visibility and the brief change. This alone keeps raw
   output out of the live thread.
2. Attention pressure in the bearings, and the brief's rule to pause new work
   while it is high.
3. The visible, ordered backlog.
4. Model backoff on throttling and latency, which also protects crew and
   errands.

## Open questions

- Should an errand run on the ship's model, or a cheaper one chosen for
  reviews? Recommended: the ship's model by default, with an optional errand
  profile, since review quality matters.
- Should the live thread ever start fresh? With errands it should grow slowly;
  if summaries still compound, a fresh live session seeded from bearings,
  notes and pins (as headless wakes start) is the next step, at a quiet moment
  and never mid-conversation with the captain.
- What pressure thresholds? Start by recording the measures for a week of
  real use before choosing them.
