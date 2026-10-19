# Privateer's terminal bridge

The optional Rust TUI is one column: **Fleet**, **Needs you**, **Recently done**.
Herdr owns terminals and navigation. There is no sidebar, per-ship view, Board,
Watch, ship-management control, automatic focus, pane creation or pane reorder.
Existing safe Herdr and management library helpers remain available to other
consumers; they are not bridge controls. Nothing here installs or deploys.

```sh
cargo build --locked --features dashboard --bins
./target/debug/privateer-dashboard --once --no-herdr
./target/debug/privateer-dashboard
```

The dashboard dependencies remain optional. Ordinary builds of `privateer`
need no TUI. The dashboard has no sample or demo mode: it observes registered
local homes only. `--once` prints one bounded read-only JSON observation, and
`--no-herdr` keeps it from connecting to Herdr.

## Hosted machine bridge

With a local ship's `first_mate.host = "herdr"`, host orchestration in `serve`
maintains one background `bridge` space for the machine's **per-user fleet
registry**, shared by independent home dispatchers. It uses the release's bundled
`privateer-dashboard`, or an executable sibling of the source/legacy runtime;
a PATH-only wrapper is not an automatic hosting target. Paths are shell-quoted,
cwd is the hosting ship folder, and the bridge observes the fleet with **no
current-home fallback**. Another dispatcher never retargets a live bridge.
Explicit dashboard invocation and its observation loop still create, arrange and
focus nothing. No harness/model/provider is launched or changed by this feature.

Coordination lives beside the existing fleet registry at `<registry>.bridge/`:
an owned 0700 directory, no symlinks, private regular single-link files, an atomic
cross-home `guard` flock, a bounded atomically replaced/fsynced claim and a separate
`live` flock held by the **direct dashboard process** until exit. The claim pins
registry, guard/live-file incarnations and hosting-home identities/origin, a
unique launch token, full socket
device/inode incarnation, pane/workspace/tab/terminal and exact agent/session.
The operator-only startup handshake validates that token and the new endpoint;
it does not mutate Herdr. No titles or metadata tokens confer ownership.

Ordinary ticks create without focus, label/tag the verified space as `bridge`,
and repair its presentation. Bare operator `privateer` first revalidates that
claim, hosting and live lease and uses `pane.focus` exactly once; it does not
send input or exec another dashboard when reuse succeeds. Hosting-off, missing
installation/socket, unknown home and unverifiable ownership retain the existing
inline dashboard/onboarding fallback. Agent entry remains refused. Herdr snapshot
and focus are separate operations, not an atomic occupant lock.

Closing a bridge permits recreation only after a valid same-incarnation snapshot
shows its entire old location gone **and** its direct dashboard's kernel lease
has disappeared. A removed or replaced lease/guard path is not lease expiry:
its recorded device/inode must still match. A transient missing pane while the dashboard still lives, or an
unadopted launch whose pane still exists, cannot cause another create or input
replay. A new claim fences delayed old-token adoption. Retries reserve durable
60-second exponential backoff capped at an hour, independently of wake budgets.
A changed socket, unexpected occupant, ambiguous/malformed snapshot, replaced
registry/home, or a create with an unknown outcome fails closed. In particular,
a lost create acknowledgement without an exact recorded endpoint is retained
for explicit recovery rather than guessed away by elapsed time. This does not
add a generic force-recovery/kill/close command or claim autonomous recovery from
arbitrary Herdr server replacement.

**Ordering limitation:** documentation-only inspection of Herdr's embedded JSON
API schema (artifact SHA-256
`17d7536f2da50859523680ef279dffcac8e4a235f4e46fe8da6c2ec44141897c`)
found `workspace.move_block {workspace_ids,before_workspace_id}`, ordinary
`workspace.move {workspace_id,insert_index}`, and presentation metadata; no
supported global pin/outside-machine-row parameter or RPC. No Herdr process/socket
was invoked for this check. The accepted fallback is first **under Local**, using
existing local workspace grouping: bridge, valid mates and their crew. No global
machine-row reorder, remote-group mutation, invented pin field or configuration
change is attempted. Unrelated spaces retain their relative order.

