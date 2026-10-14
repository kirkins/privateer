# Design: conversation and activation

Status: **agreed design, not yet implemented.** Written 14 October 2026 from a design discussion with the project owner. It describes where Privateer is going; [architecture](../architecture.md) describes what exists today. Work in related areas, including the in-progress Herdr integration, should align with this document rather than treat earlier work as final.

## Why this document exists

Privateer can already record charters, task ownership, targeted messages, grouped events and supervised jobs. What it cannot do is hold a conversation with a person or wake an agent when something needs its attention: agents run only when someone calls `agent.start`, and a message to a ship waits until something happens to look.

Two questions decide much of the project's future shape:

1. How does a person talk to a fleet of independent ships, especially from a phone, through one contact point, without that contact point becoming a chain of command?
2. How do ships wake up and act on messages, events and each other's requests, without a person present and without paying for a model while nothing is happening?

## Principles

- **Flat between ships, hierarchy inside a ship.** Ships are peers; no ship outranks another. Inside a ship, its first mate manages its crew.
- **A contact point is a role, not a rank.** "Talk to me through one ship" is the captain's delivery preference, not fleet architecture. Nothing in the fleet depends on the contact point existing.
- **The runtime records facts; agents supply judgement.** Outcomes that leave durable evidence (a job finishing, a request answered, a grant matched) are recorded by the runtime when they happen, never by an agent remembering to report them.
- **No model calls while idle.** Waking, grouping, routing addressed messages and answering "what is happening" from records cost no inference.
- **Autonomy within explicit authority.** Ships act without a person present inside the authority the captain granted, and that authority is explicit, auditable and revocable.
- **Durable and restart-safe.** Every message, request, wake and grant survives a crash or restart; a restart is a non-event.

## Vocabulary

| Term | Meaning |
|---|---|
| **Captain** | The person. A first-class participant with an address, a mailbox, a front door for plugins and a delivery policy. |
| **Ship** | An identity with its own mailbox, first mate and crew, working under charters on its tasks. A home hosts one or more ships. Ships are peers. |
| **First mate** | A ship's coordinating agent. Woken by the ship's mailbox and its crew's outcomes; speaks for the ship; manages its crew. |
| **Crew** | Workers a first mate starts as supervised jobs for its tasks. |
| **Liaison** | An ordinary ship with a liaison charter that the captain chooses as their default recipient. |
| **Mailbox** | A durable, typed message store for an address (a ship or the captain). |
| **Channel** | An authorized request route from one ship to another, for named request kinds. |
| **Standing grant** | A captain-created policy that authorizes matching requests on a channel without a person in the loop. |
| **Flotilla** | (Later version.) A group of ships that one ship commands. |

First Mate's language carries over naturally. A First Mate secondmate is "a first mate under a parent"; Privateer has no parent ship, so every ship's coordinator is simply its first mate and "second mate" is not needed.

## The captain

The captain is the one genuinely special participant. Making the person first-class, rather than making the contact point special, keeps the fleet flat.

- **Address and mailbox.** Ships can address `captain` like any ship, subject to the captain's policy.
- **Front door.** A stable interface that plugins connect to: mobile apps, voice, chat services, the terminal dashboard. Plugins send the captain's messages in and stream messages, notices and decisions out. A plugin is an adapter; it owns no fleet state.
- **Delivery policy**, set by the captain:
  - the default recipient of their messages (for example the liaison);
  - whether other ships may reach them directly, or only through the default recipient;
  - urgency levels, quiet hours and batching.
- **Three kinds of input**, following First Mate's inbox split:
  - *status*: "what is happening" is answered from records across ships, with no model call and no wake;
  - *ask*: a side question is answered without becoming fleet work;
  - *message*: everything else is delivered to the addressed ship, or the default recipient, and wakes it.

Switching between "everything through the liaison" on a phone and "talk to any ship directly" at a desk is a policy change, not a different system.

## Ships and the directory

A ship is an identity, not a process or a home: one Privateer home can host several ships (as the terminal dashboard presents them), or each ship can have its own home, possibly on another machine. The directory records where each ship lives.

The directory lists every ship the fleet knows, readable by all ships, not only the liaison:

