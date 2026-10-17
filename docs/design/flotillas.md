# Proposal: flotillas

Status: steps 1 to 3, 5 and 6 of the [order of work](#order-of-work) are
implemented (the disk floor, launchd agents and machine links, machine
owners, consumers, and narrower creator rights with standing grants for
ship management; see the [architecture](../architecture.md#the-fleet));
named leases wait until two app ships compete, and flotillas are a
proposal. It expands the short
flotilla section of the [conversation and activation design](conversation-and-activation.md#flotillas-later-version)
and builds on the [cross-machine fleet proposal](cross-machine-fleet.md). It
was revised after running the first app ship on the Mac (see
[What the first ship showed](#what-the-first-ship-showed)).

## The case that motivates it

The captain plans an iOS fleet on one Mac: four to six app ships and a server
ship the apps share. Each app deserves its own ship, with its own repository,
scope, notes and first mate. But they compete for things only one of them can
use at a time, and some of their work has to move together:

- **One machine.** One Xcode, one set of simulators, one signing keychain, a
  16 GiB Mac with little free disk. Two app ships booting simulators and
  building at once slow each other down or fail.
- **Shared contracts.** When the server changes an API, several apps must
  follow, in an order that keeps every app working.
- **Priorities and timing.** Which app gets the machine first this afternoon,
  which apps ship together, and when everything should pause for the captain's
  own use of the Mac.

The flat fleet handles the first poorly today, the second through ordinary
requests, and the third not at all. A flotilla commander is one answer, but
the three needs differ, and only one of them needs a commander.

## What the first ship showed

One app ship has run on the Mac with a hosted first mate and crew building
and testing in simulators. Bringing it back after a provider route was cut
off, and adding a second ship for the machine's upkeep, taught five things.

- **Disk ran out first, not Xcode.** The ships never contended for Xcode or a
  simulator. The disk reached 99% full, filled by simulator devices, caches
  and stale build trees that outlive the jobs that made them. Exclusive leases
  would not have helped. Disk admission exists, but only as a per-charter
  floor that refuses a job outright, and `init` sets that floor to zero.
- **The machine needed an owner.** Finding what to clean, proposing it to the
  captain and keeping the Mac's tools current is ongoing work that belongs to
  no app. A second ship scoped to the machine's upkeep took it on with no new
  runtime features, and the app ship's note now sends low-disk problems there.
- **Flat peers coordinated well.** One sentence in each ship's note routed the
  work between them. Scopes and notes did what a hand-kept list of consumers
  would have.
- **Operations cost the most, and the Mac was out of sight.** The app ship sat
  on a dead route for a day because nothing on the Mac reaches the captain's
  other machine. Recovering it took an operator over SSH: install a release,
  change the route, add a provider credential, restart a dispatcher that does
  not survive a reboot. With six app ships that work grows with the fleet,
  and a commander does not reduce it.
- **Authority already leaks through creation.** An operator running `ship new`
  inside a ship's folder records that ship as the creator, and a creator may
  retire or rescope what it created. The upkeep ship was created this way, so
  the app ship can retire it. Command over ships exists today by accident.

A sixth point applies to every option: all first mates on a machine may share
one inference plan's limits. Six app ships, a commander and an upkeep ship are
eight first mates drawing on it, and a commander's value is judgment, which a
cheap route is weakest at.

## Four needs, four mechanisms

### Shared resources: the runtime, with no commander

The runtime arbitrates what ships share, in the same way it already
arbitrates memory and CPU (see [admission](../architecture.md#runtime-boundary)),
first come first served, at no model cost and without any ship commanding
another.

**Disk first.** A host-wide free-disk floor in the shared admission settings
(`min_free_disk_bytes`, default 2 GiB), set with
`resource.configure {"min_free_disk_bytes": ...}` and shown by
`resource.status`. Jobs declare what they will write as well (`estimate.disk_bytes`,
defaulting to `heavy_estimate.disk_bytes`, 4 GiB, for heavy jobs), and admission
counts that demand and the demand of every live reservation above the floors, so
a machine that is running out of room refuses the next build by name rather than
starting one that fails. The machine's owner is warned while free space is still
above the floor but falling toward it within a day ([choosing the floor for a
real workload](../resources.md)). Jobs that would start below it can wait in the existing
host-wide capacity queue; `workspace.acquire` can opt into that queue with
`wait_for_capacity:true`. Long job waits appear in `runtime.needs` as a typed
disk floor blocker. The per-charter `min_free_bytes` floor remains in force, and
`init` leaves it at the charter default instead of writing zero.

**Named exclusive resources later, when contention appears.** A job declares
`"resources": ["simulator:iphone-17", "xcode-build"]` with `job.start`. A
resource has a capacity (one by default; `resource.configure` names them and
sets capacities) and is held from admission until the job ends, together with
the memory and CPU reservation. A job that cannot get every resource it names
waits in the same first-come queue, so a job that needs Xcode and a simulator
never holds one while starving for the other. `resource.status` shows who holds
each resource and who waits. A stow holds new jobs as it does today.

Until two app ships actually compete, the host's `max_heavy_jobs` cap and a
ship note sending every build and simulator run through a heavy job serve
instead. Leases may also matter less than expected: each ship can use its own
cloned simulator and its own DerivedData, leaving memory admission to limit
how many boot at once, so leases may end up covering only what truly cannot
be shared, such as the signing keychain. A booted simulator also outlives the
job that booted it, so a lease must shut down what it started or the next
holder inherits it.

### The machine: an owner per machine

Each machine has one ship that owns its upkeep: disk scouting and approved
cleanups, local tooling, resource settings, and clearing the machine for the
captain. It is an ordinary ship with an ordinary charter, and it works by
proposals: it asks the captain before deleting anything or installing
anything, and reports free disk before and after.

The runtime's part is small. A machine records which ship owns it, and
host-level problems on that machine (an unmet disk floor, stuck capacity, a
route backing off) go to that ship as well as the captain. Holding new jobs so
the captain can use the machine is a stow scoped to the machine, which the
owner may start and end. The owner sets no priorities among other ships'
work.

### Shared contracts: flat requests, with a little help

The server ship owns its API. An app ship that needs a change asks for it with
a request; the server ship tells its consumers when a change lands. What is
missing is a way for the server ship to know its consumers without being told
each time: a `consumes` list on each app ship's record (`["server"]`), so
`ship.list` answers "who uses me" and a notice to consumers needs no hand-kept
list. Ordering a breaking change across several apps is judgment, which is
where a commander helps.

### Priorities and timing: the flotilla commander

A flotilla is a named group of ships plus one commanding ship, created by the
captain. The commander is an ordinary ship with an extra, explicit grant over
its members. It leads a product family, not a machine: the machine's resources
belong to the runtime and its upkeep to the machine's owner.

- **Assign work.** Create tasks owned by a member, which the member accepts
  without the usual handoff step, within the member's own charter. The
  commander never widens what a member may do.
- **See the flotilla.** Read members' bearings, including idle tasks and what
  waits on the captain, and their jobs and resource queue positions.
- **Order members' work.** Set queue priority among members for named
  resources and capacity, so this afternoon's app goes first. Within a
  priority the queue stays first come first served, and the priority never
  touches ships outside the flotilla.
- **Pause and resume members.** A stow scoped to the flotilla, so the
  commander can hold every app while a breaking server change lands.
- **Speak for the flotilla.** Members' requests to the captain go to the
  commander first, which answers what it can within its grant and passes the
  rest on; the liaison talks to the commander rather than to six app ships.

Members keep their charters, mailboxes, notes and first mates. A member may
still mail any ship directly; the flotilla adds authority, not a wall.

## Authority

Command is a standing grant the captain creates, not something a ship can take.
It is recorded with the flotilla (`flotilla.create {name, commander, members,
grants}`, an operator command), lists exactly which of the actions above the
commander holds, and can be paused or revoked at once; every command action is
audited against it. A commander cannot add members, create flotillas or widen
its own grant. This is the same shape the design gives channels and standing
grants, and flotillas should be built on that mechanism, not beside it.

Today's creator rights are the implicit version of this and should be
narrowed before any explicit hierarchy is built on top. An operator's
`ship new` should record the captain as creator unless the operator names
another ship, and the right to retire or rescope a ship should come from a
grant, so that command over a ship is always something the captain chose.

The usual caveat applies: on one machine under one OS account the agent role
stops a confused agent, not a determined one.

## Across machines

Nothing above assumes one machine, but resources do: a disk floor or a
simulator lease belongs to one machine's ledger, and each machine has its own
owner. With [machine links](cross-machine-fleet.md), a commander on the laptop
could lead members on the Mac. Its priorities apply to the Mac's ledger
through the Mac's gateway, and its view of members comes over the same link.
Members on two machines each use their own machine's resources; a flotilla
never pools resources across machines.

Machine links matter before any of this. Without them the captain's other
machine cannot see a ship on the Mac that has stalled, and the Mac's ships
cannot reach the liaison or the captain's door.

## What the captain sees

The dashboard and Herdr sidebar group a flotilla's ships under their
commander, within each machine, with the machine's owner first. `/waiting`
and `captain.waiting` show members' requests as the commander's when the
commander holds "speak for the flotilla", so the captain's list does not
multiply by six. `resource.status` shows the Mac's free disk against its
floor, and its simulators and Xcode as held or free, and by whom.

## Order of work

1. **A host-wide disk floor** that queues jobs and workspaces, surfaced when
   it stays unmet, and `init` no longer writing a zero floor.
2. **The Mac as a first-class machine**: a launchd service so its dispatchers
   survive a reboot, then machine links ([cross-machine proposal](cross-machine-fleet.md),
   steps 1 to 3), so the laptop sees and mails the Mac's ships and their
   problems reach the captain.
3. **Machine owners**: a machine records its owning ship, which receives that
   machine's host-level problems and may stow the machine.
4. **Named exclusive resources**, once two app ships actually compete for a
   simulator, Xcode or the keychain.
5. **Consumers** on ship records, for shared contracts.
6. **Narrower creator rights**, then **channels and standing grants**, the
   authority mechanism flotillas use.
7. **Flotillas**: the record, the commander's grant and the actions above,
   then the dashboard grouping.

## Open questions

- Should a ship belong to at most one flotilla? Recommended: yes, to keep
  priorities and routing unambiguous.
- Should the commander be a member's peer or a dedicated ship with no
  repository of its own? Recommended: a dedicated ship, so its first mate's
  context is the flotilla, not one app.
- Do resource priorities preempt? Recommended: no. Priority orders the queue;
  running jobs are never stopped for it.
- How much of "speak for the flotilla" should a commander answer itself before
  passing a request to the captain? Recommended: only what its grant covers,
  with the original request quoted.
- Should a machine's owner be a runtime record or only a convention of scopes
  and notes? Recommended: a record, small enough to route host-level problems
  and allow a machine stow, and nothing more.
- Should an inference plan's limits be admitted like the machine's resources,
  so first mates and crew sharing one plan queue rather than all hitting its
  cap? Recommended: consider it with route backoff, which already handles a
  plan that refuses; a commander should not ration it.