One fake-Herdr behavior scenario in `tests/hosted.rs` exercises overlapping
independent homes, exactly one initial launch/pane, one explicit bare-entry focus,
restart/presentation recovery, stale/ambiguous/replaced endpoint refusal, durable
retry, pending-token fencing and closure/recreation. Direct dashboard children
use disposable PTYs and must exit 0, be reaped, restore terminal settings and
release their kernel lease before fixture cleanup. It asserts no automatic focus;
it is not a rendering or live-Herdr qualification.

## Fleet and focus

Fleet discovery uses the existing per-user registry (`PRIVATEER_FLEET` can select
an isolated registry). An absent registry is not initialized. `--home` or
`PRIVATEER_HOME` supplies a legacy fallback when the registry is empty. Each
row identifies a ship, its home, and current recorded tasks/titles/status,
including delegated work's originating home and owner. Offered handoffs do not
transfer custody. Rows describe bounded recorded facts, never live process health.
Unregistered observed owners remain distinguishable from registered homes.
Missing or conflicting presentation stays visible and disables focus.

Up/Down or j/k selects a ship. Enter requests focus of its **existing** validated
mate space. It cannot launch or create anything. Only an explicit
`--herdr-socket /absolute/socket` or Herdr's injected socket when `HERDR_ENV=1`
is used; `--no-herdr` disables it. It never guesses a global socket or provider.
The primary binding is the current unexpired presence binding, otherwise the
first exact manual pane/terminal binding. Similar labels never establish ownership.
The existing socket/pane/terminal/workspace/tab/agent/native-session checks
still run immediately before `agent.focus`. Fresh fleet binding and home identity
are also checked in the worker. Stale/reused/replaced/ambiguous selections refuse;
refresh and explicitly reselect. Truncated fleet scope also refuses focus, answers
and history read intent. Refresh never focuses anything. Herdr snapshot
and focus are separate server operations, **not** an atomic occupant lock.

Optional `<home>/dashboard.yaml`, `.yml` or `.json` presentation retains the
existing strict input/duplicate/alias limits and cross-home conflict checks.
Keep only one file. [Example](../examples/dashboard.yaml). No presentation entry
creates a home, custody, remote access or authority. Local registered homes are
supported; remote observation/action adapters remain explicitly unknown.

## Needs you

Attention consumes ONLY the reviewed [runtime.needs v1 contract](runtime-needs.md).
Unread mail, prose, task titles and generic blocked tasks do not classify needs.
Every source's origin, unknown categories, unavailability and retained history
remain explicit. Empty items are not an all-clear. Copied origins are ambiguous,
not independent ships. Last recorded capacity waits/orphans are evidence, not
proof of a live/stopped process; there is no generic cancel/ok/unblock button.

Tab switches Fleet/Needs selection. Enter opens an inline answer. The actual
question is shown with its source ship, local task and version. Left/Right picks
from supplied choices; without choices, type free text. Enter submits; Escape
cancels. A reply is `mail.send` from captain to the original sender with the
original request ID as correlation and a stable client-generated outcome ID.
Acknowledgment never resolves a request. Withdrawal, authoritative reply and
terminal-task closure clear items only through fresh runtime observations.

Typed production approvals show the **exact artifact, environment and charter
version**. `a` selects approve, `d` deny, Enter records the selected outcome via
existing `approval.grant` / `approval.deny`, with request_id + expected_version
and a stable explicit grant ID. Plain replies or prose never create a grant.
This is not deployment: artifact/environment consumption and production
authorization remain the runtime's existing gates.

An explicit answer uses a narrow crate-private existing-home action open,
approved for this local bridge: READ_WRITE without CREATE, mkdir, chmod,
migration, repair or journal-mode rewrite; incompatible schemas refuse. It uses
the already-resolved operator caller, foreign keys and FULL synchronous writes.
Selection pins normalized home, directory/database device+inode and durable
origin. The worker validates current configured scope, duplicate origins,
identity around open and immediately before dispatch, and requeries semantic
item/version/binding on that **same connection**. It invokes only reply/grant/deny;
existing core/mail transactions retain atomic open-request/recipient/charter/
consumption checks. There is no new outer transaction or authorization bypass.
This guards ordinary workflow replacement races, not a hostile-same-user
filesystem sandbox or an impossible atomic path/SQLite guarantee. Nonlocal
homes without equivalent reviewed guards cannot mutate through this surface.

