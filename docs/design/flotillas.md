# Proposal: flotillas

Status: proposal for later; nothing here is implemented. It expands the short
flotilla section of the [conversation and activation design](conversation-and-activation.md#flotillas-later-version)
and builds on the [cross-machine fleet proposal](cross-machine-fleet.md).

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

## Three needs, three mechanisms

### Shared resources: the runtime, with no commander

Exclusive use of a simulator, of Xcode's build system or of the signing
keychain is a fact the runtime can arbitrate in the same way it already
arbitrates memory and CPU (see [admission](../architecture.md#runtime-boundary)),
first come first served, at no model cost and without any ship commanding
another.

Proposed: named resources in the host's admission ledger. A job declares
`"resources": ["simulator:iphone-17", "xcode-build"]` with `job.start`. A
resource has a capacity (one by default; `resource.configure` names them and
sets capacities) and is held from admission until the job ends, together with
the memory and CPU reservation. A job that cannot get every resource it names
waits in the same first-come queue, so a job that needs Xcode and a simulator
never holds one while starving for the other. `resource.status` shows who holds
each resource and who waits. A stow holds new jobs as it does today.

This is useful before any flotilla exists, and it is the first thing to build
for the iOS fleet.

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
its members:

- **Assign work.** Create tasks owned by a member, which the member accepts
  without the usual handoff step, within the member's own charter. The
  commander never widens what a member may do.
- **See the flotilla.** Read members' bearings, including idle tasks and what
  waits on the captain, and their jobs and resource queue positions.
- **Order the machine.** Set queue priority among members for named resources
  and capacity, so this afternoon's app goes first. Within a priority the
  queue stays first come first served.
- **Pause and resume members.** A stow scoped to the flotilla, so the
  commander can clear the Mac for one build or for the captain.
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

The usual caveat applies: on one machine under one OS account the agent role
stops a confused agent, not a determined one.

## Across machines

Nothing above assumes one machine, but resources do: a simulator lease is a
lease on the Mac's ledger. With [machine links](cross-machine-fleet.md), a
commander on the laptop could lead members on the Mac. Its resource priorities
apply to the Mac's ledger through the Mac's gateway, and its view of members
comes over the same link. Members on two machines each use their own
machine's resources; a flotilla never pools resources across machines.

## What the captain sees

The dashboard and Herdr sidebar group a flotilla's ships under their
commander, within each machine. `/waiting` and `captain.waiting` show members'
requests as the commander's when the commander holds "speak for the
flotilla", so the captain's list does not multiply by six. `resource.status`
shows the Mac's simulators and Xcode as held or free, and by whom.

## Order of work

1. **Named exclusive resources** in the admission ledger. This is needed for
   the iOS fleet whether or not a flotilla is ever built.
2. **Consumers** on ship records, for shared contracts.
3. **Machine links** ([cross-machine proposal](cross-machine-fleet.md), steps
   1 and 2), so the laptop's liaison reaches the Mac's ships.
4. **Channels and standing grants**, the authority mechanism flotillas use.
5. **Flotillas**: the record, the commander's grant and the actions above,
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
