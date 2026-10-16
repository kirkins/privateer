# Proposal: one fleet across machines

Status: proposal for review; nothing here is implemented. Today's fleet,
peer and mail rules are in [the architecture](../architecture.md#the-fleet).

## What a new captain runs into

A captain with a laptop and a second machine wants one fleet: the liaison on
the laptop should create a ship on the other machine, mail it, and hear back
from it. Each step fails today, and none of the failures is a bug:

1. **The fleet stops at the machine.** The registry is per user and per
   machine, and `fleet.link` links only homes it can open. Reaching a ship on
   another machine means an operator `peer.put` and `ship.put` in every home on
   both sides, for every pair of ships, redone whenever a ship is created or
   retired.
2. **Mail needs SSH both ways.** Delivery is push: the sender's home calls the
   recipient's `mail.deliver`. A machine behind NAT or reached through a tunnel
   can receive mail but cannot answer.
3. **Ships are created on their creator's machine only.** `ship.create` checks
   the caller's charter in the home it runs in. The liaison has no charter in
   any home on the other machine, so it is refused there (`unknown charter
   liaison`). The only way past this is clearing `PRIVATEER_ROLE` over SSH,
   which good agents refuse and which should never be the supported path.
4. **The captain is local.** `mail.deliver` refuses captain mail ("the
   captain's mail stays in its own home for now"), and a door reads captain
   mail only from homes on its own machine. A ship on the second machine
   cannot reach the captain's phone.

## Proposal

### Machines join a fleet once

`privateer machine add NAME SSH_TARGET` is an operator command, run on a
machine that can reach the other one, in the same shape as `herdr machine
add`. It checks that the remote Privateer is installed and speaks a
compatible protocol, then records the link in both registries: a `machines`
table holding the name, the SSH target, the remote executable and registry,
and the link's direction. `machine list` and `machine remove` complete it.
The operator authorizes this once with their own SSH access, which is real
operator authority on both machines.

Ship names stay bare mail addresses and must be unique across linked
machines: `machine add` refuses a link whose fleets share a name, and
`ship.create` checks linked machines before taking a name.

### A gateway per machine instead of a peer per pair

Each machine's registry answers three RPC commands for its whole fleet:
`fleet.ships` (active ships and their scopes), `fleet.deliver` (accept mail
for a ship hosted on this machine and hand it to that home locally) and
`fleet.collect` (see below). A home reaches every ship on a linked machine
through one peer, `machine-<name>`, whose target is that machine's gateway
rather than a single home. Directory entries for remote ships are refreshed
from `fleet.ships` when a machine is linked, when a ship is created or
retired, and on a bounded interval by the dispatcher, so a new ship on either
machine is reachable without further setup.

`fleet.deliver` keeps today's acceptance rule: mail is accepted only from a
ship the receiving fleet knows to live behind that machine, and only for a
ship it hosts. Ordering, retries, idempotent ids and quarantine are
unchanged, because delivery into the final home is the existing local path.

### Pull when only one side can connect

A link added from the laptop to a machine behind a tunnel is marked
one-way. Mail on that machine for laptop ships is queued with
`delivery: "awaiting_collection"` instead of failing and retrying. The
laptop collects it with `fleet.collect {machine, through}`, delivers each
message locally by its id, and acknowledges through the last sequence it
stored, so a crash between the two re-collects and redelivery stays
idempotent. Collection runs under one fleet-wide lock (`machine-<name>.lock`
beside the registry), so any one dispatcher on the laptop does it, within the
same per-pass network budget as delivery (`delivery_budget_ms`).

### The captain is reachable from any machine

Captain mail stays in the home that wrote it; the door reads it across a
link. Through `fleet.collect`, the door on the laptop also carries captain
mail from homes on linked machines, with its existing per-home delivery
cursor. The captain's replies go back as `fleet.deliver` calls made by the
operator-side door process, and `mail.deliver` accepts captain mail only
when it arrives through a machine link. Anyone who can act as the operator
on the laptop can already SSH to the other machine with the same key; this
names that fact instead of adding authority.

### Creating ships across machines

`machine add` takes explicit grants: `--grant liaison=inspect,spawn,message`
records on the remote machine that the linked machine's `liaison` may create
ships there, with at most those actions. No grant means no remote creation;
grants are never inherited from charters on the other machine.

`ship.create {"name": ..., "machine": "mac", ...}` (`privateer ship new NAME
--on mac`) is sent to the remote gateway's `fleet.create` with the caller's
agent identity, which peer calls already pass as flags that can only narrow.
The remote machine checks the requested actions against the grant, creates
the ship in its own default ship folder with the remote binary, starts its
dispatcher and records `created_by: "liaison@laptop"`. `ship.actions`,
`ship.scope` and `ship.retire` from the same creator travel the same way and
are checked against that record and the grant. The new ship's first mate
uses the creator's harness and model only if that harness is installed on
the remote machine; otherwise creation names the missing harness and stops.

### Limits this keeps

This is not a security boundary, the same as the agent role on one machine:
an agent that clears its environment, or a person with the laptop's SSH key,
can act as the operator on both machines. A grant limits a cooperating
agent; it does not contain a hostile one. There is no distributed
transaction: creating a remote ship and refreshing directories are separate
steps, and a retry finishes an interrupted one.

## Order of work

Each step is one PR, qualified with two registries and homes in separate
directories, connected through a real subprocess transport.

1. Machine links and the gateway: `machine add|list|remove`, `fleet.ships`,
   `fleet.deliver`, directory refresh, and name uniqueness across links, with
   SSH in both directions.
2. One-way links: `awaiting_collection` delivery and `fleet.collect`.
3. The captain across machines: door collection and captain replies.
4. Remote creation: grants, `fleet.create`, and remote `ship.actions`,
   `ship.scope` and `ship.retire`.
5. Briefs and views: `ship.list` and the dashboard show which machine each
   ship is on, and the liaison's instructions cover creating ships elsewhere.

## Open questions

- Should a link be one-way by default and become two-way only after the
  remote machine proves it can connect back? Recommended: yes. Probing the
  return path at `machine add` time is cheap, and assuming it caused the
  current dead end.
- Should a remote creator be written `liaison@laptop`, or should machines be
  invisible in names entirely? Recommended: qualify only `created_by`, keep
  mail addresses bare, and enforce fleet-wide name uniqueness.
- Should grants live per machine link (above) or per ship on the remote
  machine? Recommended: per link, since that is the unit the operator
  authorizes.
