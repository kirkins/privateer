# Privateer's terminal bridge

The dashboard is a Rust TUI that runs inside Herdr or an ordinary terminal.
Its home lists active ships from the per-user fleet registry (`fleet.list`),
reading each existing home read-only. Missing homes stay visible as unavailable
and are never recreated. It shows scope, harness, attended presence, dispatcher
lock status, open requests, unread ship mail and the three latest wakes. Fleet overview is the home destination;
the sidebar always shows ships. Each ship has Bridge, Assignments, Terminals
and Logbook views. Remote peers are not discovered.

The dashboard supports observation, navigation, captain mail, attended boarding
and ship management. A model-driven conversational navigator and scheduler remain
future work. Installing it does not replace an existing fleet. The current
headless harness/job interfaces continue to work independently.

## Build and preview

```sh
cargo build --locked --features dashboard --bins
./target/debug/privateer-dashboard --demo
./target/debug/privateer-dashboard --demo --once
```

Install both binaries with `cargo install --locked --path . --features dashboard`.
Live mailbox commands use `privateer` beside the dashboard binary, falling back
to `privateer` on `PATH`.

The dashboard dependencies are optional. Ordinary `cargo build --bin privateer`
does not enable them. Ratatui 0.29/Crossterm 0.28 preserve the project's Rust 1.85
floor; this does not require adopting a newer compiler for the runtime.

Demo mode uses clearly labeled sample data, never opens an inherited Herdr
socket and does not create a runtime home, jobs, events or model calls.

Fleet home includes the live captain inbox aggregated across fleet homes through
`mail.inbox` and `mail.requests`, with decisions first, then requests, replies
and notices. `i` opens it; Enter marks the selected message read and `r` replies
to a request or decision in its originating home. Acknowledgment advances only
through messages explicitly marked read, so a decision read out of sequence
cannot clear earlier unseen notices. Read markers waiting on an earlier message
are session-local and survive refreshes beyond the current inbox page until
acknowledged. Refreshing a newly reachable page acknowledges only its contiguous
prefix of explicitly marked messages. Acknowledged open requests remain visible
until answered.
Each refresh reads at most 200 unread messages and 200 open requests per home,
with at most 1,000 displayed items overall. Admission prefers unread items,
then decisions, requests, replies and notices within each read state. Mark items
read to reach later mail; list previews are bounded to 100 characters, while
details retain the supported body and JSON payload. Scroll with Page Up/Down or
the wheel to read long mail;
payload-only messages include a JSON preview and readable payload details.
Refresh preserves scroll only while the same message in the same home remains
selected; selecting another message starts its details at the top.
No unseen row is acknowledged. Failed homes show an error and retain their last
mail observation. The UI uses bounded worker queues; commands have 15-second
deadlines and bounded output sized for a full mail page. Failed sends retain
their draft and id for an idempotent retry; a deadline means the action's outcome
needs checking.

Select any ship and press `m`: Status summarizes recorded state without a model
call; Ask sends a request and Message a notice from `captain`. Replies use the
original message id as correlation. All mail changes go through Privateer's
commands, never direct SQL writes. Mailbox commands use the runtime's normal
Store open/migration behavior; fleet/task observations remain read-only.

Demo mode keeps sample mail and its Status/Ask/Message composer entirely in
memory. No live mailbox commands, acknowledgments or sends run in demo mode.

## Existing state and Herdr

In a Herdr pane, run:

```sh
/path/to/privateer-dashboard --home /absolute/privateer/home
```