- name and a plain-language scope ("infrastructure, deploys, the build servers");
- address: a local home, or a remote home reached through `peer.call`;
- the request kinds it accepts, with their payload schemas;
- how its first mate is activated (see [Activation](#activation)).

A ship's address is transport configuration, not authority. A peer's address remains what it is today: not a credential and not a rank.

## First mates and crew

Each ship has one first mate. It is woken by the ship's mailbox and by its crew's outcomes, decides what to do, starts crew as supervised jobs, and speaks for the ship to other ships and to the captain.

- Crew work on the ship's tasks under its charter. By default they communicate through their first mate; ship-to-ship conversation is first mate to first mate.
- A crew member's outcome (finished, failed, needs a decision) is recorded by the runtime from the job's real state when it happens, and wakes the first mate. It never depends on a worker remembering to report.
- Heavy crew work goes through `job.start` with admission and the capacity queue, so ships share the machine fairly.

## The liaison

The liaison is an ordinary ship whose charter makes it a good contact point. The captain makes it their default recipient through the delivery policy.

- **Default charter:** message, delegate (create tasks for other ships, which take ownership through the normal handoff and acceptance) and read (repositories, logs, status). It does not implement. This is a default, changeable per fleet.
- **Why read-only by default:** it stays responsive (short turns, never mid-build), its context stays about the captain and the fleet, it can run on a cheaper model, and a router that can also implement is tempted to keep work rather than route it.
- **It owns no one.** It relays, routes and summarizes. Work it delegates is owned by the ship that accepts it.
- **It is optional.** A one-ship fleet needs no liaison: that ship is the contact point. If the liaison is down, the captain's messages queue durably and the captain can still address ships directly.

A typical phone conversation:

- "How's the API refactor going?" is answered from records, with no model call.
- "Ask infra why the deploy failed" becomes a correlated request from the liaison to the infrastructure ship; the runtime routes the answer back to the captain.
- "Fix the flaky billing test" leads the liaison to find the right ship in the directory and create a task that the billing ship accepts.
- The captain can still message the billing ship directly at any time.

## Mailboxes

Mailboxes generalize today's targeted messages and grouped events into a standard every ship, and authorized external agents, can use.

**Message kinds:**

| Kind | Meaning |
|---|---|
| `request` | Asks the recipient for something. Carries a correlation id and expects a `reply`. |
| `reply` | Answers a request, referencing its correlation id. |
| `notice` | A fact for the recipient. Expects nothing. |
| `decision` | Something only the captain can decide. Held until answered. |

**Envelope:** id (idempotency key), from, to (`ship:<name>` or `captain`), kind, correlation id, task, request kind and payload, urgency, and an optional reply deadline.

**Semantics:**

- Delivery is durable and idempotent: replaying a message with the same id is a no-op, and different content under the same id is refused.
- An open request is durable state. The runtime routes its reply to whoever asked, including the captain when the request originated with them, and flags a request whose deadline passes. This is First Mate's pending-reply guard, built in from the start.
- Request kinds are typed, with payload schemas (for example `deploy` with project, artifact and environment), so channels and grants can evaluate them mechanically.
- Repeated facts about one task and issue keep grouping into one pending item, as today, and acknowledgements cover only the sequence actually observed.
- Retention follows `history.prune`: a fact that still backs a pending item or an open request is never pruned.

## Activation

Activation wakes the right agent when there is something to do, and does nothing, at no model cost, when there is not.

### Three layers

1. **Ship:** a message in a ship's mailbox wakes its first mate.
2. **Crew:** a crew outcome wakes its first mate.
3. **Captain:** a message to the captain is delivered according to their policy: pushed to a plugin, batched, or held for quiet hours.

### The resident service

A small long-running service per host (`privateer serve`, run as a systemd user service or launchd agent) dispatches wakes. Writes that create a reason to wake (a message, an event, a job outcome) record a wake request in the same transaction, so nothing is missed across crashes or restarts. The service makes no model calls; it reads records, evaluates triggers and starts or resumes agents.

### How an agent is woken

Each harness plugin declares which wake modes it supports:

| Mode | What happens | Best for |
|---|---|---|
| **Resume** | Continue the ship's persistent native session (Codex, Claude and Pi can resume). Prompt caching makes this cheaper, and the first mate keeps its conversation. | The default |
| **Inject** | Deliver into a live interactive session, such as a Herdr or tmux pane. | When the captain is watching |
| **Fresh** | Start a headless run with the brief plus the new mail. Simple, but rebuilds context every time. | The fallback |

Terminal integrations such as Herdr are **activation and observation backends**: they provide the inject mode, live status and attach/reconnect. They are not a separate orchestration system.

### Grouping, guards and acknowledgement

- **Grouping:** a burst of messages and events becomes one wake carrying everything pending.
- **No double wakes:** a ship whose first mate is already running is not woken again; new mail waits for its next inbox read, or is injected.
- **Loop detection:** a flat fleet has no boss to stop two first mates politely replying to each other forever. Correlated conversations carry hop counts and per-conversation budgets, and the runtime refuses or escalates runaways.
- **Budgets and quiet hours:** per-ship wake budgets (for example at most N wakes per hour) and quiet hours.
- **Authority:** a revoked charter or a cancelled task means no wake.
- **Capacity:** wakes go through admission and the capacity queue.
- **Acknowledgement:** a wake hands the agent everything up to a given sequence number, and the agent acknowledges exactly what it saw; anything newer triggers the next wake.

Activation is where a fleet's inference cost lives: every wake is a model turn. Grouping, resuming rather than starting fresh, and deciding what is wake-worthy largely determine what the fleet costs to run.

## Channels and standing grants

Channels let ships, and other authorized agents, ask each other for work and have it happen without a person present.

A **channel** names a requesting ship, a receiving ship and the request kinds allowed between them, for example "the app ship may send `deploy` requests to the sysadmin ship".

A **standing grant** is the captain's policy for a channel. Everything in it is defined by the captain: nothing is mandatory, and nothing is imposed beyond what they choose. Conditions can include:

- projects, repositories and environments;
- evidence about the artifact (merged to a given branch, checks passed on that exact commit, time spent in staging without alerts);
- rate limits and time windows;
- an expiry, if the captain wants one;
- whether the captain is notified of each matching request;
- an optional veto window ("deploying `abc123` to production in 15 minutes; veto?"), where silence means go.

Suggested defaults, all adjustable: production grants notify the captain; no veto window, expiry or rate limit unless set.

**How a request is evaluated:**

1. The runtime checks the request against the channel's grants: who asked, what kind, which project and environment, the artifact evidence, and limits.
2. If it matches, the request is authorized without a person. For a deployment, the runtime creates a one-time approval bound to the exact artifact and environment and records the grant that allowed it, so the audit trail is as precise as a per-deployment approval.
3. If it does not match, it is refused, or escalated to the captain as a `decision`.

**Rules:**

- Creating, changing and revoking channels and grants are operator commands; agents cannot widen their own authority. The captain can ask the liaison to draft a grant, but the captain creates it.
- Grants can be paused or revoked instantly, and every authorization is audited.
- Production grants are explicitly marked as production.
- This updates the production rule in `AGENTS.md`: production deployment requires explicit authorization bound to an exact artifact and environment, either a per-deployment approval or a standing grant the captain created, with each deployment recorded against its artifact.

**Identity caveat:** a grant is only as strong as the requester's identity. On one machine under one OS account, "this request came from the app ship" relies on the agent role marker, which stops confused agents but not deliberate impersonation. Across machines, the receiving home can identify the calling peer more firmly through its SSH key. Hard per-ship identity (separate OS accounts or signed requests) is future work.

## Flotillas (later version)

A flotilla is a group of ships commanded by one ship: the commanding ship can assign them work and supervise them, while flotilla ships still keep their own charters and mailboxes. It is a deliberate, opt-in exception to flatness for cases where one ship should own a domain spanning several homes. It is out of scope for the first version and will be designed once ships, mailboxes and activation are running.

## Build order

1. **Mailbox v1:** typed messages, correlated requests with reply routing and overdue flags, and the ship directory.
2. **Resident service and first-mate activation:** `privateer serve`, wake requests, grouping and guards, and resume/fresh wake modes through harness plugins.
3. **Crew outcomes wake the first mate:** runtime-recorded crew outcomes become wakes.
4. **Channels and standing grants:** channel and grant records, request evaluation, auto-minted approvals, notices and veto windows.
5. **The captain as a participant:** front door, delivery policy, status/ask/message handling and a liaison charter, followed by plugins (mobile, voice, chat) and an inject wake mode for terminal backends such as Herdr.

Each step ships with tests at real process boundaries and an isolated pilot, as earlier milestones did.

## Open questions

- Which harnesses support resumable sessions, and how session identity is stored and recovered.
- Concrete loop-detection limits and how runaways are escalated.
- Where request-kind schemas are defined: core, plugins or ship configuration.
- Stronger cross-ship identity for grants on a single machine.
- How the dashboard and terminal backends present the captain's mailbox.

## Credit

This design takes much of its inspiration from [First Mate](https://github.com/kunchenguid/firstmate), whose operating model the project owner used and valued. Ideas adopted from it include the captain/first mate vocabulary and its nautical language, one contact point for the person, a zero-token watcher that wakes agents only when something is actionable, the principle that scripts report facts while agents report judgement, the inbox's status/ask/note split, and the pending-reply guard. Privateer's difference is structural: ships are peers rather than links in a chain of command, the contact point is a role chosen by the captain, and ships are meant to act more autonomously within authority the captain grants.