Errors retain the draft and stable ID, not an automatic retry with a new ID.
Only a proved identical committed outcome reaches the runtime's idempotent retry
path after closure. Refresh removes only genuinely closed items. Stale selection
never silently moves an action onto the row that replaced it. Selection pins
physical directory/database identity, durable origin and source ID, not just home
and ID. A same-origin copy with identical IDs requires explicit reselection after
a complete fresh scan. Ordinary observed-time changes do not invalidate a draft;
an already-open draft keeps its immutable original binding and refuses replacement.

Attention queries one local home/page per worker pass (250 ms cadence), at most
100 items/page, four pages/scan, 200 retained sources and 400 retained items.
Partial scans do not replace a completed observation. Bounds/unavailable sources
retain old items as stale with explicit errors. Empty continuation pages count.
Each scan pins physical identity and origin independently of items, including an
empty first continuation page. Every page rechecks identity after the query;
replacement discards the pending scan/cursor and restarts at the beginning, never
combining generations or publishing partial all-clear results.
Per-home successful scans wait two seconds; failures back off up to sixteen
seconds rather than retrying each input tick. Action rechecks have the same
four-page bound; observations older than ten
seconds cannot act. Large fleets may therefore require later fresh observation;
unsupported or over-budget data is never healthy/quiet. The query's cooperative
250 ms budget cannot preempt OS/filesystem I/O. No remote inference or live
CI/provider/dispatcher/wake-health adapter is introduced.

## Recently done

This shows **finished tasks**, not inferred merges. Completion requires a done
task and its last `task.update` audit timestamp. Missing/pruned audit time does
not become newly recent. Rows retain home and current recorded owner, dim after
90 minutes and expire after three hours (including while the UI stays open).
Arbitrary event prose, titles and exit status cannot establish a merge. Reads and
retention are bounded to 200 rows, with source truncation visible.

`Snapshot.truncated` means recorded display/history rows were clipped, not that
runtime homes went undiscovered. This remains visible as “recorded history
clipped”; it does not suppress authoritative typed needs, answers, explicit history
reads or validated existing-pane focus. The separate `discovery` state is complete
only after successful bounded fleet/home mapping (or a successfully read explicit
local fallback). Default/older serialized snapshots are unknown, never implicitly
complete. More than 200 discovered identities, unavailable homes or invalid mapping
remain incomplete; unknown/incomplete scope refuses actions even with retained
facts. An empty registered fleet cannot resurrect a retired fallback home. Failed
refreshes retain evidence, not permission to act or an all-clear.

## Mail history and controls

`i` opens separate captain history through existing `mail.inbox` / `mail.requests`
commands. Enter marks read using the existing contiguous read-prefix guard;
opening/refreshing alone cannot acknowledge unseen mail or resolve a request.
Needs-you answers are separate. Origin/home, full supported body and payload,
scroll preservation, bounded queues and failure retention are retained. Bridge
history queries use an existing READ_ONLY Store and existing mail query dispatch,
never CLI initialization/migration. Explicit Enter uses the same origin and
filesystem-identity-pinned existing-home action open, allowlisting mail.ack only.
Read/unread markers are generation-bound to that identity and origin, invalidated
on replacement, retirement, ambiguity or failed observation. History selection
also pins physical generation, origin and message ID: disappearance or replacement
clears selection rather than selecting an adjacent row. Enter cannot mark another
row until explicit navigation/reselection. Incomplete/unknown/unavailable fleet scope refuses
history write intent; retained rows may remain visible read-only. Before acknowledgement
the pinned connection rechecks the selected message and actual unread mailbox
prefix against the observed page. Requests merged from later pages cannot skip
unseen earlier mail. Operator privacy is checked before loading captain bodies,
not merely at the standalone binary boundary. No refresh acknowledges mail.
Legacy library command-backed helpers remain for other consumers; the bridge
never calls them. History is loaded only on explicit entry (or compatibility
JSON `--once` output), not used as attention.

