# Proposal: ship placement

Status: a proposal (captain findings 5 and 6, task `privateer-ship-placement`),
revised after design review. Declared needs (PR 1) are implemented as described,
including `ship.create` refusing a machine missing a need (decision 5). Owner
proposals (PR 2) are implemented as described, with the captain's decisions 2,
3, 4 and 6 as recommended; the approved move runs in a detached runner a
dispatcher pass starts (never in a dispatcher, which the move may stop), only
the captain's reply answers a decision, and the captain's answers are read where the proposal's owner is (a forged
"not now" or "never" can only hold a ship back). `ship.propose_move` takes the
ship as `name`, since an agent's `ship` argument always names itself. It builds on [ship
moves and machine owners in the architecture](../architecture.md#linked-machines),
the [flotillas proposal](flotillas.md) (machine owners, the disk floor) and the
[cross-machine fleet proposal](cross-machine-fleet.md) (links and gateways).
Names marked *(new)* do not exist yet; every other file, function, table and
command named here does.

## The case that motivates it

`privateer ship move NAME --to MACHINE` works, but it knows only two things
about what a ship needs: its first mate's harness and plugin, and that the
other machine can fetch its projects (`begin` in `src/moves.rs`). An iOS app
ship moved to the Linux laptop arrives, starts its dispatcher, and fails at
its first build because there is no Xcode. A ship whose crew drive Chrome, or
whose work needs `kubectl` and a cluster context, or a harness login that
only one machine holds, has the same problem, and nothing records it.

The other half is pressure. The Mac fills its disk and its owner is told
(`alert_owner` in `src/machines.rs`), but the only remedies on offer are
cleanups and waiting. Sometimes the right answer is that one ship should live
elsewhere. The owner sees the pressure; the captain decides where ships live.

Two changes, one PR each:

1. **Declared needs.** A ship declares what it requires, machines offer what
   they have, and a move to a machine missing a need is refused by name
   before anything stops.
2. **Owner-proposed moves.** Under persistent disk pressure, a machine's
   owner may propose moving one idle ship to the other end of one of its
   links, if that machine meets the ship's needs. The proposal is one captain
   decision; nothing moves without the captain's own yes.

Running single crew jobs elsewhere instead is compared at the end.

## How moves run today

Everything below has to fit this, so it is stated first. A link is one-way:
the machine that added it (role `collects` in the `machines` table) reaches
the other one's gateway, and the other one (role `collected`) never connects
back. Only the linking machine runs a move (`plan` and `run` in
`src/moves.rs`), and only between itself and one machine it reaches: `--to
MACHINE` sends a local ship there, `--here` brings one from there. `known_as`
refuses a machine this one does not reach, and `run` has no case for two
machines other than itself. `ship.move` is operator only (`dispatch` in
`src/moves.rs`).

So a move is always one link, run from its linking side. For a link from L to
A, the two moves that exist are "L to A" (`ship move S --to A`, run on L) and
"A to L" (`ship move S --here`, run on L). Two machines that are not linked
directly never exchange a ship, even when both are linked to a third.

`release` keeps the ship's old folder, projects included, and writes a
`moved.json` marker in its state directory (`MARKER`, read by
`refuse_moved` and `left_behind`). Nothing deletes that folder today, so a
move by itself frees no disk on the old machine.

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
probe. `init` seeds no terms (decision 1). The vocabulary is per fleet: the
linking machine's vocabulary travels to the linked machine in the directory
exchange (`exchange` in `src/machines.rs`, the same frame that already
carries stows), and the linked side stores the copy it was sent;
`capability.put` there is refused with "define it on the linking machine".

"Capability" is the stored noun because `runtime.needs` (`src/needs.rs`)
already means captain attention; the ship-facing words stay `requires` and
`offers`.

### Where requires live

On the ship's record, beside `consumes`. `ship.requires {name, requires?,
pinned?}` *(new)* follows `set_consumes` in `src/fleet.rs`: a ship may set
its own, otherwise its creator, a grantee holding the new grant action
`ship.requires` (added to `ACTIONS` in `src/grants.rs`), or the operator. It
refuses a term the vocabulary does not hold, so a typo fails at once, caps
the list at 16, writes the ship's own record and every other home's
directory entry on this machine, and reaches linked machines on the next
exchange, as `consumes` does. `ship.show` and `ship.list` show it.

The record, not the charter, holds it: a charter decides authority, and a
need grants nothing. The record also travels in every move's snapshot.

A ship may also declare its needs as it is created: `ship.create` takes
`requires`, records them on the new ship, and refuses to create it on a
machine that does not offer every one of them, naming each unmet need. The
machine is the `machine` argument, checked against what it last said it
offers as a placement target is, and checked live again where the ship is
created, exactly as a move's check does; a machine that has not said what it
offers is not one to create on. A machine running an older release ignores
`requires` and creates the ship without recording or checking its needs; its
reply echoes none of them, and the asking side says so in its reply and its
audit (`needs_unchecked`) instead of reporting a checked success.

A ship that declares nothing stays movable exactly as today. The first
mate's harness and plugin stay implicit needs, checked as now.

### Pins and who may clear them

`pinned` is an optional reason the ship should not move ("simulator data
lives on this Mac", "the signing keychain"). It is stored as `{reason, by,
set_ms}`, where `by` is the ship that set it or `captain` for the operator
(the same rule `set_consumes` uses to name its caller).

- A ship, its creator or a grantee may set a pin and clear a pin they set.
- A pin whose `by` is `captain` (set by the operator, or by the captain's
  "Never this ship" below) is cleared only by the operator. `ship.requires`
  from anyone else that would clear or change it is refused with "the
  captain pinned this ship; only the captain unpins it".
- An operator's `ship move` of a pinned ship shows the reason and requires
  `--unpin` *(new flag)*, which clears the pin as part of the move (decision
  6).
- An owner never proposes a pinned ship, whoever set the pin.

### Where offers come from

A machine offers a term when its probe succeeds or the operator declared it:

- **Detected.** A probe is one of: a binary on the login shell's PATH
  (`--probe-path`, checked with `on_path_in` in `src/cli.rs` and
  `login_path` in `src/service.rs`, as `begin` already checks the harness
  binary), an existing path (`--probe-file`), or an argv (`--probe-argv`).
  An argv probe runs with a cleared environment holding only `HOME` and the
  login `PATH`, no stdin, output discarded, in its own process group
  (`process_group(0)`, as `src/plugins.rs` and `src/door.rs` already start
  children); on its five-second timeout the whole group is killed, not just
  the child. Success means exit 0. A probe never reads a credential's value;
  "is there a login" is the harness's own status command answering yes or
  no.
- **Declared.** `machine.offers {add?, remove?, withhold?}` *(new, operator
  only, run on the machine itself)* records offers a probe cannot see (a
  phone on USB, a hardware key) and withholds ones the captain does not want
  used here even though they are installed. Offers = detected ∪ declared −
  withheld.

A machine runs only the probes in its own stored vocabulary. Nothing that
arrives over a gateway names a probe: `move.check` carries need names only,
and a name the machine's vocabulary does not hold is unmet.

Detection is cached for at most ten minutes in a `machine_offers` table
*(new)* and reported in each directory exchange, in both directions, so
`machine list` shows what each linked machine offers and proposals can be
planned without a gateway call. The cache is only for planning: the
preflight always probes live.

### Ship move preflight

Today the target's checks run in `move.begin`, after `move.export` has
stopped the ship's dispatcher (`export` calls `service::stop` first), so even
a refused move briefly stops the ship. The preflight moves first:

1. For a new move, `run` calls `move.check {ship, requires, first_mate,
   projects}` *(new, served by `serve`)* on the target side before
   `move.export`. The gateway already hands every `move.*` command to
   `serve`, so this needs no routing change; for `--here` the target is this
   machine and `Side::Here` serves it directly. The target probes each
   required term live, in parallel, within a total budget of 30 seconds
   (well inside `SLOW_CALL`, 120 seconds), and answers `{met: [...], unmet:
   [{need, why}], free_disk_bytes}`. `why` is "not in this machine's
   vocabulary", "probe failed: xcodebuild is not on PATH", "probe timed
   out" or "withheld here".
2. Any unmet need refuses the move with every unmet need named: "dev-app
   stays on mac: laptop does not offer xcode (xcodebuild is not on PATH),
   iphone-device (not declared there)". Nothing has stopped, so nothing
   restarts.
3. `move.begin` repeats the check, because time passes between the two
   calls; a refusal there is undone as today.

A resumed move (an unfinished `run` row picked up again) skips step 1: its
ship may already be exported or active on the target, and `run` already
refuses to undo one that might be. `move.begin` still checks if the resumed
move has not passed it.

`privateer ship move NAME --to MACHINE --check` (or `--here --check`) *(new
flag)* runs only the preflight and the existing idle test without stopping
anything, and prints met and unmet needs, the folder's size and the project
count. Placement proposals use the same call.

## Owner-proposed moves

### Persistent pressure

Pressure is the owner's disk alerts, held for a while. The keys that count,
all raised by `host_problems`:

| key | raised when |
|---|---|
| `disk_floor` | free disk is below `min_free_disk_bytes` |
| `disk_trend` | free disk will reach the floor within `disk_warn_horizon_secs` |

`capacity_wait` does not count: it is raised only when the capacity queue is
stuck behind an unhealthy holder or behind nothing, which is a runtime fault
that moving an idle ship does not fix. `route:NAME` does not count: a
provider backing off is not the machine's fault, and a move does not change
its route. The `load` alert of finding 15 exists but does not count yet;
whether it counts, and after how long, is decided when placement reads it.

The clock lives with the machine, not the owner. Today a raised alert is a
row in the owner's own `machine_alerts` table, which lives in the owner's
home and would carry stale `raised_ms` values with it if the owner moved. So
`alert_owner` also writes each counting key to a `machine_pressure` table
*(new)* in this machine's fleet registry: `key`, `raised_ms`, and
`restarted_ms` *(below)*. The row is inserted when the key is raised and
deleted when it clears, in the same pass as the owner's notice.

The machine is under **persistent pressure** when a counting key has been
raised continuously for `pressure_after_secs` *(new `resource.configure`
key, default 4 hours)*, measured from the later of `raised_ms` and
`restarted_ms`. A flapping alert resets the clock each time it clears, which
is deliberate (decision 2). The owner's notice for that key gains one
sentence once the threshold passes: "This has lasted four hours;
`ship.placement` lists ships another machine could host."

### Targets

A target is the machine at the other end of one of this machine's links,
whichever side made it, because that is the only pair `run` can serve:

- **This machine linked the other (role `collects`).** The move is "here to
  there", run here as `ship move S --to THERE`.
- **The other machine linked this one (role `collected`).** The move is
  "here to there", run there as `ship move S --here`. A linked machine
  linked only by L has exactly one possible target: L.

Two machines linked only through a third are never each other's target. A
target must also meet, from its cached offers, every requirement of the
ship; report no counting pressure key itself (the exchange carries them);
and keep free disk above its own floor after the move, by the folder's size
plus the projects' size on this machine.

### Candidates

`ship.placement {}` *(new, owner or operator, read only)* answers, for this
machine: the counting pressure keys and how long each has been raised, any
hold on proposing (an open proposal, a backoff, a cooldown, an uncleaned
folder, all below), and for each local ship whether it is eligible and, if
so, which targets meet its needs. A ship is **eligible** only if all hold:

- **Idle.** Nothing `busy` in `src/moves.rs` reports (no wake, job, errand,
  held workspace or live session); no task of its own `running`; no project
  with uncommitted or unpushed work (`projects`); no first-mate wake in the
  last hour; no ready scheduled work from `task.enqueue`; and no
  `task.defer` wait due within the next 24 hours (its `not_before_ms`).
- **Not pinned**, by anyone.
- **Not moved recently.** `last_moved_ms` *(new, stamped on the ship's own
  record by `move.activate`, so it travels with the ship)* is more than 24
  hours ago.
- **Not cooling down.** No refused-at-run-time cooldown on it (below).
- **Not the owner itself**, and not the captain's home (the liaison).

The owner's first mate chooses; the runtime only lists, with each ship's
disk use (folder plus projects) and its share of the host ledger's last day
of jobs.

### The proposal

`ship.propose_move {name, to, why}` *(new, the machine owner only)* checks
the holds, eligibility and target again, gets a live `move.check` answer
from the target (below), and writes one captain `decision`
(`payload.decision` with options, checked by `validate_decision` in
`src/mail.rs`) from the owner, on its duty task.

Where the check runs depends on the link. If this machine linked the
target, `ship.propose_move` calls `move.check` on it through the gateway.
If the target linked this machine, this machine cannot reach it: the
proposal is recorded as `checking`, the target's next exchange with this
machine (each collect pass, at most `CONTACT_MS` apart) runs `move.check`
there and returns the answer, and only then is the decision written. A
`checking` proposal with no answer after 10 minutes closes as "target
unreachable" with no backoff.

The runtime composes the body so every proposal has the same shape:

> **Move dev-app from mac to laptop?** mac has been below its disk floor for
> 6 hours (1.2 GiB free, 2 GiB floor). dev-app is idle, last worked 3 hours
> ago. laptop offers everything it requires (chrome, login-claude).
> **Cost:** dev-app is down for the move (about 4 minutes for 180 MiB and
> 2 projects); its projects are cloned again on laptop (about 3.4 GiB
> there); build caches start cold; its first mate's session starts fresh on
> laptop. **Disk on mac is freed only when the old folder is deleted**
> (3.6 GiB: the folder and its projects; all projects are pushed, so only
> ignored files and caches are lost). *Why (owner):* {why}
> Options: **Move it and delete the old folder** · **Move it, keep the old
> folder** · **Not now** · **Never this ship**

The option ids are `move_clean`, `move_keep`, `not_now` and `never`.
`payload.placement` *(new)* carries the typed facts: proposal id, ship, from,
to, the pressure key and its duration, the folder and project sizes, and the
`move.check` answer, plus `binding`, a SHA-256 over the proposal id, ship,
from, to and the check answer.

Proposal state lives in a `placement_proposals` table *(new)* in the fleet
registry of the ship's machine, so it holds across dispatcher restarts and
survives the owner's own move. The exchange carries open and recently closed
proposals both ways, so the linking side of the link can run an approved one
and the proposing side learns how it ended.

### The captain's yes

Nothing moves without it, and only the captain's own answer counts.

**What counts as the yes.** When the operator answers a placement decision
with `move_clean` or `move_keep` through `captain.reply` (the desk's own
front door; `captain.reply` refuses any caller but the operator), the desk
records a row in a `placement_approvals` table *(new)* in its own fleet
registry: proposal id, ship, from, to, the `binding` it was shown, the
cleanup choice, `approved_ms` and `expires_ms` (6 hours later). The hook runs
after the reply commits, as `block_ask::answered` does for block asks, and
only in the process where the operator wrote the reply. This follows the
existing rule that `payload.operator` on a block ask's answer is set only by
`mail.send` for the operator's own reply, and that `block_ask::check_answer`
refuses an Allow without it.

A reply that arrives as mail, collected or delivered over a link, never
writes an approval, whatever its payload says: a linked ship, a relay or a
bridge that writes "from captain" cannot approve a move.

**How it reaches the mover.** The approval travels in the directory
exchange, as stows already do (`with_stows` and `absorb` in
`src/machines.rs`), not in mail. Every exchange carries the sender's
unexpired, unused approvals; the receiver records them only if the sender
holds this machine's captain desk (`holds_desk`). When the desk is on the
linking machine, the approval is already where the move runs. When `machine
captain` moved the desk to a machine the linking machine reaches, the
linking machine reads it from that machine's exchange answer.

**Who runs the move.** The linking side of the proposal's link runs it, from
the dispatcher pass that already calls `reach_remote`, through
`moves::run_approved` *(new, crate-internal, not a command)*. It takes one
approval and:

1. finds the matching proposal (local, or from the exchange), recomputes its
   `binding` and refuses on any mismatch with the approval's;
2. refuses if the approval expired or was used, writing `used_ms` *(new
   column)* in the same transaction that starts the move, so a replayed or
   re-exchanged approval never moves the ship twice;
3. rechecks eligibility (on the source, through the gateway if it is the far
   side) and runs `move.check` again;
4. calls `run` for exactly that ship and direction, the same function
   `ship.move` calls after its operator check.

`run_approved` is not reachable from any command, takes no ship or machine
from its caller, and runs only a move named by a desk-recorded approval. The
owner's authority stays what it was: `ship.propose_move` writes a proposal,
nothing else. The approval authorizes that one move and is never reused; it
is not a grant. This mirrors how a production approval is bound to one
artifact and environment.

If step 1, 2 or 3 refuses (the ship became busy, gained a need the target
lacks, the target came under pressure, the approval lapsed), the move does
not run: the owner and the captain get one notice naming why. The result
(moved, or refused and why) is a reply on the decision's thread.

### Disk relief after a move

A move frees nothing on the old machine until the old folder is deleted, so
a yes does not end the pressure by itself, and the next proposal must wait
for real relief.

- `move_clean`: after `release`, `run_approved` calls `move.clean {id}`
  *(new, served by `serve`)* on the source. It deletes the folder `release`
  kept only if its marker names this move id, and answers the bytes freed.
  The result says what was deleted.
- `move_keep`: the folder stays. The machine proposes nothing while a
  placement move's left-behind folder remains, and `ship.placement` says so.
  The operator deletes it later with `privateer ship clean-moved NAME` *(new,
  operator only, the same `move.clean` step)*. No follow-up question is
  asked: the captain already chose.

When the folder is deleted, `move.clean` sets `restarted_ms` on every row in
the source's `machine_pressure` to now. If the disk is still under pressure,
another proposal needs another full `pressure_after_secs`, measured from the
cleanup, not from the original alert.

### Bounds

- At most one open proposal per machine, `checking` included. A second is
  refused while one is open, whoever answers first.
- A proposal expires unanswered after 48 hours, which counts as "Not now".
- After "Not now" the machine proposes nothing for 24 hours, then doubles
  per consecutive "Not now" (48, 96 hours, at most 7 days). A move that
  runs resets the backoff.
- A move refused at run time (step 1–3 above) closes without doubling the
  backoff but with a cooldown: that ship is not proposed again for 24 hours,
  and the machine proposes nothing for 6 hours.
- After a move, nothing more until the left-behind folder is cleaned and the
  pressure clock has run again from the cleanup.
- "Never this ship" pins the ship with `by: captain` and the reason "the
  captain declined moving it"; only the operator clears it.

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
| Cost | minutes of downtime, a fresh clone, cold caches, a fresh first-mate session, the old folder until cleaned | per job: fetch, build cold or warm a remote cache, logs over the gateway |
| Exists | `ship.move`, plus the preflight above | none; the gateway lets agents only deliver mail |

For a job elsewhere the runtime would need: a remote job command on the
gateway that takes argv, a commit and a project URL, admitted by the far
machine's own ledger and capacity queue; a workspace on the far machine owned
by a hosting ship there (its owner by default) so cleanup has an owner; crew
routes and harness logins there, enforced there, if the job is a crew rather
than a build; outcome and a bounded log tail returned as a crew outcome
notice, with commits pushed rather than copied; and `crew.park` extended so a
crew here parks on a job there and resumes after a link outage, with
duplicate outcomes refused by id. Like moves, it could only run across one
link, started from the side that reaches the other.

A remote heavy job still runs the repository's code with the far machine's
user's privileges. It would be admitted only under a captain grant naming
that ship and that machine, never by default.

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
- **A probe hangs or forks.** Each argv probe has five seconds and its whole
  process group is killed at the limit; probes run in parallel inside a
  30-second budget, so a check never approaches `SLOW_CALL`.
- **A gateway call tries to run a probe.** It cannot: `move.check` carries
  names, and the target runs only its own stored probes.
- **Offers change between check and move.** `move.begin` checks again; the
  cache is never trusted for a move.
- **Vocabulary drift across machines.** The linked side holds the linking
  side's copy; a term missing there reads as unmet with that reason.
- **A forged yes.** A reply that arrives as mail never writes an approval,
  and an exchanged approval is accepted only from the machine holding the
  desk, bound to a proposal whose binding still matches.
- **A replayed yes.** `used_ms` is written in the transaction that starts
  the move; a second sight of the same approval finds it used.
- **The approval arrives while the linking machine sleeps.** It waits for
  the exchange, within its 6-hour validity; after that the captain and the
  owner are told it lapsed, which counts as a refusal at run time.
- **The desk moves between proposal and answer.** The approval is recorded
  where the operator answered and accepted only from the machine
  `holds_desk` names at the time it is read.
- **The owner moves.** The pressure clock and proposal state are in the
  machine's registry, not the owner's home, so neither travels with it.
- **Interrupted move.** Unchanged: `run` resumes it without a new
  `move.check`, and `last_moved_ms` is stamped only on activation, so a
  failed move does not start the 24-hour clock.
- **Disk not freed.** Until the old folder is cleaned the machine proposes
  nothing, so it cannot propose ship after ship against the same pressure.
- **Ping-pong.** A target must report no pressure, and the 24-hour rule
  holds, so a ship is not bounced between two strained machines.

## Tests the implementation needs

Declared needs (PR 1), with two registries and homes in separate directories
over a real gateway subprocess, like the existing move tests:

- An undeclared ship moves exactly as before, in both directions.
- A ship requiring a term the target lacks is refused before its dispatcher
  stops, naming every unmet need; its dispatcher was never stopped. Both
  `--to` and `--here`.
- A declared offer meets a need; a withheld one fails it though its probe
  passes; a probe that exits nonzero or outlives its timeout is unmet, and a
  probe that forks a sleeper leaves no process behind.
- A probe sees no environment variable but `HOME` and `PATH`.
- `move.check` with argv in its arguments runs nothing but stored probes.
- `ship.requires` refuses unknown terms, more than 16, and callers without
  authority; it reaches other homes and linked machines on exchange.
- A captain's pin cannot be cleared by the ship, its creator or a grantee;
  the operator can; `ship move` of a pinned ship needs `--unpin`.
- The vocabulary defined on the linking machine arrives on the linked one;
  `capability.put` on the linked side is refused.
- `move.begin` re-checks: removing an offer between `move.check` and
  `move.begin` undoes the move and restarts the dispatcher.
- A resumed move does not call `move.check`.
- `--check` changes nothing.

Owner proposals (PR 2):

- Pressure starts only after `pressure_after_secs` continuous; a cleared and
  re-raised alert restarts the clock; `capacity_wait` and `route:` keys
  never count; the clock is unchanged by the owner moving away and back.
- Each eligibility rule excludes a ship: running task, busy wake or job,
  dirty project, ready scheduled work, a deferral due within a day, pinned,
  moved within a day, cooling down, the owner, the liaison.
- Targets are exactly the other ends of this machine's links; a machine
  reached only through a third is never listed; a target under pressure, or
  without room, is not listed.
- A proposal from a linked machine to its linking machine gets its check
  from the next exchange and only then writes the decision.
- A second proposal while one is open is refused; backoff doubles after
  each "Not now" and an expired proposal counts as one; a move resets it; a
  refusal at run time sets the 24-hour ship and 6-hour machine cooldowns.
- The operator's `move_clean` answer moves the ship once from the linking
  side, deletes the old folder and restarts the pressure clock, with the
  owner on the linking machine, on the linked machine, and with the desk
  moved to the linked machine.
- `move_keep` moves it and holds further proposals until `ship clean-moved`
  runs.
- No approval is written by: a reply delivered over a link, a reply whose
  payload claims the operator, a ship's `mail.send` from `captain`. An
  approval exchanged from a machine that does not hold the desk is ignored.
- A changed proposal (different target, different check answer) does not
  match its approval and does not move. A replayed approval does not move
  the ship twice. An approval older than 6 hours is not used.
- A ship that became busy after the yes is not moved, and the captain is
  told why.
- Proposal state survives a dispatcher restart and the owner's own move.

## Order of work

1. **Declared needs**: the vocabulary, `ship.requires` with pin authority,
   `machine.offers`, offers in the exchange, `move.check` before export,
   `ship move --check` and `--unpin`.
2. **Owner proposals**: `machine_pressure`, `ship.placement`,
   `ship.propose_move`, desk-recorded approvals in the exchange,
   `run_approved`, `move.clean` and `ship clean-moved`, `last_moved_ms`.
3. **Later, if needed**: remote heavy jobs under a captain grant, then remote
   crew.

## Decisions for the captain

Each has a recommendation; the design above follows it unless the captain
says otherwise.

1. **Seed the vocabulary?** Should `init` seed a few common terms (`xcode`,
   `docker`, `chrome`) the captain can remove? *Recommended: no.* The fleet's
   vocabulary should be the captain's words, and an empty list costs one
   command per term.
2. **Pressure threshold.** *Recommended: 4 hours continuous, disk keys only
   (`disk_floor`, `disk_trend`), with no daily total for flapping alerts.*
   An alert that clears on its own is not a reason to move a ship; `load` is
   decided once it exists.
3. **Who runs the move after the yes?** *Recommended: the runtime, through
   the bound one-shot `run_approved` on the link's linking side, with
   deleting the old folder offered as part of the yes (`move_clean` /
   `move_keep`).* The point is relief while the captain is away, and relief
   needs the folder gone.
4. **A standing grant for owners to move ships without asking?**
   *Recommended: no.* Placement is the captain's call, and proposals are
   cheap to answer.
5. **Should requirements also steer `ship.create {machine}`?**
   *Recommended: yes, as a small follow-up once `ship.requires` exists:
   refuse to create a ship where it cannot work, naming the unmet needs.*
6. **Who clears pins: should `ship move --unpin` exist, or should only the
   captain's explicit unpin clear a pin?** *Recommended: keep `--unpin` on
   the operator's `ship move`, since only the operator can run it, and let
   only the operator clear a pin the captain set; a ship clears only a pin
   it or its managers set.* It keeps one deliberate step for the captain to
   override a pin without a separate command, and no agent can undo the
   captain's "Never this ship".
