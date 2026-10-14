# Design: terminal dashboard and Herdr

Status: **design direction, not implemented functionality.** Updated 14 October
2026 against the [conversation and activation design](conversation-and-activation.md)
at `f15581c`. This refines its terminal presentation and backend boundaries;
that document governs mailboxes, authority and activation. The
[dashboard guide](../dashboard.md) describes the current implementation.

## Keep the ship-centered navigation

Fleet overview remains home, with a persistent Ships sidebar. Selecting a ship
opens its Bridge, Assignments, Terminals and Logbook views. Ships are peers;
each ship's first mate manages that ship's crew. A liaison badge identifies the
captain's chosen contact point, not a command rank. A one-ship fleet needs no
separate liaison.

A ship means one Privateer home, not a task-owner string or a terminal pane.
The implemented dashboard groups owner identities from one local database and
an optional presentation registry. It is a useful navigation prototype, not
yet a directory of independent homes. Future ship entries come from the runtime
directory, with a stable identity and a configured local or remote address.
Several homes may share a host and Herdr server; one ship may have several crew
terminals. None of those relationships can be inferred from names.

Assignments remain scoped to their recorded owner. Incoming handoffs show the
current owner until acceptance, and custody history remains available after
transfer. Directory identity must be mapped explicitly to the relevant home
and task-owner identity; listing a ship grants no authority over its tasks.

## Fleet home and the captain's inbox

Fleet home presents a concise fleet summary and the captain's inbox: decisions
awaiting an answer, correlated replies, overdue requests and grouped notices.
Ship workspaces retain their own assignments and operational detail. A fleet
summary may link to an assignment without making it a global assignment list.

The captain is a participant, not another ship. Keep an Inbox destination beside
Fleet overview, accessible from every workspace. Fleet home previews that same
inbox; it does not create another mailbox or notification stream. Prioritize
actionable items and substantive replies over routine activity, with full
history available on demand. Notification priority, quiet hours and batching
come from the captain's delivery policy, not a separate dashboard policy.

The dashboard connects through the captain's front door when that interface
exists. It must not create a second message store or read other homes' database
files over SSH. Fleet summaries use bounded runtime queries through configured
peer transport. Each source carries its age and availability: one unreachable
ship must not block navigation or make the rest of the fleet appear current.
If an aggregate is incomplete, say which ships are unavailable.

## Conversations and explicit actions

Provide a quick **Talk to liaison** action when a default contact is configured,
and **Message ship** in every ship workspace. Show the recipient before sending;
direct contact follows the captain's delivery policy. An unavailable liaison
does not remove direct addressing where policy permits it.

Use the front door's three input modes with clear labels:

| Mode | Effect |
|---|---|
| Status | Read recorded state; no agent wake or model call |
| Ask | Answer a side question without creating fleet work; may use inference |
| Message | Deliver to the selected recipient and request activation through the runtime |

Opening Fleet home, selecting a ship, reading records, or focusing an existing
terminal must not wake an agent or start a new conversation. Opening a ship's
Bridge presents its recorded summary and conversation entry point. Do not embed
a second terminal emulator: **Open terminal** focuses or attaches to the native
session when supported. If the session is absent, show that fact; starting or
resuming it is an explicit runtime action, not a side effect of navigation.

Messages enter the durable mailbox before any attempt to inject them into a
terminal. Display accepted/queued, delivered, observed and answered only when
the corresponding evidence exists. A socket write or successful focus is not
proof the agent read a message. Reconnect and retry reuse the same message ID;
uncertain delivery must not retype the text as a new message. A decision answer
is recorded against its decision ID and current state, so a stale dashboard
cannot apply an answer to a replaced request.

Until the front door and mailbox interfaces exist, keep these actions visibly
unavailable rather than substituting raw terminal input. Reading a notice or
opening a terminal does not implicitly acknowledge all pending work. Explicit
acknowledgement covers only the sequence actually observed.

## Herdr is an activation and observation backend

Herdr supplies native interactive terminals, observations, focus and, once
qualified, attach/reconnect and live injection. The resident service owns wake
dispatch; the runtime owns mailbox state, grouping, authority checks, capacity
admission and task outcomes. Harness plugins declare resume, inject and fresh
capabilities. Herdr availability alone does not establish that a harness can
accept semantic message delivery or resume its previous conversation.

The terminal adapter reports capabilities and bounded observations to that
machinery. It does not independently decide to start an agent whenever a pane
looks idle. Resume is preferred where supported; inject targets a verified live
session; fresh is a fallback only after reconciling any previous execution.
An unsupported or uncertain inject operation leaves mail pending. It must not
silently launch a second first mate.

