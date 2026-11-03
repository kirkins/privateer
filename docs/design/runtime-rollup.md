# Runtime observability: retained records and backlog rollup

This completes the fourth code increment of the
[#452 observability plan](https://github.com/kirkins/privateer/issues/452#issuecomment-6590312745).
It does not configure a runtime-domain manifest or authorize deployment.

## Additive contract

Only the existing `runtime.v1` envelope changes. Unversioned inventory/status,
`fleet.runtime.v1` wire DTOs, inspection RPCs, scopes and provider routes are
unchanged. Old peers still parse the same requests. The host composes the
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
as `local:alpha:jobs`. Missing/budgeted sources cannot make a total complete.
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
visibility, filter, limit, source and snapshot binding. Use the named paging
tool with the same ship and `paging_limit:50`, not the runtime tool. Later
windows do not claim a complete retained scan.

Scheduled/unscheduled task totals include recorded terminal tasks; they are not
synonyms for backlog. Backlog excludes done/cancelled tasks. Its scheduled and
unscheduled splits count only returned work rows (`count_basis:returned_page`),
and are incomplete while more rows remain or the scan was partial. Clients may
inspect subsequent disjoint work rows to build a larger split, but must retain
all coverage gaps and cannot sum scan-window counts. The rollup intentionally
does not drain an arbitrarily large home on every fleet status request.

Composition admits at most 12 selected ships and starts no further reader after
three seconds. Existing reader scan/byte/time bounds still apply; this is a
cooperative admission budget, not a hard filesystem/transport deadline. Omitted
ships say `rollup_budget`. A duplicate ship handle resolving to another machine
says `ambiguous_target`; it is not redirected. Noncurrent remote census facts
(including disconnected, cached and reverse-only cases) do not trigger new
inspection calls. A successful record read never promotes stale census evidence.
Runtime-section contact reuse and fresh inspection contacts are independent;
`contact_scope` and the per-ship inspection attempts make that distinction
explicit. No inspection cache is introduced.

Observation still writes no home/work/presence/mail state. Existing inspection
readers may create bounded registry continuations; this is separately disclosed
as `inspection_cache_writes:bounded_registry_continuations_only`, alongside the
existing runtime process-memory cache disclosure. Continuations may expire or
be evicted under existing bounds; refresh rather than treating expiry as zero.

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
