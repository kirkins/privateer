# MCP and voice: fleet visibility and runtime parity

## Status and recommendation

Follow-up to [#452](https://github.com/kirkins/privateer/issues/452), the captain's field report and follow-up of 2026-10-27. **V1 implements opt-in local runtime metadata and coverage; V2 adds linked recorded runtime status; V3 adds identity-checked worker/session observations; V4 adds selected-ship job/work inspection; V5–V8 remain gaps.** No scope, grant or provider route is expanded. The inventory below distinguishes delivered V1/V2 behavior from the remaining proposed tools. Examples use fictional machines `alpha`, `beta`, `gamma` and ships `survey`, `relay`, `lookout`.

Recommend one **core-owned, permission-filtered runtime view**, built from [fleet.snapshot](fleet-snapshot.md), not a second MCP/voice scheduler. First fix coverage and unknown-versus-idle reporting, then expose workers, jobs, scheduling and capacity as separate dimensions. Use existing grants and gates; do not expand anyone's scopes on upgrade. ChatGPT used by voice is the same scoped MCP caller as text. Migrating the bundled trusted live-voice door to scoped v2 is separate, explicit work.

This extends [MCP front door §6](mcp-front-door.md#6-captain-operation-inventory-and-scope-mapping) and phase 2.5. Reuse [2.4c's coverage contract](door-mail-coverage.md#37-coverage-on-every-result), identities, privacy rules and freshness vocabulary. Runtime observation and captain-mail completeness remain **independent**: seeing every worker does not prove the remote inbox is covered.

## 1. Native inventory and current MCP gaps

Sources of truth are the runtime, its host ledger and identity-checked host observations, not task titles or model narration. “Across machines” below describes native behavior, **not** a delegated door capability. Existing `fleet.snapshot` is operator-only. Its gateway returns only the answering machine's section; its caller merges configured links, never recursively traverses the fleet.

| Native capability | Where it lives / what is served now | MCP today | Proposed scope / tool | Across machines and remaining gap |
| --- | --- | --- | --- | --- |
| Directory, local attendance and dispatcher | [door.rs](../../src/door.rs), `ships_visible`; [machines.rs](../../src/machines.rs), linked directory | `fleet_ships {}` preserves legacy inventory; opt-in `runtime.v1` adds nullable local errors and per-facet coverage | `fleet:read`; extend `fleet_ships` with coverage | Directory is not live evidence. V1 keeps registered missing homes and unknown local attendance/dispatcher facts; directory-only linked activity stays unknown. V2 adds bounded recorded job/presence facts from callable peers and dated cached/carried sections; remote dispatcher remains unknown, not a field already in the snapshot. |
| Local work summary | `door.rs`, `status_visible`: status plus last 20 wakes; `working` counts only running wakes | `fleet_status {}` unchanged; `schema:"runtime.v1"` selects safe local and linked recorded metadata with per-source freshness | `fleet:read`; versioned `fleet_status` projection | Does not include hosted first-mate activity or crew in `working`. Replacing that interpretation needs explicit schema migration. |
| Machine/ship live snapshot | [snapshot.rs](../../src/snapshot.rs), `own`, `snapshot`, `shown`; machine rows, routes, ships, needs, caps/unknowns | Opt-in `runtime.v1` recorded metadata via dedicated `fleet.runtime.v1` gateway | `fleet:read`; `fleet_status`, optional machine/ship selectors | Direct gateway, five-second reused reads, cached fallback, directory-only unknown, sections carried on `fleet.collect`. V2 reuses the bounded snapshot ship reader and job-kind joins; delegated replies contain only `RuntimeSection`/`RuntimeShip`/`RuntimeJob`. Host memory reuse is grant/selector-bound; stored operator sections are filtered dated fallback only. No operator JSON is returned. |
| First-mate presence and sessions | [presence.rs](../../src/presence.rs), lease/session/asleep holder; snapshot `presence`, `presence_since_ms`, `hosted`, `harness` | Legacy local `attended`; opt-in `runtime.v1` adds V3 checked activity/session counts | `fleet:read`; `ship_runtime` and status summaries | Snapshot carries hosted/asleep/session/absent/unavailable/unknown. V3 checks bound host/session identities separately from the lease. A lease names one holder, not all sessions; absent does not mean no headless wake. |
| Herdr first-mate/crew activity | [herdr.rs](../../src/herdr.rs), `Pane`, `Client::snapshot`, bindings; [host.rs](../../src/host.rs), blocked/duplicate checks; [session.rs](../../src/session.rs), `crew_panes` | V3 metadata-only checked census in opt-in ships/status | `fleet:read`; metadata-only `ship_runtime` session rows | Native `session.read` routes to linked ships but is not invoked by the census. V3 uses bounded `session.snapshot`/`workspace.list` only, checks socket/pane/terminal/adapter-session identity, distinguishes crew/viewers, and reports duplicate spaces as anomalies. No screen scrape or sidebar `repair` (which arranges panes). Missing adapter identity remains unknown. |
| Live jobs, kinds, progress and waits | Snapshot jobs: `first_mate`, `crew`, `errand`, `command`; times, route, `doing`, `wait`; at most 20 expanded per ship, full `jobs_total` | V2 metadata: owned nonterminal IDs/kinds/states, unattributed wait kind, full nonterminal total and explicitly partial expanded-state counts | Metadata `fleet:read`; progress text also `mail:read`; `fleet_status`, `ship_jobs` | Already carried/direct/reused/cached. Queued is capacity wait; parked has child/next; orphaned remains uncertain, not finished. Expanded-list length cannot substitute for totals. |
| All job states and inspection | [jobs.rs](../../src/jobs.rs), `job.list {limit,cursor,after_id}`, `job.status {id}`, durable job record | V4 closed DTOs and bounded selected-home reads via dedicated `fleet.inspection.v1`; retained history gaps explicit | `fleet:read`; `ship_jobs`, `job_inspect`; text additionally `mail:read` as below | Native list keyset pages include terminal jobs. V4 adds selected-ship delegated local/remote reads with opaque high-water continuations (boot-bound across Linux/macOS gateway processes), batched progress/resume joins and scan-window count coverage. `public_job` serializes `command` (argv), cwd, ledger, process/binding fields: **not a safe external DTO** and not used by V4. Status reads do not establish fresh process liveness. |
| Scheduled work, task ownership and waits | [scheduling/commands.rs](../../src/scheduling/commands.rs), `work.status`; [observation.rs](../../src/scheduling/observation.rs); ready/offered/claimed/executing/waiting/stopped/fault/unscheduled, reason/wait/revision/lease/check times; backlog alarm | V4 `ship_work`, bounded pages, separate scheduled/unscheduled/backlog and state counts; no scheduler invocation | `fleet:read`; `ship_work` | Native `work.status` is local ship/home, bounded at 100 rows without complete paging. V4 adds bounded selected-ship local/remote pages and explicitly complete/partial scan-window totals via the dedicated inspection gateway; no new scheduler. `work.status` alone is not a full job list or census. |
| Bearings/backlog context | [context.rs](../../src/context.rs), `ship.bearings`: tasks, idle/stale tasks, attention, scheduling, capacity waits, hosted pane drift, mail/knowledge text | V4 structured scheduled/unscheduled task and backlog rows; not the mixed bearings prose object | Structured metadata `fleet:read`; mail/knowledge text separately reviewed; `ship_runtime`, `ship_work` | Per home; do not expose the mixed prose object wholesale. Running tasks with no job are explicitly idle-task evidence, not workers. |
| Capacity, holds, host readings and queue | `jobs.rs`, `resource.status`, reservations/queue/crew-memory sources; [admission.rs](../../src/admission.rs), settings/measure/check; [core.rs](../../src/core.rs), charter `max_heavy_jobs` | Absent | `fleet:read`; `machine_capacity` and selected-ship effective-policy projection | Host ledger shared across homes; snapshot only supplies holders behind waits, **not** a capacity report. Add bounded sanitized read per answering machine; never sum duplicate ledgers across ships. |
| Provider/route availability | [crew_routes.rs](../../src/crew_routes.rs), `states`; snapshot machine routes; bearings summary | Absent | `fleet:read`; safe availability in `machine_capacity`/`ship_runtime` | In snapshot across links. Named/account route keys and classified error strings still require visibility/redaction review; reading must not probe or alter routes. |
| Typed captain/resource blockers | [needs.rs](../../src/needs.rs), `runtime.needs`; [runtime-needs.md](../runtime-needs.md); snapshot needs include machine/source/id/waits | `fleet_waiting` gives local raw open captain asks, **not** typed needs or all-clear | `fleet:read` for blocker/ask metadata; `mail:read` for ask content; `runtime_needs` | Native needs is local, read-only, budgeted/paged with explicit unavailable/unknown; snapshot aggregates/deduplicates it with caps. New delegated projection must omit approval argv/cwd and sensitive question/choices under metadata-only reads. |
| Mail/attention completeness | [door/reader_link.rs](../../src/door/reader_link.rs), selected linked mailbox; [door/desk.rs](../../src/door/desk.rs), desk actions | `mail_inbox/show/thread/ack` selected mailbox; `fleet_waiting` local; `get_report` local only | Existing `mail:read`; aggregate tools belong to 2.4c | Reachable selected mailbox is not a fleet subscription. Remote-only captain mail and aggregate waiting depend on 2.4c, not on worker snapshot success. |
| Wake/errand transcript and pane screen | [transcript.rs](../../src/transcript.rs), `wake.transcript`; `session.read` returns recent screen/state | Absent | **Deferred**, no transcript/screen tool in this slice | Transcript local only (linked ship says ask there), normalized/masked best effort; hosted first mate has no log transcript here. Pane reads can route but expose raw terminal content. Neither is a summary-read entitlement. |
| Work mutations, cancellation/retry and admin | `task.rank/enqueue/defer`, `job.cancel/retry`, resource configure/recover; existing charter/custody/admission | `dismiss` only for direct work management; ordinary assignments through `send_message`/`ask_liaison` | `work:manage` for explicitly mapped work changes; `fleet:admin` for shared policy/recovery; §4 ledger | Native commands are not all delegated; add one operation at a time. No generic RPC/argv bridge, no implied captain authority from messaging. |

Current door scope enforcement is the exhaustive `Command`/`requirements` table in [door/scoped.rs](../../src/door/scoped.rs), with [delegated.rs](../../src/door/delegated.rs) execution. The MCP inventory is the smaller fixed map in [tools.py](../../plugins/mcp/tools.py). For example permissions/stow/upgrade have core scope classifications but no tools in that map. New MCP schemas alone cannot add safe runtime parity; core command validation, delegated execution and receiving gateway checks must land together.

## 2. Read contract: coverage before counts

### 2.1 One projection, independent observation domains

Add typed, versioned scoped read operations beside the operator API. Factor/reuse snapshot readers, caps, job-kind/progress/wait joins, route states and cache/collect transport. The existing operator cache may remain internal under the linked machines' shared operator trust; its full payload must never cross into plugin results or delegated wire responses. Prefer filtering at the answering core using the immutable delegated context; filter again at the host before release, including stored operator sections. Cache authorization is never durable permission to expose data. Native snapshot polling updates its bounded `fleet_snapshots` cache/`changed_ms` bookkeeping; document those internal writes separately from work mutations. New scoped observations must not initialize/migrate homes, reconcile jobs, arrange panes, release reservations, acknowledge mail or change presence. Factor read-only capacity readers instead of blindly forwarding `resource.status` and its ledger-open lifecycle.

Callable delegated reads use a dedicated versioned linked-core allowlist, as existing delegated readers do. Do not tunnel a client-selected command to the operator `fleet.snapshot` gateway. The receiving core serves its own homes only and revalidates scopes and ships. Use configured routes only, no recursive remote search, desk move, route creation or reverse spool. Old peers report `unsupported` rather than retrying as operator.

Use 2.4c's `coverage.scope`, `complete`, `sources`, observation/contact timestamps, source states, `other_machines`, and retry information. Add a runtime domain kind (for example `runtime-domain`) rather than falsely asserting a `desk-domain` mail barrier. Owner generation/incarnation is required on desk-mail responses; do not fabricate a desk owner for worker observations. Runtime `domain`/manifest denotes the operator-selected configured machines/ships, filtered by the current grant. A selector narrows the requested observation, never expands authorization or the domain. Missing or unprovable membership means incomplete, not a silently complete local fleet.

Report coverage **per machine, per ship and per facet** (`inventory`, `workers`, `sessions`, `jobs`, `work`, `capacity`, `needs`, `mail`). Local home failure or truncated jobs must not invalidate known inventory, nor make missing activity look idle. New result fields distinguish coverage from paging (`more`/continuation), truncation and action outcome. Explicitly label totals `local` until a runtime domain is configured; ship/machine-specific queries name their selected scope, not the entire fleet.

### 2.2 Provenance and freshness mapping

Retain native `via`, `as_of_ms`, `changed_ms`, link direction and read gaps internally; expose only allowed provenance and safe error codes. Promote host-measured receipt/contact age into the scoped projection: native machine rows do not currently export `received_ms`. `changed_ms` means content change, **not** last contact. Never use a remote clock or a recent HTTP response to renew stale observations.

| Evidence | Result coverage / rendering |
| --- | --- |
| Local bounded home/ledger/host read succeeds | `covered`, mode `local_live`, observation time; facet-specific gaps still apply. |
| Callable peer answers this read | `covered`, mode `remote_live`, `barrier_mode:on_demand`, local receipt time and source observation time. This is a vector of bounded observations, not one simultaneous fleet instant. |
| Native `via:reused` within five seconds | Explicit `reused`, original contact/observation time and age. Can satisfy the declared runtime freshness policy, never described as queried anew. |
| Native `via:collect` on a reverse-only link | `cached`, mode `carried`, last receipt/age; route remains `reverse_only` for on-demand queries. Native `reachable:true` for a recently carried section does **not** mean remote-live. No on-demand inspection/action path is implied. |
| Previous section after failed call | `cached`, `stale` reason when expired; last successful contact/age, latest safe failure/retry. Current activity unknown; last-known values remain visibly dated. |
| Directory only, no worker observation | Inventory cached, runtime facets unknown/unavailable; no activity zero, no fabricated last-live timestamp. |
| Timeout, unreadable home, missing ledger/host socket | `unavailable`/`unreadable`, safe reason and last success if any; preserve separately dated cache. No false idle or free slots. |
| Reverse-only or older peer | `reverse_only`/`unsupported`; partial/unavailable with bounded retry/remediation. No operator fallback. |
| Denied scope/ship | Nonrevealing refusal, internally `permission_denied`. Only already authorized source metadata may carry an explicit denied state; otherwise omit names/counts and use an unexplained incomplete/unknown-domain marker. |

For the first runtime release, only local/direct/reused observations within the declared bound count toward **current** totals; carried observations remain dated partial facts. This is deliberately more conservative than native `reachable`. Reuse 2.4c's `barrier_mode`, `received_at`, `age_ms`, `max_source_age_ms` for any later bounded-contact completeness. Its approved 15-minute push policy applies to reconciled **mail source barriers**, not automatically to job/worker liveness: existing snapshot carry has no such barrier proof. Do not relax runtime freshness from a plugin argument. Restart/clock discontinuity makes contact age unknown until continuity is proved or a new contact succeeds.

### 2.3 Workers, sessions, jobs and tasks are different

- `working` in new-version results is `true | false | "unknown"`, with reason and observation reference. True requires fresh affirmative execution evidence (identity-checked worker payload with an executing job, or an identity-checked hosted/session working-turn observation). A newly read stored `running`/`launching` record alone proves recorded job state, not current worker liveness; keep `recorded_running_jobs` distinct until the census reader supplies that evidence. False requires complete fresh activity evidence for that ship; unknown hosted adapter state, unverified job liveness, stale section or unreadable home precludes false. A positive observation may establish true while other facets remain partial. Keep `last_known_working` separate from current unknown. Do not silently reinterpret the legacy boolean for older clients.
- Count active workers by deduplicated execution identities, not task rows, pane titles or mail volume. First-mate headless wake and its job count once. A hosted first mate with a working turn counts once; a live idle/asleep session is not an active worker. Crew/errand workers are identified by jobs and linked sessions; command subprocesses have their own job count, not agent-worker count. A parked coordinator is waiting, not actively executing; its running child is separate execution. Uncertain/orphaned liveness remains an explicit uncertain bucket, never a free slot or assumed stopped payload.
- Return separate first-mate activity, crew/errand activity, active-worker counts, session counts (`live`, `idle`, `asleep`, `blocked`, `viewer`, `unknown` as applicable), and job-state counts. Session/worker categories are not additive unless explicitly disjoint. A viewer is not another worker. Deduplicate job/session observations by core/home incarnation, job incarnation and checked host binding; duplicate first-mate spaces remain anomalies rather than silently becoming two authorized first mates. Raw binding/socket/session payloads stay internal.
- Preserve native job states: `starting`, `queued`, `launching`, `running`, `parked`, `orphaned`; terminal `succeeded`, `failed`, `cancelled`, `timed_out`, `resumed`. Display “completed” as an explicitly defined terminal grouping, with success/failure/cancel/timeout/resumed breakdown; `resumed` is lineage, not another successful worker. “Blocked” and “waiting” are **wait dimensions**, not invented stored job states. Include queued capacity/disk wait, parked child wait, hosted input block and scheduled dependency/request/time/remote/attention/manual waits with their distinct origins.
- `ship_work` returns task/schedule state, owner, task ID, reason code, wait kind/ref, revision, created/last-checked/next-check times and custody/claim metadata safe for the caller. A claimed/offered task is not necessarily an executing worker. **Open tasks are backlog, never active workers.** A ship with 40 open tasks, an idle first-mate session and no running crew has zero observed active workers, if activity coverage is complete.
- Ages come from the appropriate timestamp: queue age from creation, execution age from spawn, hold age from reservation admission, progress age from last report, completion age from finish, scheduling wait age only from recorded evidence. Return timestamps and age basis; unknown transition time stays null, not `created_ms` relabelled as an exact state age. Source-clock durations are computed at that source observation; host receipt age dates the observation. Keep computed ages outside native section hashing so quiet polls do not manufacture content changes.
- Aggregates name **requested, queried, reused, cached and omitted** allowed machines and counts' contributing sources. Emit `observed_totals` plus per-metric completeness/unknown/truncated markers; an incomplete zero is not a fleet zero. Never count only the first 20 jobs or a page and call it a full state total. Native `jobs_total` is all nonterminal jobs, not a state breakdown. Add bounded authoritative state aggregates or mark counts partial. A complete page is not a complete domain or drained history.

### 2.4 Proposed read tools and safe inspection

All names/arguments below use fixed schemas, not arbitrary RPC forwarding. V1–V4 delivery is recorded in §5; remaining rows are proposed. Ship/machine selectors resolve through allowed core handles, never filesystem paths or client-supplied endpoints. Pages default to 25, max 50, like existing MCP reads, with explicit byte/scan/time/fan-out budgets and grant/filter-bound expiring continuations. Handle exact IDs, not cross-fleet probing by guessed job ID.

| Tool | Input and output | Scope / writes |
| --- | --- | --- |
| `fleet_ships`, `fleet_status` vNext | Optional allowed machine/ship selectors; inventory, per-facet coverage, separate activity/job/task summaries, named aggregate sources | `fleet:read`; no wake, ack, presence change or reconciliation. Old local mode labelled local. |
| `ship_runtime` | Ship and optional machine handle; first-mate harness/host-mode identity (no options), observed activity/session metadata, crew/errand summaries and safe blocker refs | `fleet:read`; optional authorized text additionally `mail:read`; no pane content. |
| `ship_jobs` | Ship, state/kind filters, limit/continuation; safe live and retained terminal rows, per-state counts/coverage | `fleet:read`; no log paths or job reconciliation. Retention gaps explicit. |
| `job_inspect` | Ship + job reference bound to origin/incarnation; owner ship, task, worker/harness, state/wait, times, exit classification, last progress/error and safe related IDs | `fleet:read`; progress body/park-next text additionally `mail:read`. No raw job record. |
| `ship_work` | Ship, limit/continuation; scheduled and unscheduled tasks, states/waits/backlog with separate totals | `fleet:read`; free-form mail-derived reasons additionally `mail:read`; read never enqueue/defer/claim. |
| `machine_capacity` | Allowed machine, optionally ship/task for effective policy; configured/effective limits, reservations, headroom, queue/blockers with coverage | `fleet:read`, subject to shared-host restrictions in §3; no configure/recover. |
| `runtime_needs` | Selected allowed scope, limit/continuation; typed blocker/ask metadata and IDs; explicit unavailable/unknown | `fleet:read`; request question/choices/body additionally `mail:read`. Reading conveys no action authority. Aggregate mail-backed coverage stays partial until 2.4c proves it. |

`job_inspect` joins current task custody, owning home, recorded worker actor/harness, parent/retry/resume lineage and latest progress/error. If custody or incarnation changed, return fresh safe references or stale/refused, never inspect a different job at a reused reference. `doing.text` and park `next` are arbitrary authored text, not inherently sanitized by their short length. Reuse existing text masking where applicable, strip control/terminal escapes, bound text and explicitly flag withheld/unknown fields. Default to typed error category and safe ID when text safety cannot be established. `mail_show`, authorized `get_report` or later safe file handles provide detail; references do not themselves authorize those reads.

No raw argv, environment, cwd, absolute paths, OS hostname/user/socket identifiers, process handles, plugin bindings, private endpoints, credentials, raw logs, pane screen or transcripts. Allowed configured machine labels or opaque source handles are intentional, not raw OS identity disclosure. `wake.transcript` already masks normalized log text, but documents known masking gaps (bare secrets and some encodings) and lacks hosted first-mate transcripts. It must not be reused as a no-secrets guarantee or enabled under `fleet:read`. Any later explicit transcript surface needs separate review, existing masked normalization, stricter retention/limits and negative canary tests; no second raw-log path.

## 3. Capacity and authorization boundaries

Capacity is not one “number of agents allowed.” Native host `max_heavy_jobs` is optional (default no numeric cap); effective admission also depends on the task charter's heavy share, CPU/load with ramp/released-load accounting, memory floor/estimates, disk floor/demand on the target filesystem, queue fairness and provider-route holds. Crew memory comes from route, host harness override, manifest or default; light/memory-reserving jobs can occupy resources without consuming heavy slots.

Return configured host heavy cap, resolved defaults/source, applicable charter heavy cap and observed occupancy separately. For a selected task, effective heavy headroom is the minimum remaining host and charter share when finite, labelled **heavy-slot headroom only**. Null cap means unbounded by that particular policy, not zero, unknown or infinite runnable agents. `occupied_heavy_slots`, numeric `free_heavy_slots` (when finite/known), all reservation counts, memory/CPU/disk readings and queue depth are different fields. Holding resources is not synonymous with executing a worker. Include uncertain/stale holds conservatively; only the existing identity-checked recovery path may release them.

There is no universal native `free_agent_slots` value. If a demand-specific admission explanation is added, it must reuse `admission.rs` including filesystem/charter/parent rules and identify its observation/demand. It is advisory, never a reservation or promise that a later launch will fit. Do not reimplement admission arithmetic in Python/voice, infer memory slots by dividing free memory, or count open tasks as slot occupancy. Native queue `blocked` is a last head-of-queue resource classification, not a continuously fresh diagnosis for every waiter; preserve timestamp/unknown evidence and do not guess typed blockers solely from prose.

Security rules apply to every summary, detail, count, continuation, cache and action:

1. **Scopes are upper bounds.** `fleet:read` covers authorized metadata; `mail:read` covers authorized mail/progress/question text and existing reports. `ship:message` assigns work by normal scoped mail, not arbitrary execution. `work:manage` covers only enumerated work mutations. `fleet:admin` covers enumerated shared policy/admin operations. `captain:presence`, `approval:answer`, `file:transfer` keep their current independent requirements. No scope expansion, new preset entitlement or Full Captain wildcard is implicit in this design.
2. **Restrict before aggregation.** Resolve grant/token at admission in the core, reject reserved provenance/owner fields, and apply ship restrictions to home owner, task owner, worker, related mail participants, wait dependencies, blocker holders and all references. A restricted ship can know it waits on unavailable capacity without learning a hidden ship/job ID, title or count. Filter before counting; do not subtract visible from global to reveal hidden work. Denied guessed targets are nonrevealing; precise reasons stay in bounded redacted audit.
3. **Shared host data is special.** A named-ship `fleet:read` grant does not automatically authorize the machine's complete queue/reservation census, host policy or route/account inventory. Return selected-ship occupancy/queue facts and an unattributed capacity-blocked reason; shared totals/limits stay withheld unless the grant is an all-ships grant (no ship restriction) for that configured machine/domain. A named-ship list never qualifies, even when it happens to cover every ship on the machine today, so adding, removing or moving ships never widens what a grant sees. No new machine grant is inferred from a selector. Machine-wide resource writes need `fleet:admin` and authority over every affected ship; a partial grant cannot tune shared limits via an allowed ship.
4. **Fresh delegated admission.** Recheck effective scopes/ships on every page, detail, action and release to adapter. Bind continuations/cache views to grant/resource, visibility revision, operation/filter, source incarnation and snapshot; restrict/revoke invalidates them. Host/receiver use a request-bound delegated context, no bearer token to peers. Revocation before admission denies; an admitted remote operation may already have observed/changed state before revocation, so host release recheck can discard a result but cannot undo the operation. Never retry denied/unsupported calls as operator.
5. **Keep existing trust limits honest.** Configured linked cores share operator trust, as 2.4c records; this is not cryptographic isolation from a malicious linked operator. Initial same-user trusted plugins are not an OS sandbox either. The external grant boundary must still hold for direct/guessed v2 calls, every alias and gateway entry, not just tool discovery or model instructions.
6. **Voice is a renderer, not authority.** Speak coverage omissions and observation ages before a partial all-clear/count. “Two observed crews on alpha and beta; gamma could not be checked” is valid; “two crews fleet-wide” is not. The bundled [live-voice](../../plugins/live-voice/rules.py) currently allows send/ships/status/waiting, requires utterance read-back/confirmation and refuses approvals. Preserve those safeguards; adopting parity reads or writes requires scoped v2 and independent client/device grant migration. A voice-model tool call, echoed hash or spoken yes cannot create captain authentication, widen a grant or satisfy sensitive trusted confirmation.

## 4. Action parity ledger (not a new orchestration path)

Native parity includes authorized actions, not just counts. These rows track missing exposure without claiming a permanent MCP-only ceiling. Each proposed mutation requires its own exhaustive core scope/argument/target mapping, idempotency/result IDs, charter/custody checks and tests before appearing in discovery. A received request or cancellation acknowledgment is not completed work or proof a payload stopped.

| Outcome / native path | MCP mapping and authority | Current gap / required preservation |
| --- | --- | --- |
| Ask a first mate to act; answer ordinary ask | Existing `send_message`, `ask_liaison`, `reply`; `ship:message`, approval-bearing replies also `approval:answer` | Already exposed; do not introduce a parallel worker-launch loop. Preserve delegated provenance through liaison/linked routes; no arbitrary payload or presence side effects. |
| Rank, enqueue or defer an owned task | Proposed `work_rank`, `work_enqueue`, `work_defer`; `work:manage` | Missing. Preserve one task owner, offer/revision checks, stopped ancestor rules, wait validation/cycle detection, current claim and no-live/uncertain-job requirements. Scheduling lifecycle `work.poll/started/settled/release` stays a worker/runtime protocol, not externally forged claims. |
| Cancel an exact job | Proposed `job_cancel`; `work:manage`, ship/task/job-incarnation checks | Missing. Use existing cancellation/process-identity semantics. Report cancel requested, stopped or outcome unknown accurately; surviving descendants retain custody/capacity. No raw PID/group kill endpoint. |
| Retry a stopped unsuccessful job | Proposed `job_retry`; `work:manage` plus native recovery authority | Missing. Only native failed/cancelled/timed-out replay; no live/orphan retry, no changing stored argv, route or policy, and no bypass of provider backoff, workspace, charter, admission or deployment gates. |
| Start crew/command, review, recovery or other native work | Normal scoped assignment first; any later explicit native action tool needs its individual `work:manage`/policy mapping | Direct native start parity remains tracked/deferred, not silently granted by messaging. No generic `job.start {command:[...]}` or operator dispatch in this visibility slice. Exact launch provenance and sensitive-action classification need a separate design before exposure. |
| Configure host resources; inspect/recover a hold | Proposed `resource_configure`, `resource_recover`; `fleet:admin`, all affected targets and existing process checks | Missing. Settings changes/recovery must not be disguised as reads, force-free live resources or become available to a restricted grant via an allowed home. |
| Permissions, stow/resume, upgrade | Existing scoped core classifications; future fixed admin tools under `fleet:admin` (reads as currently classified) | Absent from current MCP tool map. Preserve grant-bound plans, exact target sets, custody and confirmation. This design does not enable them by adding a read tool. |
| Inspect/type into native session | Metadata only now; raw `session.read/send` **deferred** for explicit security design | Screens can leak secrets; arbitrary typing/keys can bypass scoped mail, approval and shell controls. Not reachable through read/work tools. Future Full Captain parity needs reviewed content/interaction authority, not wrapping the current operator path. |
| Production/privileged approvals and execution; grant/route/desk administration | [Front-door §4](mcp-front-door.md#4-sensitive-approvals-new-runtime-work) and existing individual admin mappings | No client-only approval, `privileged.run`, arbitrary machine gateway, grant creation/expansion, route change or desk-placement/handoff access. Full Captain still needs mandatory exact-artifact/environment trusted confirmation and existing gates. Internal replication/handoff verbs never become user tools. |

## 5. Small PR sequence and evidence

V1–V4's implementation and evidence are recorded below; V5–V8 are a plan, not delivered behavior. Keep runtime/native consumers compatible; negotiate/version additions and retain honest local mode until each capability works. Each PR must update the ledger with command/tool, scopes, hidden writes, coverage, targets and exact tests; split a row further rather than bundle unrelated security changes.

| PR | Deliverable and dependency | Tests required before claiming it |
| --- | --- | --- |
| V1 — Coverage and safe projection | Runtime coverage schema, local/directory unknown handling, safe metadata DTO, tri-state vNext migration and filtered machine/ship selectors. Can start before 2.4c PR 2a/2b/3; share/reuse **2.4c PR 1** coverage/identity types, or land that foundation first, not a competing schema. | Local active/idle/unknown, home/ledger/service errors never false, legacy compatibility, restricted topology/counts/canary redaction, unsupported versions, no implicit work/presence/mail writes (bounded cache bookkeeping explicit); Python/schema/real supervised adapter tests. |
| V2 — Linked runtime status | Implemented: scoped snapshot readers plus `fleet.runtime.v1`, merge/cache/reuse/carry provenance and per-facet totals. Existing opt-in ships/status schema and grants; bounded process-memory cache only, stored operator sections remain dated fallback. No mail-complete claim. | Real gateway subprocesses over one-way local transport; callable/reverse-only, reuse/timeout/cache, no recursion/operator fallback, remote clock skew and restart, grant changes before admission/release, hidden waits/topology, truncated state counts. |
| V3 — Worker/session census | Implemented: shared closed optional census on V1/V2 scoped snapshot DTOs, identity-checked metadata-only host/payload reader, bounded deduplication and fresh activity/worker/session counts. Stored running and live supervisors alone remain unverified. Legacy semantics unchanged; old peers and native operator caches without census remain unknown. Independent of desk bootstrap. | Idle hosted pane vs busy turn, headless wake/job counted once, crew/viewer/command distinctions, sleeping/lapsed lease, duplicate spaces, reused pane/socket/session identity, host/process namespace unavailable, stale stored-running job, race at job finish; no screens or pane rearrangement from reads. |
| V4 — Job/work inspection | Implemented: `ship_jobs`, `job_inspect`, `ship_work`, retained terminal history and paging/count readers, safe owner/task/lineage/progress/error DTO. Can precede 2.4c 2a/2b/3; mail-derived text still scope-filtered, not a mail subscription. | Exact owner/incarnation restrictions, live/terminal/resumed states, scheduled vs unscheduled tasks, 40 open tasks with zero workers, queue/park ages, null state-transition age, concurrent finish/page/restrict, byte/scan limits, retained-history gaps and secret/control canaries; native CLI parity assertions. |
| V5 — Capacity and blockers | `machine_capacity`, safe typed `runtime_needs` metadata, effective host/charter policy provenance and headroom, reuse admission readers only. Independent of desk bootstrap; aggregate **mail** needs remain incomplete. | Finite/unbounded/unknown limits, charter vs host occupancy, shared ledger counted once, CPU/ramp/memory/disk on distinct filesystems, light memory holds, stale/escaped payload reservations, queue fairness/last-blocker freshness, restricted-host inference protection and nonmutating reads. |
| V6a, V6b, V6c — Guarded actions | Separate small PRs for rank/enqueue/defer; exact cancel/retry; shared resource admin. Use V4/V5 references and existing grants. Explicit native launch/session interaction and other admin remain separate tracked follow-ups, not packed here. | Exhaustive scope/argument/target matrix including raw v2 bypass attempts, two unequal grants, replay/restart/idempotency, custody/offer races, no live/orphan retry or route alteration, denied approval/liaison escalation, real descendant cancellation boundary, admission and mandatory production gates intact. |
| V7 — Mail integration and voice migration | After **2.4c PR 1** schema, **PR 2a** explicit agreed generation-1 placement, **PR 2b** source intake/barriers and **PR 3** desk-owned readers, wire aggregate mail/waiting coverage alongside runtime facets. Adapter exposure also waits for applicable 2.4c migration/exposure gates; PR 3 alone is not finished 2.4c. Separately migrate voice to individual scoped v2 grants and the same typed reads, preserving read-back/approval refusal. | Remote-only captain mail visible only after proved intake/read barrier, no deletion of reverse link rows to fake desk ownership, metadata vs body scopes, revoked/restricted voice client, spoken partial/unknown wording, fake-provider supervised adapter tests; reuse 2.4c ack/restart/stale-generation suites, not a new cursor system. |
| V8 — End-to-end acceptance, then real ChatGPT | Close only tested ledger rows. Run the multi-machine test below, then the captain-run ChatGPT text **and voice** check last. No production deployment is authorized by this plan. | Exact fixture IDs/counts/times/coverage and denied calls; record build/protocol/client versions and failures. Real account evidence is separate from SDK/fake-provider tests. |

### V1 delivered contract and evidence

Scoped v2 `ships` and `status` (MCP `fleet_ships`/`fleet_status`) accept exactly
`{schema:"runtime.v1", machine?:HANDLE, ship?:HANDLE}`. Empty arguments preserve
legacy output, including legacy status's boolean interpretation; selectors require
explicit opt-in. Unsupported versions/extra fields fail core admission, including
raw v2 calls. `local` is the answering core handle (never an OS hostname); linked
machine labels resolve solely from allowed directory ships. The label `local` is
reserved: conflicting linked directory records are omitted and inventory is marked
incomplete. Unknown and denied selectors have the same refusal on a complete scan;
a failed/incomplete inventory cannot establish absence and returns an incomplete
projection for an allowed selector instead. No endpoints, home paths, operator snapshot or
job records can be selected or forwarded. Trusted v1 is not migrated. Without
opt-in, `ships` still omits missing homes, computes its recipient from existing
homes (as routing does), defaults a missing/unreadable scope to `""` and
attendance/service read errors to `false`. Legacy store-open errors, including
rejection of symlink ancestors, remain request failures rather than fabricated
null-valued rows. Only runtime.v1 uses non-initializing observation and nullable
unknown facts.

The closed DTO contains ship `name`, `machine`, nullable `attended`, `dispatcher`,
`recorded_running_jobs`, `open_tasks`, tri-state `working`, `working_reason`,
`observed_at`, and `activity_evidence` (`no_nonterminal_jobs`,
`no_launching_turns`, `no_sessions_or_leases`, `ledger_clear`,
`dispatcher_running`, `observed_ms`).
V1 never emits `working:true`: even stored running light jobs and hosted/asleep
leases are `unknown`, not positive execution evidence. False requires fresh local
negative evidence from *all five* checks: no nonterminal jobs for the ship, no
offered/running wakes or running errands (these claim custody before a job exists),
no lease/session/pane rows (including hosted first-mate panes; even expired rows
are conservative), a readable host ledger with no selected-ship reservation/queue
entry, and an observed running dispatcher.
Missing, failed or budgeted-out reads are unknown. Unknown custody fails closed.
Open tasks are backlog, not activity: forty open tasks can accompany false.
Reads are bounded observations across home, ledger and service, not one atomic
instant or a worker census. V3 must supply positive activity evidence.

Coverage extends the existing `coverage.v1` `Source` types with `runtime-domain`,
without desk ownership/mail barriers. Each allowed machine/ship has sources for
inventory, workers, sessions, jobs, work, capacity, needs and mail: state, mode,
observation/receipt time, age and fixed safe reason codes. Unimplemented facets
are unsupported/unavailable, never zero. Workers remain `unavailable` even with
`fresh_local_negative_evidence`: local negatives are not a worker census.
Directory-only facts are undated cached
inventory, never live observations. `domain:null`, `state:"unknown_domain"` and
`complete:false` avoid claiming configured fleet membership. `inventory_complete`
is independent of runtime completeness; `observed_totals` labels local/selected
recorded-running counts and their completeness. Active-worker totals remain null.
Metadata contains no authored progress, ask/mail body, scope prose, argv, cwd,
ledger/process/binding fields or operator payloads, even with `mail:read`.

V1 opens existing homes, registry and ledgers read-only (no migrations or repairs),
with at most 200 returned ships, 2000 scanned directory entries, a three-second
local admission budget and existing 250 ms SQLite observation budgets/25 ms waits.
These are cooperative VM/checkpoint limits, not hard filesystem I/O deadlines.
Selectors/filtering precede visible count limits. `writes:"none"` denotes runtime
observation: no work, presence, mail or snapshot-cache writes. Existing grant/token
admission/audit bookkeeping remains in the registry. Native operator snapshot
polling's bounded `fleet_snapshots` cache/`changed_ms` writes are unchanged and are
not invoked by this projection. V2 will separately document its cache writes.

Evidence fixtures: `door::runtime::tests` covers tri-state serialization/evidence,
strict versions/handles, runtime coverage validation/redaction, partial directory
failure/selector uncertainty, reserved local labels and non-initializing missing-home
reads. `tests/fixtures/door_runtime.rs` exercises the real supervisor: forty-task
idle, offered/running wakes and running errands with no jobs, hosted pane without
a lease, hosted lease, stored running light job, ledger held/missing/unreadable,
bad service lock and missing home, legacy shape/defaults/recipient/symlink homes
in both protocols, unavailable worker coverage, filtered topology/counts/canaries,
same denied/unknown selectors, and no
home or snapshot-cache writes. `tests/mcp_door_test.py` covers Python schemas,
coverage rendering repair and the real HTTP SDK/supervised adapter in both eras.
V1 does not claim linked reads, an activity census, job/work tools, capacity,
actions, mail integration, real account/voice acceptance or deployment.

### V2 delivered contract and evidence

The same opt-in `ships`/`status` arguments now request linked **recorded** runtime
metadata, not worker liveness. `fleet.runtime.v1` is a dedicated operator-link-
authenticated gateway operation with exactly `{context,args}`. The immutable
request-bound delegated context must contain `fleet:read`; args use the existing
`runtime.v1` schema and optional ship selector, never a machine selector, command,
path or endpoint. The receiver reads only permitted local homes. It never asks
its links, calls operator `fleet.snapshot`, or retries an unsupported/denied read
with operator authority. No scopes or provider routes are changed.

`src/snapshot/scoped.rs` defines closed `RuntimeSection`, `RuntimeShip` and
`RuntimeJob` DTOs, sharing the native snapshot reader, job-kind joins and caps.
Scoped jobs are attributed through current task ownership **before** counting/
limiting. The operator snapshot preserves its historical all-home job view,
including missing-custody jobs; additive `owned_jobs_total` and per-job `owner`
fields let stored sections prove attribution without exposing home-global counts.
Scoped reader output also marks `jobs_owner`. Older operator sections without
ownership evidence cannot supply job IDs or counts. Metadata includes stored presence,
open-task/nonterminal-job totals, up to twenty job IDs/task IDs/kinds/states and
wait kind (`job`, `capacity`, `disk`) without dependency/holder references.
Expanded-state totals are partial when truncated; `jobs_total` is not a state
breakdown. Shared capacity, routes, task titles, progress/park text, mail,
commands, process/binding fields and arbitrary record fragments are withheld,
including under `mail:read`: this slice does not yet expose authored text.

The host resolves allowed directory ships first, queries at most eight callable
machines concurrently with the existing ten-second snapshot transport deadline,
and filters answers again against its own admitted context and selected directory
names. Omitted allowed machines remain directory-only/unknown. A second
credential resolution immediately before release discards the result if scopes,
ships, grant identity or credential validity changed while waiting (including
OAuth expiry/revocation); it rolls back its bookkeeping to preserve one request
audit. This cannot undo an already admitted remote observation. Grant changes
also partition cached views; cache authorization is not durable release permission.

Each ship's `runtime` object carries closed `facts`, `current`, source observation
time, host receipt time, age and mode. `remote_live` dates this call;
`reused` retains the original receipt and uses monotonic contact age within the
fixed five-second bound. A sixteen-entry process-memory cache is keyed by
registry, machine, selector and admitted authority. Timeout/unsupported responses
may preserve last successful facts as `cache`, never current. Stored native
`fleet_snapshots` sections are read-only fallback; `via:collect` is `carried`,
even on a callable link. Persistent receipt times remain dated facts, with null
age because restart/clock continuity is unproved; restart never promotes them to
current. A new direct contact re-establishes continuity. Remote clock timestamps
never determine host contact age. Reverse-only machines are never called.

Coverage reuses `coverage.v1` runtime-domain sources and adds remote/reused/cache/
carried modes, independent observation/receipt timestamps, on-demand barrier mode
and the fixed freshness bound. Unknown runtime membership still means
`domain:null`, `complete:false`; these are observations of allowed configured
sources, **not** a configured complete fleet manifest. Requested, queried,
reused, cached and fanout-omitted machine lists are explicit. Current nonterminal
job and expanded-state totals name contributing sources and completeness;
carried/cached facts contribute neither current counts nor completeness.
Worker/session/work/capacity/needs/mail totals remain unknown; stored presence is
not a session census, stored running is not execution, and runtime success never
proves mail coverage. V1's local negative-evidence and legacy semantics remain.

`writes:"none"` still describes runtime/home observation. The separate
`cache_writes:"bounded_process_memory_only"` records the only observation-cache
mutation. No scoped read stores native operator payloads, initializes/migrates
homes, arranges panes, reconciles jobs, changes presence, releases reservations
or acknowledges mail. Existing machine gateway/credential registry schema and
auth/audit bookkeeping remains; explicit operator snapshot/collect cache writes
are unchanged. No durable delegated cache is added; after host restart only
separately stored operator/carried sections can provide fallback facts.

Evidence: `snapshot::scoped::tests` checks closed operator projection, hidden
wait references/text and unattributed/truncated counts. The real supervised
`runtime_linked_gateway_reuse_timeout_carry_restart_and_grant_boundaries` fixture
uses one-way local transport and actual gateway subprocesses for callable vs
reverse-only, unchanged-contact reuse, timeout/cache, no recursion/operator
fallback, remote clock skew, host restart, receiving scopes/ships, host grant
restriction before admission/release, hidden topology/ownership, stored operator
filtering, truncated state totals and dated collect carry. V1 fixtures continue
to test local tri-state/legacy compatibility and nonmutating homes. Full gate
results are recorded in the task report. The initial gate passed fmt, strict
Clippy and both targeted lib suites, then found a fixture home-invocation error.
The next gate passed the complete door suite, then caught four native snapshot
compatibility regressions in the owner filter. The reader now preserves the
operator all-home view and adds separate ownership evidence for scoped filtering.
The subsequent gate passed fmt, strict all-target/all-feature Clippy, 67 `door::`
lib tests, 10 snapshot lib tests, 68 door integration tests, 52 fleet tests,
125 machine tests, the bundled-plugin bytecode guard and the locked build. It
stopped at MCP bootstrap because the example venv already existed; Python tests
were not reached in that run. The rerun uses a fresh task-private venv.

This slice does **not** deliver V3 worker/session liveness, V4 history/detail/
continuations, V5 capacity/needs, work actions, mail integration, a configured
runtime-domain manifest, real-account/voice acceptance or deployment.

### V3 delivered contract and evidence

The existing `runtime.v1` opt-in and dedicated `fleet.runtime.v1` gateway now
carry an optional, closed `census` on each ship's runtime facts. Older sections
without it still deserialize and cannot establish workers or sessions. The
native operator snapshot does not acquire host I/O or change its historic job
view; its stored sections remain dated fallback without a census. There are no
new tools, selectors, scopes, provider routes or work actions. The gateway
extension is explicitly requested with `census:1` beside `{context,args}`; without
it a V3 receiver returns the V2 shape (omitting `census`, not serializing null).
An exact old-envelope `unsupported_arguments` refusal permits one retry of the
same scoped metadata read without the extension. No other refusal causes that
retry, and neither request uses operator snapshot authority. Peers predating the
gateway still require upgrade only on its exact unknown-command refusal.

`working:true` requires an affirmative checked execution observation: either a
non-hosted running/launching job's recorded payload leader is alive with the
same kernel start/boot/PID-namespace identity and a recorded job incarnation,
or a live lease/crew binding matches a host snapshot's socket inode, pane,
terminal, workspace, tab, agent and explicit adapter-session identity and that
adapter says `working`. A live supervisor is never evidence. Interactive hosted
jobs require a checked turn, not merely a live interactive payload. A missing
payload leader is uncertain, not proof surviving descendants stopped; foreign
or inaccessible process namespaces cannot contribute positive evidence.

Counts distinguish `first_mate`, `crew`, `errand`, command payloads,
`uncertain_jobs` and `active_workers` (first-mate + crew + errand, excluding
commands). Wake/job rows are joined rather than counted independently; checked
payloads and checked session endpoints are deduplicated within each owning ship
observation. Session counts distinguish live, idle, asleep, blocked, viewer and
unknown. `live` overlaps its state categories; these are not additive workers.
Viewer panes never add execution. Queued and parked jobs are waiting. A sleeping
lease contradicting a busy turn is unknown; a lapsed lease cannot authorize a
turn observation. Duplicate mate spaces are bounded anomalies, not a second
authorized first mate. Unknown host/adapter/session identity precludes a complete
negative observation. Positive observations can coexist with incomplete facets.

The census scans at most 200 selected owned jobs and 200 crew-pane bindings,
with explicit truncation and uncertain buckets; it does not reuse the expanded
twenty-job list as a total. Each ship observation has a three-second cooperative
budget, per-database 250 ms observation budgets, and the existing bounded Herdr
protocol (800 ms request deadlines, 1 MiB replies, at most 1000 panes/spaces).
A per-observation socket cache avoids repeated host reads. Only metadata methods
`session.snapshot` and `workspace.list` are called: no screens, logs, transcript,
sidebar repair, pane movement, presence change or job reconciliation. Census
input selects identity/state fields, not argv/progress/mail bodies. Two separate
SQLite transactions compare job incarnation/state/payload/custody, lease and pane
bindings around host/process I/O; database inode and home ownership changes also
invalidate the observation. A detected finish/rebind race discards positives,
not just the completeness flag. These are bounded vectors, not simultaneous
fleet instants; work can start/finish immediately after observation.

Complete negative activity additionally requires the existing readable empty
selected-ship ledger and running dispatcher. Missing ledger/service evidence
stays unknown even when the worker/session census reports a complete zero.
Current totals use only local/direct/reused census evidence under V2's fixed
freshness policy; cached/carried census facts are dated, never current, and
cannot turn `working` true. Every metric names contributors and completeness;
an incomplete observed zero is not a fleet zero. Runtime domain/mail completeness
remain independent and unknown as before.

Normal writable home initialization adds nullable `crew_panes.incarnation`;
`record_crew` binds new rows to their job incarnation in its existing transaction.
Old rows/homes missing that evidence remain unverified; scoped reads never run
that migration. No new observation writes are introduced beyond V2's bounded
process-memory cache and existing authorization/audit bookkeeping. Public DTOs
never include raw process identities, binding/socket/session payloads, home paths,
workspace labels, pane titles, screens or authored text.

Evidence fixtures: `snapshot::census::tests` exercises idle/busy/blocked/asleep/
lapsed sessions over actual Unix sockets; pane/socket/session reuse; duplicate
spaces; headless payload deduplication; crew/viewer/command/parked distinctions;
live-supervisor-only and stale-running records; unavailable identities and foreign
namespaces; finish/incarnation/custody races in new SQLite transactions; and a
real payload subprocess before/after exit. Fixtures refuse all host methods
except the two metadata reads and assert canary redaction. The existing real
supervised local fixture now asserts fresh zero-worker/session census coverage
and unchanged home/WAL bytes. The linked real-gateway fixture adds a checked
executing payload outside the twenty-job expansion, fresh/reused counts, cached
activity unknown, exact legacy upgrade refusal versus an unrelated unsupported
schema error, and `section_truncated` for omitted ships. MCP SDK assertions keep
stored-running-only work unknown with an explicit uncertain bucket. Heavy-gate
results are reported separately; no unrun gate is claimed here.

Remaining limitations: adapters without explicit matching session identity stay
unknown, surviving payload descendants without a live leader stay uncertain,
observations cannot prevent subsequent races, and budget/host failures reduce
coverage rather than guessing idle. V4 jobs/work tools, V5 capacity, actions,
mail completeness, real-account/voice acceptance and deployment are not delivered.

### V4 delivered contract and evidence

The fixed scoped commands and MCP tools `ship_jobs`, `job_inspect`, and
`ship_work` require `fleet:read` and a selected `ship`. Lists accept `limit`
(default 25, max 50), `continuation`, and `include_text`; jobs additionally
accept one native `state` (or the explicit terminal grouping `completed`) and
one `kind`. Detail accepts only the exact `reference` returned by a read,
including ship, task, job ID, origin and incarnation, plus `include_text`.
No raw job ID probing, arbitrary RPC/argv, path or endpoint selector is exposed.
A dedicated `fleet.inspection.v1 {context,operation,args}` gateway independently
validates the fixed operation, schema, scope, ship and owning local home. It
never traverses links or falls back to operator snapshot/dispatch. The host
rechecks the actual credential and visibility generation before release.
Unsupported, reverse-only and unavailable linked reads return empty, incomplete
coverage, never an idle assertion. Detail refusals for hidden, stale and guessed
references do not distinguish existence.

`src/snapshot/inspection.rs` shares the existing read-only Store, runtime origin,
observation budget, native snapshot kind/wake joins and terminal vocabulary.
It does not call `public_job`, reconcile jobs, read process logs/screens, open
resource ledgers, repair liveness or invoke the scheduler. Origin handles hash
the persisted runtime origin and database device/inode; replacement invalidates
references and continuations. Ownership and worker attribution precede counting,
and related jobs and dependency IDs are independently restricted. Reads use a
SQLite observation transaction, then recheck custody/references before release.
These are recorded-state vectors, not a simultaneous or fresh liveness census;
V3's separate worker/session census remains the activity authority.

The closed job DTO contains `reference`, `owner`, `worker`, nullable `harness`,
`kind`, native `state`, explicit `completed`, conservative `liveness`, `wait`,
`created`, `spawned`, `finished`, `state_since`, `exit`, `progress`, `error`, and
safe `parent`, `retry_of`, `resume_of`, `relaunch_of`, `resumed_as` references.
`resumed` is lineage, never success. Age objects contain timestamp, age and basis;
queue age uses creation, park age recorded park update, progress age recorded
report time, execution age spawn, terminal age finish. Unknown state-transition
and scheduling wait times stay null. Work rows contain task/owner/task-state,
schedule state, scheduled/backlog flags, revision, structured wait, reason,
created/last-check/next-check, lease expiry, claimed/offered booleans, safe job
reference and allowed handoff ship. Claim/holder, offer, binding, process, socket,
route/account, argv/env/cwd/log and filesystem fields are not serialized.
Task titles/briefs and raw runtime error prose are withheld.

Authored progress, park-next and schedule reason text requires **both** opt-in
`include_text:true` and `mail:read`; metadata-only callers get typed category,
safe ID/timestamp when known, null text and an explicit withheld flag. Terminal
escapes/controls and bidi overrides are removed before existing transcript
masking, path/endpoint-shaped words are withheld, and output is bounded to 512
characters. Runtime error reasons remain category-only even with mail scope.
This is best-effort authored-text masking, not a no-secrets transcript promise;
bare unlabelled secrets can still require withholding/review. Text permission is
not mail subscription/coverage or permission to dereference reports/mail.

Each request scans at most 2000 selected records, expands at most 256 DTOs,
uses a 64 KiB page budget and cooperative three-second/250 ms SQLite budgets.
Counts name `authorized_scan_window` and `counts_complete`; later pages and
budget/invalid-row gaps are partial, never first-page counts relabelled as full
totals. The terminal grouping includes succeeded/failed/cancelled/timed_out/resumed.
Work counts distinguish states, scheduled, unscheduled and backlog; tasks are
never workers. Retained job history explicitly says
`retained_only_prior_history_unknown`: even complete retained counts do not
prove no prior pruned history. Paging uses keyset/high-water snapshots; later
pages re-observe current recorded state/custody under that bound, not a frozen
historical database. New refs name any current incarnation, and stale detail
refs are refused.

Continuations are opaque, grant/resource/filter/visibility-generation/source/
snapshot bound, expire after ten minutes, and are bounded to 32 per grant/128
globally. They are persisted in the **registry**, not homes: a separate gateway
subprocess must be able to serve the next page. Lifecycle triggers delete them
on restrict/revoke and monotonically advance visibility even on restrict/restore
within one clock tick. On Linux, expiry uses boot-time monotonic milliseconds
and a boot-bound key; reboot invalidates tokens without trusting wall clocks.
On macOS, `kern.boottime` from `sysctlbyname` binds the key to the boot and
Darwin's `CLOCK_MONOTONIC_RAW` supplies continuous monotonic milliseconds,
including sleep, across gateway processes. Failure to obtain either value fails
closed; wall-clock synchronization cannot extend expiry. A change to the boot
stamp conservatively invalidates tokens. Other platforms retain the conservative
process-bound fallback (a subsequent linked gateway must restart listing). `writes:none` describes home/
work/presence/mail observation; `cache_writes:bounded_registry_continuations_only`
and ordinary registry authorization/audit/schema bookkeeping are explicit.
No scopes, presets, provider routes, scheduler policies or production deployment
are changed.

Evidence fixtures added: `snapshot::inspection::tests` covers exact owner,
worker and incarnation checks, all native states and CLI-reader parity, queue/
park/progress ages, null transitions, resumed lineage, separate schedules/
backlog, forty open tasks with zero census workers, bounded scan windows and
secret/control/path canaries. `door::inspection::tests` covers strict fixed
schemas, concurrent finish/page and custody moves, page byte limits/no skipped
rows, grant/filter/source/expiry/generation boundaries and bounded token state.
`tests/fixtures/door_inspection.rs` uses a real supervised door and one-way
linked gateway subprocesses for paging/detail/native CLI parity, hidden owners,
receiving scope/ship checks, unsupported peers and restriction before host
release. MCP SDK tests exercise discovery/schema/read-only tools in both API
eras. Gate outcomes are reported separately; fixture addition alone is not a
claim that a gate passed.

The final single heavy gate on `ed3c9e9` passed `cargo fmt --check`, strict
locked all-target/all-feature Clippy, the **full** lib suite (554 tests), door
(69), fleet (52), jobs (63), machines (125), the bundled-plugin bytecode guard
(1 selected test), the locked build, fresh task-private locked MCP bootstrap,
and `tests/mcp_door_test.py` (27 tests, both protocol eras). Builds and tests used
`CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1`. The previously reported local
`move_batches` size-sensitive failure did not reproduce. Python 3.14 emitted
non-fatal unclosed-SQLite resource warnings and session-termination 401 messages;
the MCP suite finished `OK`. Initial validation caught a related-reference
closure lifetime error; the next run caught the session test module placement,
a stale MCP tool-count assertion, and oversized progress expansion exhausting
its observation budget. The final implementation preserves resumable partial
facts at that budget, does not skip an interrupted row, and ends correctly if
retention removes a former high-water tail. The final full run covers those
regressions. Runtime evidence is for `ed3c9e9`; the following documentation-only
commit records it without claiming additional behavior or a deployment.

Also addressed V3 review nits: routine presence expiry renewal no longer
invalidates the census identity vector; a lapsed lease loses only its own turn
observation, preserving unrelated job evidence. Tests use a real metadata-only
host socket and a live headless payload identity. `session::init` first checks
for the incarnation column and takes/rechecks the write lock only for migration;
a held-writer test covers steady-state initialization.

#### V4 round-2 corrections

The OAuth SDK discovery assertion includes the three V4 tools, matching the
existing MCP door tool-set/count assertions (trusted v1 still has twelve tools).
Linux and macOS continuations now share the kernel boot-bound lifetime across
independent gateway processes; the clock subprocess regression runs on both,
without a Linux-only exclusion on the linked paging fixture. Local validation
cannot execute macOS; its build and linked test remain CI evidence.

Inspection precomputes the latest matching-incarnation progress metadata and
`resume_of` reverse map once per selected scan window, instead of scanning the
ship's mail and the job table for each row. Expanded report bodies use primary-key
reads, keeping large authored bodies out of the scan map. A realistic fixture
contains 400 jobs and 8000 mail rows and requires at least a 25-row page within
the 250 ms observation budget; it checks latest-report/incarnation and resumed
lineage selection. Existing byte/scan/time and continuation limits remain.

Receiving text validation checks the length and absence of controls, bidi
formatting, and path/endpoint-shaped words, not masking idempotence: cutting at
512 characters inside already masked output is legitimate. A regression exercises
that cut with a labelled secret. Masking still occurs before truncation, and scope
and opt-in requirements are unchanged. A shared task-status list includes
`pending` for both snapshot production and receiving validation. Park evidence
must match the job's task and still be `parked`; historical/mismatched rows cannot
supply a child or next text. Hidden worker attribution marks coverage partial
before applying state/kind filters, so filter changes cannot probe hidden states.
A lease lapsing during a census now subtracts only its original observed session/
turn contribution: it never performs a second unchecked host/process observation.
The real socket regression asserts exactly one pair of host metadata calls.

Round-2 gate results are reported separately; the corrections and added fixtures
above do not themselves claim a passing run or macOS execution.

Remaining gaps: V5 capacity/needs, actions, aggregate mail, transcripts/screens,
a configured complete fleet manifest, real ChatGPT/voice acceptance and deployment.

### Multi-machine end-to-end acceptance

Use three isolated cores/homes, real gateway subprocesses via local test transport, and the MCP adapter as a real supervised subprocess. Door on `alpha`; `alpha -> beta` callable; `gamma -> alpha` only (alpha cannot initiate a gamma read). No extra route or provider change to make a test pass. Extend [fleet tests](../../tests/fleet.rs), [machine tests](../../tests/machines.rs), the [linked reader fixture](../../tests/fixtures/door_reader_linked.rs), the [linked desk fixture](../../tests/fixtures/door_desk_linked.rs) and [MCP adapter tests](../../tests/mcp_door_test.py); do not replace real transport with a mocked aggregate response.

1. `survey` on alpha has an actively executing first mate/crew and a queued child; `relay` on beta has an idle first-mate session, many open tasks and no active worker; `lookout` on gamma has live work but is reverse-only. Add a parked coordinator, completed/failed jobs and a known resource blocker to exercise each view. Assert exact job IDs and deduplicated worker/session counts, not UI screenshots alone.
2. Before gamma contacts alpha, inventory knows lookout but activity is unknown. Assert alpha local-live, beta remote-live with zero observed workers despite its backlog, gamma reverse-only/directory unknown; aggregates name alpha/beta as observed and are incomplete. After collect carries gamma's section, assert dated carried state, not remote-live or query-complete. Expire it, interrupt beta, lock a home/ledger and restart host/adapter: preserve last success, explicit unknowns and no false zero/free slots.
3. Query the same ship/job/work/capacity through native readers and MCP at pinned observation boundaries. Compare safe fields/state grouping/age basis; check unknown and truncated counts, terminal history and exact ownership. Test two restricted grants, denied guessed IDs and shared-host inference, then restrict/revoke while a linked page is in flight. Verify canary secrets in argv/options/logs/mail/pane data never appear on metadata paths; text tests exercise scoped masking/withholding separately.
4. Put an uncollected **remote-only captain notice and open ask** on beta and separately a source on gamma, with no local copies. Runtime completeness cannot imply mail visibility. Before 2.4c intake, coverage must identify the missing input, never say no asks. After agreed placement, barriers and subscription ingestion, assert exact origin mail IDs appear under `mail:read` (and not under metadata-only grants); exercise duplicate/lost/stale acknowledgments and restart without moving dashboard/other-reader cursors. Reverse-only selected reads remain unavailable even when source intake succeeds on gamma's contact.
5. With explicit action grants in disposable fixtures, rank/defer/retry/cancel permitted work and verify native custody/admission/outcome semantics. Deny forbidden targets, payload/dispatch escapes, approval self-confirmation and host-wide writes from restricted grants. Do not run a production deployment, privileged command or change a provider route.

### Captain-run ChatGPT check (last)

After the above passes, supply the captain a disposable fleet and exact tested artifact/configuration; the captain retains account credentials and deliberately configures the approved test endpoint. Ask in text and voice: “Which first mates and crews are actually working, on which machines?”, “Which jobs are queued, blocked or failed, and for how long?”, “Why can't this job start; which slots are free?”, and “Inspect this job and show its last progress.” Compare spoken and structured results, machine names/omissions/timestamps, and the idle ship with many open tasks. Read remote-only mail, try a restricted action and revoked client, then an allowed nonproduction action with its own grant. A voice caller gets no additional privilege or approval path. Record actual account/client/build/date, protocol and registration behavior; unsupported voice tool access is an outstanding limitation, not a simulated pass. Follow the [front-door real-client checklist](mcp-front-door.md#10-captain-run-real-client-checklist-q5), including cleanup; make no production/autonomy claim from the pilot.

## 6. Short captain questions (recommendations, not implicit approvals)

1. **Runtime carry freshness:** may a recent reverse-only snapshot count toward current totals? **Recommend no initially:** show dated carried facts and incomplete current totals; consider bounded runtime barriers separately rather than borrowing mail's 15-minute policy for worker liveness.
2. **Progress text entitlement:** should new job/needs tools return authored text with only `fleet:read`? **Recommend metadata under `fleet:read`, text additionally under existing `mail:read`**, opt-in/versioned with a migration notice; no automatic grant expansion and no change to legacy waiting's body behavior.
3. **Direct interaction priority after read parity:** prioritize raw session/launch access or guarded work actions? **Recommend guarded rank/enqueue/defer/cancel/retry first**, keep normal assignment via first mates; design explicit native launch/session parity separately because raw argv/keys/screens can bypass grants and leak secrets. These remain tracked gaps, not permanent Full Captain exclusions.