Keep one recorded execution owner. A headless execution belongs to its job
supervisor; an interactive execution belongs to its terminal backend. Both run
through the runtime's admission and lifecycle contract. Closing the dashboard
must leave the resident service, mailboxes and work intact. Losing Herdr can
interrupt executions it owns; treat that separately from closing the UI and
reconcile actual payloads before replacement. Unaffected headless work continues.

Terminal bindings need ship/home identity, endpoint identity and incarnation,
pane and terminal IDs, native session identity, harness, execution incarnation
and role (first mate or crew). Link crew executions to their runtime jobs and
tasks. A first mate's persistent session may span several tasks; do not force
one conversation per assignment. Store private endpoints and provider routes
in deployment configuration, outside generic presentation defaults.

Focus remains a navigation operation. Inject or lifecycle control needs a
stronger contract than today's separate snapshot and focus calls: a verified
recipient at delivery, correlated acknowledgement and a safe response to a
replaced pane or session. If the installed backend cannot supply those
guarantees, retain observation/focus and use a supported mailbox consumption or
resume path. Labels and keystroke timing are not substitutes for delivery proof.

## Responsive operation and honest status

- Herdr events signal bounded refreshes; they are neither durable mailbox facts
  nor instructions to call a model. Reconcile after reconnect and capability
  check the installed server. Keep buffers bounded and back off failed sources.
- Group repeated failures by endpoint or affected execution. Preserve the
  underlying facts without appending an alert for every poll or output line.
- Keep mailbox state, activation state, execution state and terminal connection
  separate. A connected terminal does not prove a healthy agent; an idle pane
  does not prove completion. Crew outcomes come from runtime job evidence.
- The runtime prevents duplicate wakes, applies budgets and groups new mail.
  Dashboard refreshes and subscriptions must not create their own wake loop.
- Reconnection restores observations. It does not automatically resend text,
  cancel work, acknowledge newer mail or assume an old process has ended.

## Persistent home and nautical presentation

Keep the dashboard available as a dedicated Herdr tab, with a discoverable way
to return from agent terminals. Qualify the installed backend's navigation
capabilities before choosing a shortcut; do not commandeer existing bindings,
tabs or workspaces. Herdr remains optional, and the same dashboard can run in
an ordinary terminal with terminal-specific actions unavailable.

Retain the compact nautical vocabulary and icons with plain text labels.
Reserve the animated boat for an explicit future Calm experience, where it
identifies reduced routine chatter rather than occupying normal dashboard
space. Calm changes presentation only: decisions, failures, substantive replies
and cancellation outcomes remain accessible, with no change to authority,
mail delivery or scheduling. Provide reduced-motion and ASCII alternatives;
animation runs locally and never generates durable events or model calls.

## Delivery sequence and qualification

Follow the conversation and activation build order, without making runtime
implementation wait for a finished terminal UI:

1. Keep the existing observation/focus dashboard usable. As the ship directory
   becomes available, replace inferred owner groups with explicit home/owner
   mappings and bounded per-ship projections. Retain the demo as sample data.
2. Once activation exists, expose recorded session and wake state. Qualify
   resume and first-mate/crew execution bindings before offering lifecycle
   actions. Model calls never depend on the dashboard staying open.
3. Once the captain's front door exists, add Inbox, liaison selection display,
   Status/Ask/Message and correlated replies through that interface. Grants and
   decisions use runtime validation, not dashboard-maintained approval rules.
4. Add inject and attach/reconnect per supported backend/harness combination,
   then qualify the persistent home navigation. Unsupported capabilities remain
   clearly unavailable; a tmux backend can use the same boundary later.

Qualify each implemented boundary in isolation, including:

- Opening and closing the dashboard or switching ships causes no model call,
  duplicate agent or job termination.
- A disconnected remote ship or liaison does not block other views; stale or
  incomplete summaries are labeled.
- Directory entries cannot redirect input through a reused label, pane,
  endpoint or native session identity.
- A crash between durable acceptance and inject acknowledgement preserves one
  message and reconciles delivery without blind replay.
- Mail arriving during an active turn remains pending beyond the acknowledged
  sequence; a restart neither loses it nor creates concurrent first mates.
- A decision answered elsewhere cannot be answered again from stale UI state.
- A first mate resumes its conversation across tasks; crew outcomes wake the
  correct ship through runtime records, independently of terminal observation.
- Losing the terminal server distinguishes ended, surviving and uncertain
  payloads before any replacement; headless work remains independent.
- Provider-specific routing remains enforced by private harness configuration
  for every activation mode, without dashboard fallback around it.

These are acceptance scenarios for future implementation, not claims that the
current dashboard or backend already passes them. Exact front-door APIs,
cross-home projections, session recovery and backend delivery acknowledgements
remain implementation design work shared with the runtime milestones.
