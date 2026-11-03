# Runtime observability: retained records and backlog rollup

This completes the fourth code increment of the
[#452 observability plan](https://github.com/kirkins/privateer/issues/452#issuecomment-6590312745).
It does not configure a runtime-domain manifest or authorize deployment.

## Additive contract

Public runtime changes stay inside the existing `runtime.v1` envelope. Unversioned inventory/status,
`fleet.runtime.v1` wire DTOs, RPC names, scopes and provider routes are
unchanged. The existing `fleet.inspection.v1` envelope accepts an optional
`summary_budget_ms` integer (1–3000): a nonpaging summary with that remaining
wall-time budget. Ordinary inspection requests/replies stay unchanged. Old
peers refuse this added envelope; summaries then remain unknown, with no retry
that could mint tokens and no operator fallback. The host composes the
existing `ship_jobs` and `ship_work` readers, which independently enforce
custody, worker visibility, bounds and retained-record semantics. No new job
model, operator fallback, scheduler poll or reconciliation is introduced.

Each selected ship has `rollup`:

- `checked.workers` and `checked.sessions`: nullable values and independent
  completeness from the current census. Workers include first mate, crew,
  errands, active workers and commands; sessions retain their state buckets.
- `metrics`: `inspected_nonterminal_jobs`, `retained_terminal_jobs`,
  `retained_job_states`, `scheduled_tasks`, `unscheduled_tasks`, `task_backlog`,
  `scheduled_backlog`, `unscheduled_backlog`. Every metric carries `value`,
  `complete`, `sources` and `count_basis`.
- `jobs` and `work`: inspection `counts`, `scan_complete`, `history`, observation
  time/mode, `more`, `continuation`, `snapshot`, `paging_tool` and `paging_limit`.
  `attempted`/`successful` describe these independent record reads, not a new
  census. Missing reads have null counts and paging fields, never empty totals.

The same metrics are aggregated in `observed_totals`, with source handles such
as `inspection:local:alpha:jobs`. These are inspection contributors, not IDs
in `coverage.sources` (which describes runtime-section evidence). Missing/budgeted
sources cannot make a total complete. `retained_job_states` contains only mutually
exclusive states; the overlapping `completed` aggregate lives separately in
`retained_terminal_jobs` and the original inspection `counts`.
Without any observed source the value is null (except a proved empty selected
inventory). Fleet here means **selected directory**, not a configured runtime
domain. `layers.jobs.value.retained` now holds the terminal metric and
`retained_states` holds the state breakdown. Jobs layer completeness means the
selected recorded scans completed, never complete lifetime history.
`layers.backlog.value` remains the existing open-task scalar for compatibility;
its additive `recorded_work` breakdown carries independent metric completeness.
A backlog can be known while worker/session evidence is unknown.

## Paging, bounds and wording

Inspection counts cover the authorized scan window, not just returned rows.
For example, 60 retained jobs may produce a complete count of 60 and a first
page of 50 with `more:true`. Continuation **counts overlap** the previous scan
window; never sum those counts. Opaque continuations retain the existing grant,
visibility, filter, limit, source and snapshot binding. Summary `continuation`
is always null, even with `more:true`: status polling neither mints nor prunes
paging handles, locally or remotely. Begin the named paging tool with the same
ship and `paging_limit:50`, **without** a continuation, to obtain an independent
snapshot and handle. Continue that tool's snapshot, not the summary's snapshot.
Later windows do not claim a complete retained scan.

Scheduled/unscheduled task totals include recorded terminal tasks; they are not
synonyms for backlog. Backlog excludes done/cancelled tasks. Its scheduled and
unscheduled splits count only returned work rows (`count_basis:returned_page`),
and are incomplete while more rows remain or the scan was partial. Clients may
inspect subsequent disjoint work rows to build a larger split, but must retain
all coverage gaps and cannot sum scan-window counts. The rollup intentionally
does not drain an arbitrarily large home on every fleet status request.

Composition admits at most 12 selected ships, using a deadline three seconds
from **runtime request start**, not composition start. Every inspection gets
only the remaining budget: remote transport is capped by it and local SQLite
observation/custody checks cooperate with it. No read starts with less than
one millisecond remaining. An exhausted prior runtime phase therefore starts
no rollup inspections. This is not a three-second SLA for the whole request:
the pre-existing runtime contact phase runs up to eight peers in parallel,
with up to three ten-second calls per peer for exact legacy-envelope downgrade
refusals (up to about 30 seconds of transport allowance). Local runtime/census
phases have their own observation limits. Filesystem I/O, process teardown and
OS scheduling remain cooperative/unbounded, so there is no hard end-to-end
worst-case wall time. Rollup network waiting adds at most the remaining part
of three seconds, not another ten seconds per inspection.

Fleet `rollup_coverage` reports `admitted` ships (both readers attempted),
`omitted` ships (at least one reader skipped), `omissions` counts by reason,
`reason`, `complete`, `inventory_complete`, `ship_limit`, `budget_ms` and
`basis:selected_ships_both_inspections_attempted`. A partial ship still contributes
its observed metrics. This is admission coverage, **not** success or scan
completeness: even all-admitted fleets can have unknown or partial metrics.
Omissions exclude unenumerated directory entries, so incomplete inventory is
reported independently. The MCP aid renders this distinction and never fills
missing admission data with zero. Budgeted ships say `rollup_budget`. A duplicate
ship handle resolving to another machine says `ambiguous_target`; it is not redirected. Noncurrent remote census facts
(including disconnected, cached and reverse-only cases) do not trigger new
inspection calls. A successful record read never promotes stale census evidence.
Runtime-section contact reuse and fresh inspection contacts are independent;
`contact_scope` and the per-ship inspection attempts make that distinction
explicit. No inspection cache is introduced.

Observation still writes no home/work/presence/mail state. Summary reads do not
create or prune registry continuations (`inspection_cache_writes:none`), alongside
the existing runtime process-memory cache disclosure. Explicit paging tools
still use the bounded registry: their continuations may expire or be evicted
by other explicit paging requests; refresh rather than treating expiry as zero.
Repeated status polls are tested not to evict either their own reader's or
another reader's in-progress paging handles.

The plain MCP aid names ships whose facets are missing and caps facet prose at
12 ships, with an explicit omitted count. Structured ship rows are unchanged by
this presentation cap. Contact facets with only an earlier receipt are now
`state:stale,current:false`; `read,current:true` still means read, not healthy.
Unknown is never described as idle. Jobs/tasks are not workers. Retained history
before available records remains unknown, even after a complete selected scan.

## Acceptance audit

All five code acceptance bullets in the plan have corresponding surfaces/tests:

1. Inventory, reachability, census, backlog and unknown-domain completeness are
   independent (`runtime_layers`, real door and MCP fixtures).
2. Runtime contact attempts/results, reuse, cache/carry and receipt ages are
   independent of worker evidence (`runtime_linked`, MCP contacts test).
3. Session verification uses bounded safe `session_unverified` categories;
   binding renewal/replacement is fenced in native presence code (prior
   increments' presence/census tests; MCP anonymous-session fixture).
4. Checked workers/commands/sessions, nonterminal/retained terminal records,
   and scheduled/unscheduled backlog have distinct metrics (rollup unit tests,
   MCP retained/paging fixture, active/idle/unverified session fixture).
5. Sources, scan completeness and paging metadata survive truncation and
   continuation (60-row and 300-row MCP fixtures, rollup unit tests).

Limitations remain explicit: no complete runtime-domain membership proof,
complete retained lifetime history, exhaustive scheduled backlog split beyond a
page, fresh reverse-only inspection, hard real-time snapshot, real ChatGPT
text/voice account qualification, or production deployment is claimed. These
are not converted into zero counts or healthy/idle labels.
