# Pi conversation view: `/calm`

Privateer's Pi sessions load `/calm` explicitly: attended first mates,
`start --hosted` first mates, and hosted Pi crew. Pi and Pi-signed use it;
OMP and other harnesses do not. Nothing is installed in personal Pi resources.
The same launch paths work with omitted legacy bundle selection or an explicit
`tool_bundles: []` bounded plan. Bounded preflight recognizes only the exact
runtime-owned Calm path paired with attended mode or the hosted crew adapter;
no arbitrary `-e`, stale hosted marker, registry approval, or headless inheritance
can authorize it. Native HOME, agentDir, permission policy and provider/model
selection are not changed.

Calm is **off by default**. `/calm` toggles it; arguments have no special meaning.
The choice is stored as `on\n` or `off\n` in
`$PRIVATEER_HOME/config/calm`, using a private temporary file and atomic rename.
A failed write leaves the current choice unchanged. Hosted crew use their
operational home, not the checkout directory. Each subsequent session reads
that home's preference; already running sessions retain their own choice.

## Visible behavior

While on, one logical run (`agent_start` through `agent_settled`) replaces the
stock Working row with a yellow two-row sailboat over blue water. No Calm status
row is added. The hull moves every 880ms; water advances every 220ms. Rendering
uses terminal cells, clamps on resize, and has a deterministic narrow fallback.
The animation freezes at its last rendered position between runs, resumes in
the same direction, and resets for a new session or extension lifetime. Settle,
abort, error, shutdown, replacement and reload release its scheduler/widget.
Retry and compaction indicators remain stock.

Herdr reads a Pi pane's working state from that stock Working row, so hiding it
would make a working first mate look idle in Herdr's sidebar. In a Herdr pane
(`HERDR_BIN_PATH` and `HERDR_PANE_ID` set), Calm therefore reports the state
itself while on: `herdr pane report-agent --source privateer-calm --agent pi
--state working|idle` as runs start and settle, numbered with `--seq`. Turning
Calm off, shutdown and disposal run `release-agent`, returning the pane to
Herdr's own detection. Outside Herdr nothing is reported.

Calm hides:

- Collapsed thinking, without hiding explicitly expanded reasoning.
- Each settled assistant text block from a tool-use step (also a length-limited
  step carrying tool calls), **only** when its raw text has no newline and its
  trimmed length is below 240. Streaming, multiline/substantive and final text
  remains visible.
- The call/text-result shells for `read`, `bash`, `edit`, `write`, `grep`, `find`
  and `ls`. Images remain visible. No tool is re-registered or executed through
  a replacement wrapper; an extension overriding a built-in keeps its executor.
- Complete Privateer-mail custom rows, including their leading spacing.

Turning Calm off restores ordinary rendering without cycling Pi's expansion
settings or overwriting `/export` confirmation notices. Existing controllable
rows redraw; the terminal's old scrollback is not rewritten.

## Mail preservation

Privateer's existing producer (`AttendedMail.message`) uses `customType:
privateer-mail` and home/ship/sequence/count details, **not** an ordinary user
prompt or a First Mate marker. Only that exact, home-matching custom message
shape is classified. Human user text, including mail-shaped near misses, is
never classified, rewritten, acknowledged or diverted into another input path.

Pi 1.2 keeps these custom notifications out of its string-only queued-message
listing already, but its Escape/dequeue clear operation also clears the custom
agent queues. Calm's capability-checked restore adapter keeps those exact
custom objects in their original priority/order while returning only the human
queued text to the editor. If a stopped run leaves them queued, it waits for
idle and tree navigation, then starts one custom-message turn with the notice
`Privateer supervision continues in a new turn.` Native draining delivers the
rest. A preparation refusal restores the first input unless its durable custom
receipt already exists. Compaction-owned delivery is not started independently.

## Compatibility and limits

There is no numeric Pi version gate. Adapters independently probe exported
`InteractiveMode`, `AssistantMessageComponent` and `ToolExecutionComponent`
methods, then scope changes to mounted interactive transcript components.
Export tool components are unmounted and retain ordinary rendering. The queue
adapter additionally checks the current session's methods, receipt reader and
agent queue shapes before retaining anything, including boolean `isCompacting`
and the durable-receipt reader `sessionManager.getBranch`. Tests remove each
required seam independently and cover Escape/dequeue, order, once-only delivery,
compaction, refused versus receipted failures, mid-wait arrivals, shutdown and
rebind epochs. Missing seams emit a bounded,
named warning and leave that adapter stock. These exported layout/queue members
are **internals**, not a promised stable global transcript-filter API.

Arbitrary tool names, custom extensions, user-bash, skill/summary rows and generic
notices remain visible. Calm changes no tool execution, provider route, model
context, transcript storage, mail acknowledgement or export data. It patches
no installed harness file. This is a fresh implementation from approved behavior
documentation and Privateer's own dashboard boat geometry, not First Mate code.

## Bounded checks

- `node tests/pi-calm.mjs`: simulated extension events, queue transitions and
  deterministic clock/geometry assertions; included in `cargo test`.
- `python3 scripts/pi_calm_native.py /path/to/pi`: isolated installed-Pi entry-point,
  real exported layout methods, a real `AgentSession`'s queue/restore seams and
  real built-in subprocess execution. Also checks actual headless, attended and
  hosted-crew resource inventories for legacy and explicit-empty bounded plans.
  Uses a fixed clean environment allowlist and disposable authless HOME/agentDir,
  offline/no-MCP flags, fixture inference refusal, and Linux libseccomp transport
  refusal installed before exec (also inherited by descendants). It fails closed
  when the transport guard is unavailable. Delivery events are simulated; it
  makes no model or network request. Records the installed version without
  gating the Calm adapters; the bounded loader checks the installed Pi's flags, not its version.
- `cargo test --test attended --test hosted --test pi_calm -- --test-threads=1`:
  real launch argv/home/custody checks, including hosted crew after #96 and
  non-Pi exclusion.

These checks are not screenshot, HTML-DOM, credentialed-model, or full live-TUI
end-to-end evidence. No deployment is performed by the implementation/tests.