| Key | Result |
|---|---|
| Up/Down, j/k | Select in Fleet, Needs, or history |
| Tab | Switch Fleet / Needs |
| Enter | Existing mate focus, inline answer, or history read |
| Left/Right in answer | Pick a supplied choice |
| a / d in approval | Select approve / deny; Enter commits |
| Page Up/Down, wheel | Scroll details / navigate list |
| i | Separate mail history |
| v | Voyage sea (v or Escape returns) |
| Escape / Home | Return to bridge (or cancel draft) |
| q / Ctrl-C | Exit (q is literal text while answering) |

Mouse clicks select, never focus or send; hit targets follow list offsets and
are cleared on resize. At least 40 columns by 14 rows is required. Terminal
control and bidi-override characters are removed. Summaries and messages can
still contain private text; this is not screenshot sanitization.

## Voyage

`v` replaces the bridge with an animated sea of the fleet, drawn only from the
snapshot and Herdr view the bridge already holds; it reads and changes nothing.
This machine's ships come first, liaison leading, then each linked machine's
ships under its own heading, drawn as distant boats because their state is
read on that machine. A boat's shape says its state without colour or motion:
sails up while its first mate works (a running wake, or its own validated
Herdr pane reporting `working` in a fresh view), anchored when idle, a bare
hull when its dispatcher is stopped, `!` when its state cannot be read and
`?` when there is none. Presence alone does not raise the sails, because a
hosted first mate holds it whenever its pane is open. Small boats trail a ship
for its active crew, a bottle drifts from the liaison to a ship with open
requests (the sender is approximated), and a flag marks a ship that needs the
captain. The bottom row is a key to what is on screen. Spare rows become open
water between the rows of ships; as space runs out names go, then boats become
one symbol each, then a count of ships not shown. `--reduced-motion` holds the
sea still.

## Reliability and verification

State/action and Herdr workers use bounded one-item channels. Slow source/action
work stays off the input loop. Herdr refreshes every two seconds with failure
backoff up to sixteen seconds. Its correlated JSON protocol keeps the existing
1 MiB/800 ms response and pane limits. Closing the dashboard does not kill agents,
recover jobs or assume a wrapper's payload stopped. No service/config/provider
route/model/permission/production change is part of this UI.

`--once` emits bounded read-only JSON with one attention scan step plus explicit
not-yet-observed sources; it does not imply a complete multi-home attention scan.
SQLite may use its ordinary WAL/shared-memory sidecars. The boat helper remains
reserved and tested; the bridge runs no animation timer.

Validation uses disposable databases, fake Herdr sockets and actual keyboard to
action/command scenarios, not screen snapshots. Preserve the identity/privacy/
custody/read-only safety tests; old tab/boarding-layout smoke expectations are
replaced. The live PTY proof observes incremental current terminal cells, including
cursor placement, erasure and split UTF-8/escape chunks; it never matches concatenated
raw frames. Denial requires a newly changed outcome match, not the earlier reply's
notice. Unsupported terminal controls fail the proof rather than being stripped.
Run through resource admission on a clean frozen SHA:

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -j1 -- -D warnings
cargo test --locked --all-features -j1 -- --test-threads=1
cargo check --locked --no-default-features --bin privateer -j1
python3 scripts/dashboard-smoke.py --binary target/debug/privateer-dashboard
```

Both PTYs and their local fixture CLI children receive a literal clean environment
allowlist with disposable HOME, XDG, fleet, resource and temporary roots, a fixed
system PATH and no inherited credentials, actor/role or Herdr endpoints. Only these
disposable authless children are isolated from the supervisor's role: the supervising
agent environment and real operator guards remain unchanged. The actual absolute
built binaries are used. Exit/reap and terminal/screen/mouse restoration are checked
before fixture cleanup; forced fallback cleanup is a failure, never a pass.

These are offline/local qualifications, not live Herdr/API canaries or proof of
native model autonomy. Live fleet adoption/deployment requires separate approval.
