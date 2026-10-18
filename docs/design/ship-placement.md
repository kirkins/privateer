# Proposal: ship placement

Status: a proposal (captain findings 5 and 6, task `privateer-ship-placement`).
Nothing here is implemented. It builds on [ship moves and machine owners in
the architecture](../architecture.md#linked-machines), the [flotillas
proposal](flotillas.md) (machine owners, the disk floor) and the
[cross-machine fleet proposal](cross-machine-fleet.md) (links and gateways).
Names marked *(new)* do not exist yet; every other file, function and
command named here does.

## The case that motivates it

`privateer ship move NAME --to MACHINE` works, but it knows only two things
about what a ship needs: its first mate's harness and plugin, and that the
other machine can fetch its projects (`begin` in `src/moves.rs`). An iOS app
ship moved to the Linux laptop arrives, starts its dispatcher, and fails at
its first build because there is no Xcode. A ship whose crew drive Chrome, or
whose work needs `kubectl` and a cluster context, or a harness login that
only one machine holds, has the same problem, and nothing records it.

The other half is pressure. The Mac fills its disk, its capacity queue backs
up, and its owner is told (`alert_owner` in `src/machines.rs`), but the only
remedies on offer are cleanups and waiting. Sometimes the right answer is
that one ship should live elsewhere. The owner sees the pressure; the captain
decides where ships live.

Two changes, one PR each:

1. **Declared needs.** A ship declares what it requires, machines offer what
   they have, and a move to a machine missing a need is refused by name
   before anything stops.
2. **Owner-proposed moves.** Under persistent pressure, a machine's owner
   may propose moving one idle ship whose needs another machine meets. The
   proposal is one captain decision; nothing moves without the captain's yes.

Running single crew jobs elsewhere instead is compared at the end.

## Declared needs

### The vocabulary

A need is a short name from a vocabulary the captain keeps: `xcode`,
`chrome`, `kubectl`, `docker`, `login-claude`, `iphone-device`. The
vocabulary is data in the fleet registry, not code, so the captain extends it
without a release (all *new*):

```
privateer capability put xcode --probe-path xcodebuild --note "Xcode and its CLI tools"
privateer capability put kubectl --probe-path kubectl
privateer capability put login-claude --probe-argv claude,auth,status
privateer capability put iphone-device --declared-only --note "a phone on USB"
privateer capability list | remove NAME
```

`capability.put` *(new, operator only)* writes a row in a `capabilities`
table *(new)* beside `machines` in the fleet registry (initialised in
`src/machines.rs` `init`): the name (an `identifier`), a note, and at most one
probe. `init` seeds no terms; the captain adds the few the fleet uses. The
vocabulary is per fleet, so the linking machine's vocabulary travels to the
linked machine with the directory exchange (`exchange` in `src/machines.rs`,
the same frame that already carries stows), and the linked side keeps the
copy it was sent; `capability.put` there is refused with "define it on the
linking machine".

"Capability" is the stored noun because `runtime.needs` (`src/needs.rs`)
already means captain attention; the ship-facing words stay `requires` and
`offers`.

### Where requires live

