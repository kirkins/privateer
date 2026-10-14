# Privateer's terminal bridge

The first dashboard slice is a Rust TUI that runs inside Herdr or an ordinary
terminal. It supplies a nautical overview of **one existing local Privateer
home**, organized as equal peer ships. Fleet overview is the home destination;
the sidebar always shows ships. Each ship has Bridge, Assignments, Terminals
and Logbook views. It does not aggregate other homes or remote peers.

This is an observation and navigation tool. It is not yet a conversational
navigator, scheduler or interactive agent launcher. Installing it does not
replace an existing fleet. The current headless harness/job interfaces continue
to work independently.

## Build and preview

```sh
cargo build --locked --features dashboard --bin privateer-dashboard
./target/debug/privateer-dashboard --demo
./target/debug/privateer-dashboard --demo --once
```

The dashboard dependencies are optional. Ordinary `cargo build --bin privateer`
does not enable them. Ratatui 0.29/Crossterm 0.28 preserve the project's Rust 1.85
floor; this does not require adopting a newer compiler for the runtime.

Demo mode uses clearly labeled sample data, never opens an inherited Herdr
socket and does not create a runtime home, jobs, events or model calls.

Fleet home now previews a separate captain's inbox. Press `i` to inspect its
sample decisions, replies and grouped notices, or `l` to open the liaison's
Bridge. The liaison is an optional contact role among peer ships. In the demo,
`m` opens a local Status/Ask/Message composer: Tab changes mode, Enter previews
the interaction, and Escape cancels. Preview exchanges stay in memory and are
discarded on exit. They do not call a model or deliver a live message. In live
mode, the captain's inbox and messaging remain unavailable until the runtime's
front door is connected; task alerts are not treated as captain mail.

## Existing state and Herdr

In a Herdr pane, run:

```sh
/path/to/privateer-dashboard --home /absolute/privateer/home
```

`PRIVATEER_HOME` may supply the home. When `HERDR_ENV=1`, the dashboard can use the
injected `HERDR_SOCKET_PATH`. It never guesses a global socket, discovers a
focused session or starts a Herdr server. An explicit `--herdr-socket
/absolute/path` selects a socket; `--no-herdr` disables the integration. Herdr
is optional: without it, tasks, jobs and events remain visible.

The adapter uses the versioned `session.snapshot` and `agent.focus` socket
contracts. Protocol 20 or newer is required for this initial implementation;
older/malformed responses produce a visible connection error. This minimum is
based on the installed 0.8.2 protocol schema inspected during development, not
a claim that all future protocol revisions are qualified.

Select a ship and its **Terminals** view, choose a bound agent and press Enter or `f` to focus
its native Herdr terminal. Return through Herdr's own tab/workspace navigation.
The dashboard does not embed a second terminal emulator. The operation checks
the selected socket incarnation and re-reads the pane's terminal, workspace,
tab, agent and native session identity before focusing. Focus is navigation
only; it does not authorize input, confirm delivery or verify a task outcome.
The check and focus are separate server operations, so they are not an atomic
occupant lock. No keystroke or command delivery relies on that check.

Herdr pane observations are deliberately separate from Privateer task records.
There is no inferred binding based on similar labels or matching owners. A
Herdr `done` state does not mark a task done or prove a passing PR. Stored job
statuses may be stale because this UI deliberately does not reconcile processes.

## Ships, ownership and terminal bindings

Ships are equal peer identities, not ranks. `Navigator` is a familiar point of
contact in the demo and has no special authority. The demo also includes Apps,
Infrastructure and an idle Research ship.

Live ships come from the optional private `<home>/dashboard.json` registry plus
owner/handoff identities observed in the bounded local snapshot. A registry
entry keeps an idle ship visible between sessions; an unregistered identity may
fall out of a later bounded snapshot. This is a dashboard registry, not a new
runtime authority table or remote-host discovery mechanism.

One optional `liaison: true` ship supplies the contact badge and shortcut. This
is presentation configuration, not a grant of authority or an implementation of
the captain's runtime delivery policy.

See [the example registry](../examples/dashboard.json). Its `id` must match the
runtime's exact task-owner identity. Names, descriptions and terminal bindings
are presentation configuration. The dashboard reads the file without creating
or modifying it. It accepts at most 64 KiB, 200 configured ships and 32 terminal
bindings per ship. Duplicate identities and a pane bound to multiple ships are
rejected. Missing configuration falls back to observed owners.

A terminal belongs to a ship only through an exact `pane_id` + `terminal_id`
binding in this registry. The first binding is its primary terminal. Similar
labels, process kinds and task titles never establish ownership. Stale bindings
show no connected terminal until the configuration is updated. The existing
focus action still revalidates the actual server and observed agent/session.
Unbound Herdr panes are not presented as another ship's crew.

Assignments appear only in their owning ship's Assignments view. An offered
handoff also appears under the recipient as **Incoming**, with the current owner
shown explicitly. Ownership changes only after the runtime records acceptance;
this dashboard does not initiate or accept transfers. The same task ID, jobs and
pending facts follow the current owner. Logbook includes current assignments'
events and custody audit rows involving the ship, preserving an outgoing ship's
handoff reference. Audit rows identify actor/action and current owner; the UI
does not invent historical target information the runtime did not record.