No home argument is needed for fleet discovery. `PRIVATEER_FLEET` selects a
private registry; otherwise the runtime's usual per-user path is used. A missing
registry means an empty fleet and creates no state. `--home` / `PRIVATEER_HOME`
provide a legacy single-home fallback when the fleet is empty. When `HERDR_ENV=1`, the dashboard can use the
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
occupant lock. Input to existing agents remains disabled; the separate
[Board action](#boarding-and-management) sends launch text only to a new shell pane.

Herdr pane observations are deliberately separate from Privateer task records.
There is no inferred binding based on similar labels or matching owners. A
Herdr `done` state does not mark a task done or prove a passing PR. Stored job
statuses may be stale because this UI deliberately does not reconcile processes.

## First run and everyday entry

With an empty fleet, the interactive dashboard welcomes the captain and suggests
`liaison` at `~/ships/liaison`. Name and folder are editable; the suggested folder
tracks the name until changed explicitly. Choose among supported installed
harnesses (←/→ cycles Harness), and choose `liaison` or an ordinary `ship` role.
Tab moves fields, Delete/Ctrl-U clears a field, Enter creates, and Escape skips
without creating a home or fleet registry. `n` reopens setup in an empty fleet.
A missing harness keeps setup open with guidance, without initializing anything.

Creation uses `privateer init --ship NAME --harness ID [--liaison]` in the chosen
folder, then `privateer service start` and Board. Existing nonempty folders are
refused unless they hold that same initialized ship (safe command retry).
Without Herdr, creation succeeds and the Board limitation is shown; use
`privateer start` in that folder to talk to its first mate.

`privateer init --liaison` creates a default charter with `inspect`, `message`
and `spawn` (read, communicate, delegate), without implementation authority. Its
generated `AGENTS.md` asks about work and preferences, proposes peer ships and
charters, helps create them and add projects, and explains direct dashboard
access. New agent-created ships remain bounded by the creator's charter; the
captain grants implementation rights explicitly when needed. The preset and
checklist survive a later ordinary init; captain-edited charters and instruction
files are preserved. It is a suggested agent interface, not evidence of an
actual autonomous onboarding conversation.

Inside Herdr (`HERDR_ENV=1`), bare `privateer` opens an executable
`privateer-dashboard` beside the runtime or on PATH. An empty-fleet entry opens
the dashboard before offering to initialize the current directory. An existing
ship still gets its dispatcher ensured before the dashboard opens. Outside
Herdr, or without an installed dashboard, existing behavior is preserved.
`privateer start` always launches the harness directly. Install both binaries
with `cargo install --locked --path . --features dashboard`.

## Boarding and management

Select a ship and press `b` (or click Board). A live exact binding focuses its
Herdr terminal. Otherwise Board creates a tab in the ship folder and launches
`privateer start` using the installed protocol 20 `tab.create` and
`pane.send_input` contracts. The installed 0.8.2 CLI was also checked against
a private protocol recorder: `pane run` emits `pane.send_input` with text and
`keys: ["Enter"]`. Shell tokens are quoted; captain text is never
shell input. Only the pane returned by creation is eligible for this launch,
after a fresh identity/protocol check. A pending launch blocks retries for up to
15 seconds, and confirmed live presence clears that pending guard. A completed
or failed session can be boarded again even when its old tab remains. Every new
launch checks fresh presence; a live lease blocks a competing attended session.
The runtime's [transactional lease claim](architecture.md#attended-mode) also
protects against simultaneous launches from separate dashboards.
An unbound attended session must be reached directly or its binding refreshed;
the dashboard does not open a second first mate.

`privateer start` and attended hooks record an optional `binding` in the presence
lease, captured from the explicit inherited socket path and caller pane. It pins
the socket's device/inode incarnation, pane, terminal, workspace and tab ids,
plus a native session identity when available. Only a refresh by the same holder
while its lease is still live preserves the binding and start time. Reclaiming
an expired lease captures a new binding and start time even
when the holder string is reused; a replacement holder also captures its own
context. Missing/invalid Herdr context leaves attendance usable without a binding.
The dashboard ignores bindings after lease expiry or release. No label matching
or guessed socket is used. Dashboard observations also support homes made before
the binding column was introduced, without migrating them. Board needs an
explicit/inherited Herdr socket; a
regular terminal still shows state and supports mail and management.

`n` opens New ship (name, scope, optional sibling folder, harness), using an
existing available ship as its creator. For an empty fleet, create the first
ship through the [quick start](../README.md#quick-start); dashboard onboarding
is a later step. `x` opens Retire: type the exact selected ship name to confirm;
its folder is kept.
Retirement refuses if the fleet name now belongs to a different home. `d` opens
Dispatcher: status, start or stop. The forms call `ship.create`, `ship.retire`
and `privateer service` through argv, in a worker; errors retain the form and
commands never write SQLite directly. Retirement uses the runtime's existing
behavior: it revokes/unlinks and stops the dispatcher, without taking ownership
of an attended agent process.

The [attended injection proposal](design/herdr-inject.md) explains the lease,
presentation cursor and headless-wake rules. Injection remains disabled because
the installed prompt contract has no atomic expected-occupant guard.

## Ships, ownership and terminal bindings

Ships are equal peer identities, not ranks. `Navigator` is a familiar point of
contact in the demo and has no special authority. The demo also includes Apps,
Infrastructure and an idle Research ship.

Live fleet ships come from active registry entries, with scope and status read
from their own homes. The optional private `<home>/dashboard.json` supplies
presentation names and exact terminal bindings. Fleet and single-home views also
include observed owner/handoff identities. A registry
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
Pane ownership and liaison uniqueness are also checked across assembled fleet
homes, including manual claims against automatic presence bindings. Claims from
every observed configuration remain conflict evidence, including entries for
another ship and locally invalid presentation. Invalid presentation produces a
visible source error and disables Board, terminal focus and liaison shortcuts
until corrected; recorded work, originating homes and custody history remain
visible.

A terminal belongs to a ship through its live verified presence binding, or an
exact `pane_id` + `terminal_id` binding in this presentation registry. The live presence binding is primary, otherwise the first configured binding is primary. Similar
labels, process kinds and task titles never establish ownership. A manual claim
cannot bypass the known automatic socket, terminal, workspace, tab or native
session constraints for the same pane or terminal. Stale bindings show no
connected terminal until their identities match the observed context. The existing
focus action still revalidates the actual server and observed agent/session.
Unbound Herdr panes are not presented as another ship's crew.

Assignments appear only in their owning ship's Assignments view. An offered
handoff also appears under the recipient as **Incoming**, with the current owner
shown explicitly. Ownership changes only after the runtime records acceptance;
this dashboard does not initiate or accept transfers. Every task, notice, job,
activity and custody row retains its originating home, including work delegated
to an owner registered in another home or absent from the fleet registry. Task
selection and job joins use the home plus local task ID, so identical IDs from
different homes remain distinct across refreshes and handoffs. Jobs and pending
facts follow the current owner. Logbook includes current assignments'
events and custody audit rows involving the ship, preserving an outgoing ship's
handoff reference. Audit rows identify actor/action and current owner; the UI
does not invent historical target information the runtime did not record.

Fleet overview summarizes all observed ships and pending issues. Ship-level
alerts, jobs, assignment history and terminal lists are filtered to that ship.
The preview reads bounded local fleet homes plus one explicitly chosen Herdr socket;
listing a ship does not establish remote access or task-to-native-session
continuity. Demo terminals are labeled sample data and never connect to a socket.

## Controls and presentation

| Key / action | Result |
|---|---|
| Click a ship | Open its workspace |
| `b` / Board | Focus its bound first mate or launch `privateer start` in a new Herdr tab |
| `n`, `x`, `d` | New ship, confirmed retirement, dispatcher status/start/stop |
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
Herdr. Clicking a terminal changes selection only; live terminal focus requires
Enter/`f` or the explicit Board action. Hit targets follow rendered layout and
list offset; resizing clears stale targets. Mouse capture is released on exit.

The compact summary starts on. Grouped pending issues remain visible and routine
facts stay available in Logbook; viewing never deletes facts or acknowledges an
alert. State and Herdr worker errors retain the last snapshot with its age;
an unavailable fleet home remains listed with an error. The details toggle is
session-local.

The home screen has no boat or animation/status band: those three rows are
available to assignments and details. `--ascii` uses a plain ASCII attention
marker. Terminals narrower than 50 columns or 14 rows get a resize notice rather
than a broken layout.

The tested Rust boat module is retained for a future Calm experience, where the
boat can identify that mode and replace an existing working indicator. The
current dashboard detail toggle is not First Mate's transcript-level Calm
function. No animation timer or motion controls run in the dashboard.

## Reliability boundaries

- Fleet/task observation opens an existing database with `READ_ONLY`; it does
  not call `Store::open`, run migrations, chmod the home, touch the shared
  resource ledger, run job reconciliation or alter task/event/approval state.
  SQLite may use its normal WAL/shared-memory sidecars. A missing home is an error, not an implicit
  initialization. Mailbox commands use the Store behavior described above.
- Fleet/task sections are bounded to 200 rows with a truncation notice. Queries
  bound long fields; SQLite uses a 100 ms busy timeout and a 500 ms progress
  deadline. Text displayed in the terminal has control and bidi-override
  characters removed. Task views read selected summary fields; mail details keep
  the full supported body and JSON payload within the display's row bound.
  Views omit argv, provider configuration and environment variables. Summaries
  and messages can contain private text; treat screenshots accordingly.
- State and Herdr refresh in separate workers, each with a one-item result
  channel. A stalled source cannot block keyboard handling. Herdr snapshots
  refresh every two seconds, with failure backoff up to 16 seconds. This first
  version polls bounded snapshots; it does not yet consume push subscriptions.
- Socket JSON responses have correlation checks, a 1 MiB cap and an 800 ms
  response deadline. At most 1,000 panes are accepted. Repeated failures replace
  the source error in the UI instead of appending transcript notifications.
- The dashboard opens an attended session through `privateer start`; that
  runtime holds its presence and waits for its harness. It does not own, cancel,
  restart or recover agents.
  Closing the dashboard never sends worker shutdown signals. Boarding does not replace headless job ownership or admission.
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
session bindings, native-session delivery evidence, push events,
agent recovery and dashboard resource controls. These need meaningful
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
python3 scripts/pilot.py --binary target/debug/privateer
```

Dashboard tests use isolated databases, temporary Unix sockets and Ratatui's
in-memory terminal backend. They cover non-mutating observation, bounded views,
terminal replacement, socket replacement, malformed replies, timeouts, demo
isolation, ship scoping, custody transfers, idle ship registration, mouse input,
resize and the reserved boat module. The PTY smoke also drives real isolated runtime binaries for mail, boarding,
presence binding, repeated focus and management against a simulated Herdr socket
and a fake harness. It does not contact a real Herdr session or provider and
does not prove model-driven execution. Use a real isolated Herdr session to qualify
interactive navigation on the installed server version before fleet adoption.