On the ship's record, beside `consumes`. `ship.requires {name, requires,
pinned?}` *(new)* follows `set_consumes` in `src/fleet.rs` exactly: a ship
may set its own, otherwise its creator, a grantee holding the new grant
action `ship.requires` (added to `ACTIONS` in `src/grants.rs`), or the
operator. It refuses a term the vocabulary does not hold, so a typo fails at
once, caps the list at 16, writes the ship's own record and every other home's
directory entry on this machine, and reaches linked machines on the next
exchange, as `consumes` does. `ship.show` and `ship.list` show it.

The record, not the charter, holds it: a charter decides authority, and a
need grants nothing. The record also travels in every move's snapshot.

`pinned` is an optional reason ("simulator data lives on this Mac", "the
signing keychain"). An operator's `ship move` still moves a pinned ship but
shows the reason and requires `--unpin` *(new flag)*; an owner never
proposes it (below).

A ship that declares nothing stays movable exactly as today. The first
mate's harness and plugin stay implicit needs, checked as now.

### Where offers come from

A machine offers a term when its probe succeeds or the operator declared it:

- **Detected.** A probe is one of: a binary on the login shell's PATH
  (`--probe-path`, checked with `on_path_in` and `login_path`, as `begin`
  already checks the harness binary), an existing path (`--probe-file`), or
  an argv run with the login PATH, no stdin, a five-second timeout, output
  discarded, success meaning exit 0 (`--probe-argv`). A probe never reads a
  credential's value; "is there a login" is the harness's own status command
  answering yes or no.
- **Declared.** `machine.offers {add?, remove?, withhold?}` *(new, operator
  only, run on the machine itself)* records offers a probe cannot see (a
  phone on USB, a hardware key) and withholds ones the captain does not want
  used here even though they are installed. Offers = detected ∪ declared −
  withheld.

Detection is cached for at most ten minutes in a `machine_offers` table
*(new)* and reported in each directory exchange, so `machine list` shows what
each linked machine offers and proposals (below) can be planned without a
gateway call. The cache is only for planning: the preflight always probes
live.

### Ship move preflight

Today the target's checks run in `move.begin`, after `move.export` has
stopped the ship's dispatcher, so even a refusal briefly stops the ship. The
preflight moves first:

1. `run` in `src/moves.rs` calls `move.check {ship, requires, first_mate,
   projects}` *(new gateway call, served by `serve`)* on the target before
   `move.export`. The target probes each required term live and answers
   `{met: [...], unmet: [{need, why}], free_disk_bytes}`. `why` is "not in
   this machine's vocabulary", "probe failed: xcodebuild is not on PATH", or
   "withheld here".
2. Any unmet need refuses the move with every unmet need named: "dev-app
   stays on mac: laptop does not offer xcode (xcodebuild is not on PATH),
   iphone-device (not declared there)". Nothing has stopped, so nothing
   restarts.
3. `move.begin` repeats the check, because time passes between the two
   calls; a refusal there is undone as today.

`privateer ship move NAME --to MACHINE --check` *(new flag)* runs only the
preflight and the existing idle test without stopping anything, and prints
met and unmet needs, the folder's size and the project count. Placement
proposals use the same call.

## Owner-proposed moves

### Persistent pressure

Pressure is the owner alerts that already exist, held for a while. Each
raised alert is a row in the owner's `machine_alerts` table with
`raised_ms`, cleared when the problem clears (`alert_owner`). The keys that
count:

| key | raised by `host_problems` when |
|---|---|
| `disk_floor` | free disk is below `min_free_disk_bytes` |
| `disk_trend` | free disk will reach the floor within `disk_warn_horizon_secs` |
| `capacity_wait` | the capacity queue's head has waited `LONG_WAIT_MS` (10 min) behind an unhealthy holder, or behind none |
| `load` *(new)* | the load alert of finding 15, once it exists; it does not today |

`route:NAME` does not count: a provider backing off is not the machine's
fault, and moving a ship does not change its route.

The machine is under **persistent pressure** when one of those keys has been
raised continuously for `pressure_after_secs` *(new `resource.configure`
key, default 4 hours)*. A flapping alert resets the clock each time it
clears, which is deliberate: an alert that clears on its own is not a reason
to move a ship. The owner's notice for that key gains one sentence once the
threshold passes: "This has lasted four hours; `ship.placement` lists ships
another machine could host."

### Candidates

`ship.placement {}` *(new, owner or operator, read only)* answers, for this
machine: the pressure keys and how long each has been raised, and for each
local ship whether it is eligible and, if so, which linked machines meet its
needs. A ship is **eligible** only if all hold:

- **Idle.** No wake, job, errand, held workspace or live session (the
  `busy` test in `src/moves.rs`), no task of its own `running`, no project
  with uncommitted or unpushed work (`projects`), and no first-mate wake in
  the last hour.
- **Not pinned.** No `pinned` reason on its record.
- **Not moved recently.** `last_moved_ms` *(new, stamped on the ship's own
  record by `move.activate`, so it travels with the ship)* is more than 24
  hours ago.
- **Not the owner itself**, and not the captain's home (the liaison).

A **target** is a linked machine reached from here, whose cached offers meet
every requirement, that reports no raised pressure key itself (the exchange
carries the keys), and whose free disk after the move stays above its floor
by the folder's size plus the projects' size on this machine.

The owner's first mate chooses; the runtime only lists, with each ship's
disk use and its share of the host ledger's last day of jobs.

### The proposal

`ship.propose_move {ship, to, why}` *(new, the machine owner only)* checks
eligibility and the target again, runs `move.check` against the target, and
writes one captain `decision` (`payload.decision` with options, checked by
`validate_decision` in `src/mail.rs`) from the owner, on its duty task. The
runtime composes the body so every proposal has the same shape:

> **Move dev-app from mac to laptop?** mac has been below its disk floor for
> 6 hours (1.2 GiB free, 2 GiB floor). dev-app is idle, last worked 3 hours
> ago. laptop offers everything it requires (chrome, login-claude).
> **Cost:** dev-app is down for the move (about 4 minutes for 180 MiB and
> 2 projects); its projects are cloned again on laptop (about 3.4 GiB there,
> freed here once the folder is cleaned); build caches start cold; its first
> mate's session starts fresh on laptop. *Why (owner):* {why}
> Options: **Move it** · **Not now** · **Never this ship**

`payload.placement` *(new)* carries the typed facts: ship, from, to, the
pressure key and its duration, the folder and project sizes, and the move
check's answer.

**Bounds.**

- At most one open proposal per machine. A second is refused while one is
  open, whoever answers first.
- A proposal expires unanswered after 48 hours, which counts as "Not now".
- After "Not now" the same machine proposes nothing for 24 hours, then
  doubles per consecutive "Not now" (48, 96 hours, at most 7 days). A "Move
  it" resets the backoff.
- "Never this ship" sets the ship's `pinned` to "the captain declined moving
  it" (the captain can clear it with `ship.requires`).
- Proposal state lives in a `placement_proposals` table *(new, fleet
  registry)*, so the bounds hold across dispatcher restarts and survive the
  owner's own move.

### The captain's yes

Nothing moves without it. "Move it" records a one-shot approval bound to
the proposal id, ship, source and target, valid for 6 hours. Only the
linking machine can run a move (`plan` and `run` in `src/moves.rs`), so its
dispatcher picks the approved proposal up from the answer as it arrives (the
answer travels as ordinary mail and collected mail), revalidates eligibility
and runs `move.check` again, then runs the ordinary `ship.move`. If the ship
became busy, gained a need the target lacks, or the approval expired, the
move does not run: the owner and the captain get one notice naming why, and
the proposal closes without a backoff. The result (moved, or refused and
why) is a reply on the decision's thread.

The approval authorizes that one move and nothing else; it is not a grant,
and it is never reused. This mirrors how a production approval is bound to
one artifact and environment.

## Moving the ship or running jobs elsewhere

The alternative is to keep the ship where it is and run individual crew jobs
on the machine that has what they need. They answer different problems.

| | Move the ship | Run a job elsewhere |
|---|---|---|
| Fixes | where a ship lives | where one piece of work runs |
| Good for | a ship that wants another machine all the time; relieving a machine | one need the ship's machine lacks (an Xcode build from a Linux ship); bursts |
| Repository | cloned once on arrival | needed on the far side per job: a workspace at a pushed commit |
| Routes and logins | the target's own, once | the far machine's, per job, with route enforcement there |
| Results | already local | carried back: status, bounded log tail, commits on a branch |
| Custody | unchanged; the ship and its tasks move together | a task here, a job there: park and resume across a link |
| Cost | minutes of downtime, a fresh clone, cold caches, a fresh first-mate session | per job: fetch, build cold or warm a remote cache, logs over the gateway |
| Exists | `ship.move`, plus the preflight above | none; the gateway lets agents only deliver mail |

For a job elsewhere the runtime would need: a remote job command on the
gateway that takes argv, a commit and a project URL, admitted by the far
machine's own ledger and capacity queue; a workspace on the far machine owned
by a hosting ship there (its owner by default) so cleanup has an owner; crew
routes and harness logins there, enforced there, if the job is a crew rather
than a build; outcome and a bounded log tail returned as a crew outcome
notice, with commits pushed rather than copied; and `crew.park` extended so a
crew here parks on a job there and resumes after a link outage, with
duplicate outcomes refused by id.

**Recommendation.** Build declared needs and owner proposals now; they use a
move that already works and give the captain placement without new
transport. Later, if a ship really wants two machines, build a remote
**heavy job** (argv at a pushed commit, status and log tail back) before any
remote **crew**: it needs no routes or logins on the far side, which is
where most of the risk sits. A remote crew comes only if remote heavy jobs
prove too narrow.

## Failure modes

- **A probe lies.** `xcodebuild` is on PATH but no SDK is selected. The
  captain sharpens the probe (`--probe-argv xcodebuild,-version`); the
  ship's first failed build still reports the real error.
- **A probe hangs or is slow.** Each probe has a five-second timeout and a
  check runs at most 16, within the slow-call allowance `move.begin` has.
- **Offers change between check and move.** `move.begin` checks again; the
  cache is never trusted for a move.
- **Vocabulary drift across machines.** The linked side holds the linking
  side's copy; a term missing there reads as unmet with that reason.
- **Two owners propose the same ship.** Only the ship's own machine's owner
  may propose it, and one open proposal per machine bounds the rest.
- **The answer arrives while the linking machine sleeps.** The approval
  waits for its dispatcher, within its 6-hour validity; after that the
  captain is told it lapsed.
- **Interrupted move.** Unchanged: `run` resumes it, and `last_moved_ms` is
  stamped only on activation, so a failed move does not start the 24-hour
  clock.
- **Ping-pong.** A target must report no pressure, and the 24-hour rule
  holds, so a ship is not bounced between two strained machines.

## Tests the implementation needs

Declared needs (PR 1), with two registries and homes in separate directories
over a real gateway subprocess, like the existing move tests:

- An undeclared ship moves exactly as before.
- A ship requiring a term the target lacks is refused before its dispatcher
  stops, naming every unmet need; its dispatcher was never stopped.
- A declared offer meets a need; a withheld one fails it though its probe
  passes; a probe that exits nonzero or outlives its timeout is unmet.
- `ship.requires` refuses unknown terms, more than 16, and callers without
  authority; it reaches other homes and linked machines on exchange.
- The vocabulary defined on the linking machine arrives on the linked one;
  `capability.put` on the linked side is refused.
- `move.begin` re-checks: removing an offer between `move.check` and
  `move.begin` undoes the move and restarts the dispatcher.
- `--check` changes nothing.

Owner proposals (PR 2):

- Pressure starts only after `pressure_after_secs` continuous; a cleared and
  re-raised alert restarts the clock; `route:` keys never count.
- Each eligibility rule excludes a ship: running task, busy wake or job,
  dirty project, pinned, moved within a day, the owner, the liaison.
- A target under pressure, or without room, is not listed.
- A second proposal while one is open is refused; backoff doubles after
  each "Not now" and an expired proposal counts as one; "Move it" resets it.
- "Move it" runs the move once on the linking machine, including when the
  owner is on the linked machine; a replayed answer does not move it twice.
- A ship that became busy after the yes is not moved, and the captain is
  told why; an approval older than 6 hours is not used.
- Proposal state survives a dispatcher restart and the owner's own move.

## Order of work

1. **Declared needs**: the vocabulary, `ship.requires`, `machine.offers`,
   offers in the exchange, `move.check` before export, `ship move --check`.
2. **Owner proposals**: pressure, `ship.placement`, `ship.propose_move`, the
   bound approval run by the linking machine, `last_moved_ms`.
3. **Later, if needed**: remote heavy jobs, then remote crew.

## Open questions for the captain

- Should `init` seed a few common terms (`xcode`, `docker`, `chrome`) the
  captain can remove? Recommended: no; the fleet's vocabulary should be the
  captain's words, and an empty list costs one command per term.
- Is 4 hours of continuous pressure the right threshold, and should a
  flapping alert (raised and cleared several times a day) also count?
  Recommended: 4 hours continuous for now; add a daily total only if real
  pressure turns out to flap.
- After "Move it", should the runtime run the move, or only tell the
  operator to run it? Recommended: the runtime, bound to that one proposal,
  because the point is relief while the captain is away.
- Should the captain be able to give an owner a standing grant to move ships
  without asking each time? Recommended: no, not in this step; placement is
  the captain's call, and proposals are cheap to answer.
- Should a ship's requirements also steer `ship.create {machine}`, refusing
  to create a ship where it cannot work? Recommended: yes, as a small
  follow-up once `ship.requires` exists.