Fleet overview summarizes all observed ships and pending issues. Ship-level
alerts, jobs, assignment history and terminal lists are filtered to that ship.
The preview remains one local database plus one explicitly chosen Herdr socket;
listing a ship does not establish remote access or task-to-native-session
continuity. Demo terminals are labeled sample data and never connect to a socket.

## Controls and presentation

| Key / action | Result |
|---|---|
| Click a ship | Open its workspace |
| Click Fleet overview; `0` or Home | Return to the fleet home |
| `1`–`4`, Tab after selecting a ship | Bridge, Assignments, Terminals, Logbook |
| Click an assignment/terminal | Select its details |
| Up/Down, `j`/`k` | Select a ship, or an entry in the focused content list |
| Left / Right | Focus ship navigation / workspace content |
| Enter on ship navigation | Focus workspace content |
| Enter/`f` on a ship's terminal; `f` on Bridge | Open the selected/primary live terminal |
| Wheel / Page Up/Down | Navigate the pointed list / scroll details |
| `c` | Hide/show additional stored jobs on Bridge |
| Escape | Return home from a ship; exit when already home |
| `q`, Ctrl-C | Exit; workers continue |

Mouse support works when the terminal forwards mouse events, including inside
Herdr. Clicking changes selection only; live terminal focus still requires an
explicit Enter/`f` action. Hit targets follow rendered layout and list offset;
resizing clears stale targets. Mouse capture is released on exit.

The compact summary starts on. Grouped pending issues remain visible and routine
facts stay available in Logbook; viewing never deletes facts or acknowledges an
alert. Failed sources remain visibly unavailable while the last snapshot is
retained with its age. The details toggle is session-local.

The home screen has no boat or animation/status band: those three rows are
available to assignments and details. `--ascii` uses a plain ASCII attention
marker. Terminals narrower than 50 columns or 14 rows get a resize notice rather
than a broken layout.

The tested Rust boat module is retained for a future Calm experience, where the
boat can identify that mode and replace an existing working indicator. The
current dashboard detail toggle is not First Mate's transcript-level Calm
function. No animation timer or motion controls run in the dashboard.

## Reliability boundaries

- SQLite opens an existing database with `READ_ONLY`; it does not call
  `Store::open`, run migrations, chmod the home, touch the shared resource ledger,
  run job reconciliation or alter task/event/approval state. SQLite may use its
  normal WAL/shared-memory sidecars. A missing home is an error, not an implicit
  initialization.
- Each state section is bounded to 200 rows with a truncation notice. Queries
  bound long fields; SQLite uses a 100 ms busy timeout and a 500 ms progress
  deadline. Text displayed in the terminal has control and bidi-override
  characters removed. The view reads selected summary/message fields rather than
  full payloads, argv, provider configuration or environment variables. Summaries
  and messages can contain private text; treat screenshots accordingly.
- State and Herdr refresh in separate workers, each with a one-item result
  channel. A stalled source cannot block keyboard handling. Herdr snapshots
  refresh every two seconds, with failure backoff up to 16 seconds. This first
  version polls bounded snapshots; it does not yet consume push subscriptions.
- Socket JSON responses have correlation checks, a 1 MiB cap and an 800 ms
  response deadline. At most 1,000 panes are accepted. Repeated failures replace
  the source error in the UI instead of appending transcript notifications.
- Nothing in the dashboard owns, cancels, restarts or recovers agent processes.
  Closing the dashboard never sends worker shutdown signals. Interactive
  launch/admission/recovery must be added through an explicit backend lifecycle
  contract before it can replace headless job ownership.
- Only the explicitly selected/inherited Herdr socket is inspected. Provider
  routes and machine topology remain private configuration. Read-only viewing
  does not narrow the OS account's other credentials.

## Reuse and next boundaries

First Mate's Calm behavior and nautical working indicator inform the visual
direction. The reserved boat is a fresh Rust implementation, without importing the Pi
renderer or its transcript patches. See [attribution](../THIRD_PARTY_NOTICES.md).
Its Herdr regression scenarios also informed stale-identity checks and focus
behavior; its shell backend and policy framework are not imported.

Future work includes persistent navigator conversation, explicit task-to-native
session bindings, direct worker messaging with delivery evidence, push events,
and interactive launch/recovery/resource admission. These need meaningful
qualification; the dashboard does not claim them through its labels or demo.

The broader comparison remains an informational reference in
[issue #12](https://github.com/kirkins/privateer/issues/12), not a parity checklist.

## Validation

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo check --locked --no-default-features --bin privateer
python3 scripts/dashboard-smoke.py --binary target/debug/privateer-dashboard
```

Dashboard tests use isolated databases, temporary Unix sockets and Ratatui's
in-memory terminal backend. They cover non-mutating observation, bounded views,
terminal replacement, socket replacement, malformed replies, timeouts, demo
isolation, ship scoping, custody transfers, idle ship registration, mouse input,
resize and the reserved boat module. They do not connect to a live fleet or
prove model-driven execution. Use a real isolated Herdr session to qualify
interactive navigation on the installed server version before fleet adoption.
