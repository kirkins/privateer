# MCP and voice: fleet visibility and runtime parity

## Status and recommendation

Follow-up to [#452](https://github.com/kirkins/privateer/issues/452), the captain's field report and follow-up of 2026-10-27. **V1 implements opt-in local runtime metadata and coverage; V2 adds linked recorded runtime status; V3 adds identity-checked worker/session observations; V4 adds selected-ship job/work inspection; V5 adds scoped capacity and typed-needs reads; V6a adds guarded rank/enqueue/defer; V6b adds exact cancellation and V6b3 adds guarded retry. V6c adds tightening-only resource configuration and V6d adds exact reservation recovery using target-terminal-issued single-use permits; V7–V8 remain gaps.** No scope, grant or provider route is expanded. The inventory below distinguishes delivered V1–V5 behavior from the remaining proposed tools. Examples use fictional machines `alpha`, `beta`, `gamma` and ships `survey`, `relay`, `lookout`.

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
| Capacity, holds, host readings and queue | `jobs.rs`, `resource.status`, reservations/queue/crew-memory sources; [admission.rs](../../src/admission.rs), settings/measure/check; [core.rs](../../src/core.rs), charter `max_heavy_jobs` | V5 `machine_capacity`, closed read-only ledger DTO and coverage; host/charter heavy-slot headroom uses admission helpers | `fleet:read`; `machine_capacity` and selected-ship effective-policy projection | One bounded ledger read per answering machine/selected home, never summed once per ship. Named-ship grants see only selected-ship holds/queue with shared limits, readings, totals and routes withheld. Current queued-job/open-task custody is filtered before bounds, not capped by lifetime history. Callable peers use the inspection allowlist; reverse-only/unsupported stays unknown. |
| Provider/route availability | [crew_routes.rs](../../src/crew_routes.rs), `states`; snapshot machine routes; bearings summary | V5 state counts only, under unrestricted all-ships grants; no route/account keys or error prose | `fleet:read`; safe availability in `machine_capacity`/`ship_runtime` | In snapshot across links. Named/account route keys and classified error strings still require visibility/redaction review; reading must not probe or alter routes. |
| Typed captain/resource blockers | [needs.rs](../../src/needs.rs), `runtime.needs`; [runtime-needs.md](../runtime-needs.md); snapshot needs include machine/source/id/waits | V5 selected-ship `runtime_needs`, typed IDs/metadata; question/choices need mail scope and opt-in; legacy `fleet_waiting` unchanged | `fleet:read` for blocker/ask metadata; `mail:read` for ask content; `runtime_needs` | V5 reuses native budgeted typed needs with selected ownership and relevant job states before bounds/paging, via the inspection gateway. Approval liveness stops before foreign/missing parents and remains unknown there. Approval text/argv/cwd and raw actions are withheld; ordinary ask text also requires opt-in and mail scope. Mail-backed coverage remains partial. |
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

All names/arguments below use fixed schemas, not arbitrary RPC forwarding. V1–V5 delivery is recorded in §5; remaining rows are proposed. Ship/machine selectors resolve through allowed core handles, never filesystem paths or client-supplied endpoints. Pages default to 25, max 50, like existing MCP reads, with explicit byte/scan/time/fan-out budgets and grant/filter-bound expiring continuations. Handle exact IDs, not cross-fleet probing by guessed job ID.

| Tool | Input and output | Scope / writes |
| --- | --- | --- |
| `fleet_ships`, `fleet_status` vNext | Optional allowed machine/ship selectors; inventory, per-facet coverage, separate activity/job/task summaries, named aggregate sources | `fleet:read`; no wake, ack, presence change or reconciliation. Old local mode labelled local. |
| `ship_runtime` | Implemented selected-ship `runtime.v1` envelope: required ship, optional machine handle, checked activity/session and crew/errand counts, recorded jobs and coverage. Negotiated anonymous checked session rows and safe harness/host-mode labels; blocker refs remain a follow-up. | `fleet:read`; metadata only, no authored text or pane content. |
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
| Rank, enqueue or defer an owned task | Implemented `work_rank`, `work_enqueue`, `work_defer`; `work:manage` | Exact origin/ship/task/generation refs from `ship_work`; mandatory idempotency keys and stable acknowledgment/result IDs. Calls native rank/enqueue/defer in the same transaction as result/audit. Keeps native charter, ownership, offer, stopped-ancestor, wait/cycle and claim/live-job checks; external defer additionally cannot take a live/uncertain payload's custody. Dedicated `fleet.work-action.v1`, never the read gateway or operator fallback. Scheduling lifecycle `work.poll/started/settled/release` stays internal. See V6a contract/evidence below. |
| Cancel an exact job | Implemented `job_cancel {ship, reference, idempotency_key}`; `work:manage`, exact V4 origin/ship/task/job/incarnation and related-reference visibility | Durable native cancellation intent + metadata audit + uncertain recovery result commit atomically before native cancellation/process signaling. Final result reports `cancel_requested` or native-terminal `stopped`; signal/crash/commit/result uncertainty is `outcome_unknown`. Same-key replay never resends signals. Surviving descendants keep native custody/capacity. Dedicated `fleet.work-action.v1`, no raw PID/group endpoint. See V6b contract/evidence. |
| Retry a stopped unsuccessful job | Implemented `job_retry {ship,reference,idempotency_key}`; `work:manage` plus native recovery and launch authority | Same exact-reference graph visibility as cancellation. Native retry preparation/admission, uncertain result and metadata audit commit before native launch; same-key recovery never launches again. Only native failed/cancelled/timed-out replay; no live/orphan/parked/queued/uncertain retry, argv/cwd/env/route/policy changes, or bypass of provider backoff, workspace, charter, admission or deployment gates. See V6b3 below. |
| Start crew/command, review, recovery or other native work | Normal scoped assignment first; any later explicit native action tool needs its individual `work:manage`/policy mapping | Direct native start parity remains tracked/deferred, not silently granted by messaging. No generic `job.start {command:[...]}` or operator dispatch in this visibility slice. Exact launch provenance and sensitive-action classification need a separate design before exposure. |
| Configure host resources; inspect/recover a hold | `resource_configure` tightens only; `resource_recovery_reference` reads exact terminal-hold metadata under `fleet:read`; `resource_recover` applies a target-terminal-issued permit under unrestricted `fleet:admin` | Configure cannot relax/reset floors/caps or release holds. Recovery is one exact terminal-job reservation, never orphan/nonterminal settlement. Host and receiver refuse every named ship list. Clients never issue permits. See [the typed recovery contract](native-reservation-recovery.md). |
| Permissions, stow/resume, upgrade | Existing scoped core classifications; future fixed admin tools under `fleet:admin` (reads as currently classified) | Absent from current MCP tool map. Preserve grant-bound plans, exact target sets, custody and confirmation. This design does not enable them by adding a read tool. |
| Inspect/type into native session | Metadata only now; raw `session.read/send` **deferred** for explicit security design | Screens can leak secrets; arbitrary typing/keys can bypass scoped mail, approval and shell controls. Not reachable through read/work tools. Future Full Captain parity needs reviewed content/interaction authority, not wrapping the current operator path. |
| Production/privileged approvals and execution; grant/route/desk administration | [Front-door §4](mcp-front-door.md#4-sensitive-approvals-new-runtime-work) and existing individual admin mappings | No client-only approval, `privileged.run`, arbitrary machine gateway, grant creation/expansion, route change or desk-placement/handoff access. Full Captain still needs mandatory exact-artifact/environment trusted confirmation and existing gates. Internal replication/handoff verbs never become user tools. |

## 5. Small PR sequence and contracts

V1–V6a, V6b cancellation/retry, V6c tightening-only configuration and V6d exact permit-bound recovery contracts are recorded below; V7–V8 remain a plan, not delivered behavior. Run-specific artifacts, managed job identities, incarnations, test counts and gate results belong in task reports and managed job output, not this public contract. Keep runtime/native consumers compatible; negotiate/version additions and retain honest local mode until each capability works. Each PR must update the ledger with command/tool, scopes, hidden writes, coverage, targets and exact tests; split a row further rather than bundle unrelated security changes.

| PR | Deliverable and dependency | Tests required before claiming it |
| --- | --- | --- |
| V1 — Coverage and safe projection | Runtime coverage schema, local/directory unknown handling, safe metadata DTO, tri-state vNext migration and filtered machine/ship selectors. Can start before 2.4c PR 2a/2b/3; share/reuse **2.4c PR 1** coverage/identity types, or land that foundation first, not a competing schema. | Local active/idle/unknown, home/ledger/service errors never false, legacy compatibility, restricted topology/counts/canary redaction, unsupported versions, no implicit work/presence/mail writes (bounded cache bookkeeping explicit); Python/schema/real supervised adapter tests. |
| V2 — Linked runtime status | Implemented: scoped snapshot readers plus `fleet.runtime.v1`, merge/cache/reuse/carry provenance and per-facet totals. Existing opt-in ships/status schema and grants; bounded process-memory cache only, stored operator sections remain dated fallback. No mail-complete claim. | Real gateway subprocesses over one-way local transport; callable/reverse-only, reuse/timeout/cache, no recursion/operator fallback, remote clock skew and restart, grant changes before admission/release, hidden waits/topology, truncated state counts. |
| V3 — Worker/session census | Implemented: shared closed optional census on V1/V2 scoped snapshot DTOs, identity-checked metadata-only host/payload reader, bounded deduplication and fresh activity/worker/session counts. Stored running and live supervisors alone remain unverified. Legacy semantics unchanged; old peers and native operator caches without census remain unknown. Independent of desk bootstrap. | Idle hosted pane vs busy turn, headless wake/job counted once, crew/viewer/command distinctions, sleeping/lapsed lease, duplicate spaces, reused pane/socket/session identity, host/process namespace unavailable, stale stored-running job, race at job finish; no screens or pane rearrangement from reads. |
| V4 — Job/work inspection | Implemented: `ship_jobs`, `job_inspect`, `ship_work`, retained terminal history and paging/count readers, safe owner/task/lineage/progress/error DTO. Can precede 2.4c 2a/2b/3; mail-derived text still scope-filtered, not a mail subscription. | Exact owner/incarnation restrictions, live/terminal/resumed states, scheduled vs unscheduled tasks, 40 open tasks with zero workers, queue/park ages, null state-transition age, concurrent finish/page/restrict, byte/scan limits, retained-history gaps and secret/control canaries; native CLI parity assertions. |
| V5 — Capacity and blockers | Implemented: `machine_capacity`, safe typed `runtime_needs` metadata, effective host/charter policy provenance and headroom, reuse admission readers only. Independent of desk bootstrap; aggregate **mail** needs remain incomplete. | Finite/unbounded/unknown limits, charter vs host occupancy, shared ledger counted once, CPU/ramp/memory/disk on distinct filesystems, light memory holds, stale/escaped payload reservations, queue fairness/last-blocker freshness, restricted-host inference protection, nonmutating reads, long-lived homes with thousands of finished jobs/tasks/mail rows, and hidden-parent approval liveness. |
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
to test local tri-state/legacy compatibility and nonmutating homes.

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

### PR6a — scoped spoken runtime status and sessions

The opt-in scoped live-voice path now offers `privateer_status {machine?,ship?}`
under existing `fleet:read`, fixed to `status {schema:"runtime.v1",…}` or selected
`ship_runtime {ship,machine?}`. `privateer_ships` remains directory-only; trusted-v1
`door.py::status_text` is neither reused nor migrated. No new scopes/grants/routes,
freshness policy, authored-text speech, work/recovery/configuration/approval or
capacity tools are added. Pairing and keyed-send custody modules are unchanged.

The separate `scoped_runtime.py` renderer validates coverage and consumes core
contributions, completeness, machine lists and per-ship current/facts. It speaks
omissions and original timestamps/ages before counts; checked first-mate/crew/
errand workers, commands, uncertain jobs, overlapping session categories, recorded
job states and backlog remain distinct. Configuration is recorded, not liveness;
optional session rows speak only role/state and safe harness/host-mode labels,
not opaque references. Missing detail is not no sessions. Speech bounds and
additional row omissions are explicit in the scoped protocol, not a new census.

Unknown/unreadable/unsupported/timeout is **could not check**; cache/carry is
**last observed** with original timestamps and never current. Reuse retains age.
Hidden/absent/removed selectors share a nonrevealing unavailable answer, never a
retirement claim. Malformed coverage fails closed. Restriction/session replacement
during reads drops late output with the existing release fence, without v1 or
operator fallback. Mail is **disabled; not checked**. Reverse-only reads remain
**unsupported read direction / incomplete feature**, never a repaired mailbox.

Fixture map: `tests/live_voice_scoped_runtime_test.py` covers active/idle/unknown,
backlog versus workers, optional/omitted rows, malformed coverage, live/reuse/
cache/carry, ages and canaries. `tests/live_voice_scoped_test.py` adds fixed mapping,
failed reads, restriction/replacement barriers and authority isolation to the
existing send regression suite. `tests/fixtures/door_live_voice_scoped.rs` exercises
runtime overview/selected reads and denied targets through the real supervisor,
HTTP and fake app-server pipes alongside exact-send/restart custody. Trusted-v1
regressions remain in the standing live-voice suite. Gate outcomes are reported
separately; fixture inventory is not a claim of a completed gate.

PR6b selected recorded detail/capacity, mail integration, actual account/audio
interoperability, native autonomy and production deployment remain undelivered.

### Selected-ship runtime surface

Scoped v2 and MCP `ship_runtime {ship,machine?}` require `fleet:read` and return
exactly the existing closed `runtime.v1` projection with a required ship selector.
The optional machine handle only narrows authorized directory entries (`local`
means this core); neither selector creates a route or expands a grant. The core
pins the schema internally. Missing ship, extra fields (including `schema` and
`include_text`), invalid handles and raw v2 escape arguments are refused. Hidden
and absent selectors share the existing nonrevealing refusal; incomplete inventory
cannot establish absence. Trusted v1 explicitly refuses the new command, while
legacy `ships` and `status`, including their opt-in schemas, remain unchanged.

The read reuses V1–V3 observations, the dedicated `fleet.runtime.v1` allowlist,
fixed freshness/reuse policy, link-incarnation-bound caches and host credential
recheck before release. A replacement link cannot publish the previous peer's
held reply, reuse or stored fallback. No new linked operation or receiver DTO is
introduced. The same bounded process-memory cache and registry authorization/audit
bookkeeping remain explicit; no home initialization/migration, reconciliation,
work, presence, mail, resource, pane or durable snapshot-cache writes are added.

Clients receive the selected `ships` list, `selection`, per-facet `coverage` and
`observed_totals.scope:selected`, not a raw job/session/bearings object. Checked
first-mate/crew/errand workers, command payloads, uncertain jobs and distinct
session counts retain V3 semantics. Cached/carried facts stay dated, never current;
unknown domain membership and mail coverage remain incomplete. No authored text,
harness options, binding identities, screens, transcripts or operator JSON is
exposed. The optional detail extension below adds only safe configuration labels
and anonymous checked session rows. Resource administration, scoped
bundled-voice migration, mail integration and real-account acceptance remain
separate work; existing grants and provider routes do not change.

Test map: `door::runtime::tests` validates the fixed selected schema;
`door::scoped::tests` and argument-error metadata cover exhaustive scope/argument
mapping. `tests/fixtures/door_runtime.rs` exercises the supervised new command
with forty-task idle census, stored-running uncertainty, missing homes, unequal
grants, denied selectors, strict raw arguments, byte-preserved homes and trusted-v1
refusal. `door_runtime_linked.rs` exercises real gateways, checked positive census,
shared reuse with ships/status, failed-contact cache, carried/restarted fallback,
reverse-only paths and restriction before admission/release.
`link_incarnation.rs` exercises replacement and held-reply races on the new
surface without clock assumptions. MCP SDK tests cover both protocol eras,
discovery, annotations, redaction, coverage rendering and value-free argument
errors; OAuth discovery includes the new existing-scope read. Gate outcomes belong
in task reports, not fixture inventories.

### Anonymous session detail extension

The existing `runtime.v1` projection (selected `ship_runtime` and opt-in
`fleet_ships`/`fleet_status`) adds optional `harness` and `host_mode` fields on
runtime ship facts and optional `census.session_details`. Authority is unchanged:
`fleet:read`, filtered by selected directory/ship and rechecked before release.
No new command, scope, grant, action, provider route or voice migration is added.

Ship `harness` is the recorded first-mate configuration, restricted to the native
inspection harness allowlist plus `pi-signed`; custom/unrecognized labels are
withheld, never returned as paths or arbitrary plugin IDs. Ship `host_mode` is
`hosted` or `headless` from recorded configuration. These fields are not a claim
that a corresponding endpoint exists or is alive. A checked pane may be hosted
while recorded first-mate configuration is headless; they describe different facts.
Missing labels mean unknown, not a guessed default. Dated operator fallback may
supply safe recorded configuration but cannot establish live sessions.

Session detail rows contain exactly `reference`, `role`, `state`, nullable
`harness`, and `host_mode`. They describe only identity-checked live endpoints
already observed by V3's census, not additional host discovery. Roles distinguish
`first_mate`, `crew`, `errand`, `command`, `viewer` and `unknown`; states are
`working`, `idle`, `asleep`, `blocked`, `viewer` and `unknown`. `done` is rendered
as idle, and unrecognized adapter states are unknown. Viewer endpoints are never
workers; queued/parked/terminal non-viewer panes retain unknown activity. Row
harness is the checked adapter's allowlisted label, not options/provider/model;
row host mode is `hosted` (or `unknown` when a received peer label is not
recognized). Received harness/ship host-mode labels unknown to this core are
withheld, and unknown row host-mode labels become `unknown`, without discarding
the section or its checked summary facts. Closed keys, structure and field types
remain strict. Headless payloads remain worker/job evidence, not
invented live interactive session rows.

References are SHA-256 opaque metadata fingerprints bound to the existing random
home runtime origin, database device/inode, selected ship and the complete checked
endpoint/adapter-session identity. They remain stable across activity/lease
renewal, change with replacement identity, and are **not actionable handles** or
permission for session reads/typing. No raw session IDs, socket/pane/workspace/tab/
terminal identities, OS process identity, titles, paths, argv, environment,
logs, screens, transcripts, options or authored text are emitted. References are
not a cryptographic isolation guarantee against configured linked operators.
Legacy homes lacking an existing origin retain summary facts but omit details;
reads never initialize/migrate them.

Detail uses the existing 200 owned-job/200 binding budgets and deduplication:
at most 201 checked live endpoints including the first-mate lease. Present rows
match `sessions.live` and the known state-category counts. Unverified or expired
bindings produce unknown summary evidence, not fabricated row identities. An
unknown adapter state can still have a checked identity and an unknown row.
Existing `sessions_complete`, `activity_complete`, census truncation/reasons and
per-facet coverage apply to these observations. An empty row list alone is not an
all-clear. Lapse during the final input recheck removes only the original lease
row/contribution; finish/rebind/custody/home-origin races discard the observation.

The same `fleet.runtime.v1` gateway negotiates `census:2`. New receivers return
the extension only for that version. Requests with `census:1` receive exactly the
V3 closed shape, omitting configuration labels and session detail; unextended
requests keep the V2 shape. The caller downgrades 2 → 1 → unextended only on each
exact legacy `unsupported_arguments` envelope refusal. Other errors do not retry;
there is no operator fallback or new route. Every attempt, held reply, cache
reuse and fallback retains #553's link-incarnation fence. Older peer/legacy-home
missing detail means unknown support or omitted detail, not zero sessions.
Negotiated shaping happens before the 768 KiB section bound: census:1 does not
build session rows, and neither old envelope includes detail/configuration fields.
If census:2 details would exceed that bound, all session detail lists are omitted
while summary counts/completeness and configuration facts are retained; if the
summary itself exceeds the bound, the section still fails closed. Absent detail
never resets `sessions.live` or means an empty session list. Local/direct/reused
facts retain existing freshness rules; cached/carried rows stay explicitly dated
inside `runtime.facts` with `current:false` and never contribute current activity.

Writes remain `none` for homes, work, mail, presence, panes and resources; only
the existing bounded process-memory observation cache and registry admission/
audit bookkeeping apply. No new read locks, durable caches or home schema writes
are introduced. Runtime/mail/domain completeness remains independent.

Test map: `snapshot::census::tests` covers anonymous references, identity/home
replacement, deduplication, viewer/crew, idle/busy/blocked/asleep/lapsed/unknown
states, legacy origin, strict wire validation and canary redaction alongside
existing real-socket/payload race tests. `snapshot::scoped::tests` checks safe
configuration, legacy decoding, receiving label normalization/type checks and
oversized negotiated sections with summary-preserving detail omission. The supervised
`door_runtime_linked` fixture includes a real metadata-only Unix host and real
gateways, 2/1/unextended negotiation, shared reuse, dated failed-contact cache,
restricted topology, current sections with newer peer harness/host-mode labels,
and credential-release fencing. `link_incarnation` checks
nonempty session references against replacement/held-response cache races.
MCP SDK tests exercise nonempty session rows through both adapter protocol eras,
state changes, stable references, unchanged home data, metadata-only host calls
and redaction. Gate outcomes belong in task reports. Resource administration,
blocker-reference detail, aggregate mail, scoped bundled voice, real-account/
voice acceptance, native autonomy and production deployment remain out of scope.

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
On macOS, `kern.bootsessionuuid` from `sysctlbyname` binds the key to the boot and
Darwin's `CLOCK_MONOTONIC_RAW` supplies continuous monotonic milliseconds,
including sleep, across gateway processes. Failure to obtain either value fails
closed; wall-clock synchronization cannot extend expiry or change the boot
identity. V5 replaces the former wall-clock-sensitive `kern.boottime` binding. Other platforms retain the conservative
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

Inspection preserves resumable partial facts at its observation budget, does
not skip an interrupted row, and ends correctly if retention removes a former
high-water tail.

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
without a Linux-only exclusion on the linked paging fixture.

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

Remaining gaps after V4 (V5 delivery follows): actions, aggregate mail, transcripts/screens,
a configured complete fleet manifest, real ChatGPT/voice acceptance and deployment.

### V5 delivered contract and evidence

The fixed scoped commands/MCP tools `machine_capacity` and `runtime_needs`
require `fleet:read`. Capacity accepts `machine` (required; `local` means this
core), optional `ship` and `task_id` (task requires ship). Needs selects one
required `ship`, with `limit` (default 25, max 50), opaque `continuation` and
`include_text`. There is no generic native dispatch, endpoint/path selector,
new scope/preset, action entitlement or provider-route change. Both operations
extend the existing `fleet.inspection.v1 {context,operation,args}` receiving
allowlist, independently validating scopes, schema and local ownership. The
host resolves configured directory handles and rechecks the actual credential
before release. Reverse-only, unsupported and unavailable reads are incomplete,
not zero capacity or an all-clear. These reads are not cached as current facts.

The closed `capacity-needs.v1` envelope contains `capacity`, `needs`, `unknown`,
`more`, `continuation`, `coverage`, `writes`, `cache_writes`, and
`action_authority:"none"`. Coverage names the selected machine/ship and facet,
state, mode and observation timestamp; `other_machines:not_requested` and
`mail_complete:false` avoid aggregate claims. Needs coverage is always partial.
Capacity coverage is complete only for a successful unrestricted ledger read,
not a promise to admit a later launch or proof every possible filesystem is
measurable. Missing ledgers/settings/host measurements produce null capacity,
explicit unknowns and unavailable coverage, never invented empty ledgers.

Capacity fields are `ledger` (opaque identity), `policy`,
`charter_heavy_cap`, `charter_policy_state`, `charter_policy_source`,
`charter_occupied_heavy_slots`, `occupied_heavy_slots`, `free_heavy_slots`,
`headroom_basis:heavy_slots_only_not_agent_capacity`, `reservation_count`,
`memory_reservation_count`, `holds`, `queue_depth`, `queue`, `capacity_blocked`,
`readings`, `route_availability`, `route_state`, and `shared_host`. Policy
separates configured/resolved host heavy caps, load/memory/disk floors, ramp,
heavy estimate, crew default memory and per-bundled-harness memory with sources
(host override/manifest/default; not a route-specific launch estimate). Source
labels distinguish host configuration from runtime defaults. A selected task
uses its active charter and exactly admission's home/charter share accounting.
Finite heavy-slot headroom is the minimum remaining host/charter share; null
known caps mean unbounded **by that policy**, while unknown charter authority
withholds headroom. There is no `free_agent_slots` field or memory-to-agents
conversion. Ramp and heavy-share arithmetic are factored out of admission and
used by admission itself, not independently recreated in the door or Python.

Holds contain safe ship/job/incarnation/task handles, heavy flag, admission
time, resolved resource demand and `unknown_held_conservatively` liveness.
All recorded holds count, including light memory reservations, stale supervisors
and escaped/uncertain payloads; reads never release or reconcile them. Queue
entries contain safe job ID, creation time, last typed own-admission classification
and its recorded timestamp, with
`last_admission_classification_not_current_diagnosis`. Older records have null
classification timestamps. The queue writer dates each entry's own admission
observation, not a queue-order refusal; the read never runs it or prunes stale
waiters.
CPU/load, recent ramp CPU/memory, released load, available/total memory and
per-filesystem available/total/committed disk bytes are separate readings.
Filesystems have opaque handles; a recorded device without a safe measurable
path is explicitly unknown, never measured as though it were the root disk.
The machine default ledger is read without a ship; a selected home uses its
configured ledger. Ledgers are never aggregated once per ship. Multi-ledger
machine-domain discovery and demand-specific launch explanations are not added.

**Shared-host boundary:** only a grant whose ship restriction is null may see
host policy, global occupancy/queue, host readings or aggregate route states.
An explicit named-ship list never qualifies, including a list containing all
current ships. Named grants must select a ship; the reader restricts ledger
records and queue custody before counting. It returns selected-ship holds,
occupancy and queue depth with an unattributed capacity wait; no shared free
headroom, charter/host caps, hidden holders, queue positions/head reasons,
readings or route counts. It never subtracts a visible count from a global one.
Current task/worker attribution is checked with V4's reference reader. Route
availability uses the existing read-only route-state reader and returns only
counts by available/idle/backing_off/capped; no account/route keys, classified
error strings, probes, policy edits or backoff changes.

Needs reuse `src/needs.rs`'s native typed derivation with selected ownership
applied before keyset paging. Rows contain opaque stable `id`, `origin`, native
`source`/`source_id`, ship/task, kind/evidence, safe mail ID/kind/urgency,
creation/deadline/observation timestamps, opaque version, nullable question and
choices, and `text_withheld`. Blocker-holder lists, dependency/route keys,
approval artifact/environment/program/argv/cwd, actions and authored runtime
error prose are not projected. Selected queued needs remain unattributed
capacity waits rather than probing shared stale holders or diagnosing disk
from prose. Ask question/body and choices need **both** `include_text:true`
and `mail:read`, with V4 masking/control/path removal and bounds, re-masked at
receipt. Approval questions/choices are always withheld: native privileged
approval bodies embed argv/cwd, which this projection must never return. Native unknown domains remain explicitly unknown, including provider
health and aggregate mail. Machine-wide readiness items are not silently
attributed to a selected ship. Reading never answers or authorizes an ask.

Needs continuations reuse V4's bounded registry, authority/filter/visibility
binding, source incarnation, boot clock, expiry and eviction machinery. Native
string keysets re-observe current typed facts, not a frozen historical inbox.
`writes:none` means no home/work/mail/presence/resource mutation;
`cache_writes:bounded_registry_continuations_only` and existing registry
authorization/schema/audit bookkeeping remain explicit. Reads open existing
Stores/ledgers read-only with observation budgets, at most 2000 expanded ledger
records per facet and a 64 KiB reply bound; oversize/budget failure stays unknown.

The four V4 re-review nits are included: macOS boot-session UUID rather than
wall-clock-sensitive boot time; lapsed already-unknown lease sessions counted
once; enrichment interrupts return empty undrained pages retaining their cursor;
and received authored text is re-masked rather than rejected for non-idempotence.
Tests extend the cross-gateway clock regression (macOS additionally checks UUID
shape), socket lease census, large-mail enrichment and receiver masking fixtures.

Evidence fixtures: `door::capacity::tests` covers finite/unbounded/unknown
policies, charter-vs-host occupancy, shared-ledger counting, light memory holds,
conservative stale/escaped reservations, last-blocker timestamps and stale queue
retention, native admission-reader parity, named-list inference protection,
pre-page need ownership, text scopes, source/filter/expiry continuation binding,
schema and receiving authority. Linux filesystem fixtures use two real distinct
filesystems plus an unmeasurable device. Existing admission/job suites retain
CPU/ramp/memory/parent/disk and queue fairness/process-recovery boundary tests.
The real supervised linked inspection fixture exercises both new operations,
receiving scope/ship refusals, native CLI parity, hidden-canary withholding and
unchanged home bytes. All tool inventories and argument-error inventories are
updated, including MCP OAuth discovery and SDK invalid-argument/read assertions.
Gate outcomes are reported separately; fixture addition is not a passed-test
claim. No action parity, aggregate mail completeness, transcript/screen access,
real ChatGPT/voice acceptance or production deployment is claimed.

#### V5 review round 2: restricted-source failures and paging progress

The needs-token expiry regression uses issue-relative subtraction.
The job/work token expiry fixture uses the same
issue-relative subtraction; neither fixture assumes ten minutes of host uptime.
Cross-process clock tests compare ordering and boot identity, not a minimum
uptime or a fixed wall-clock epoch.

Restricted reservation SQL now selects **both recorded ship and exact home**
before deserialization, byte limits or counts. Same-named ships at another home
cannot turn a read unavailable, even with oversized/invalid reservation payloads.
A selected hold is a durable reservation fact, not proof that a retained current
job still exists: pruned/retried or no-longer-authorized job references become
null while the hold, heavy occupancy and memory reservation remain counted
conservatively. Task references additionally require current selected ownership;
lost task custody masks the task ID rather than failing the observation.
Reference checks use the same masking rule. No recovery/release is performed.

The error-shape audit also removed three related oracle paths: queue job
visibility is resolved through the existing inspection reference reader before
ledger payload expansion; withheld charter policy/ancestry is not read; and
needs' oversized-mail guard uses the same sender **and task-owner** restriction
as its candidate reader. Needs uses the same authorized-job ID selection before its public page limit;
an unprovable needs-job reference is omitted rather than making the whole page
unavailable or consuming a visible row's place. These changes do not authorize hidden
worker/job references or shared readings, limits, queue positions, route keys or
totals. Explicit named lists, including every current ship, remain restricted.

The scan result records pre-row enrichment interruption separately. Only that
case can issue an empty continuation at an unchanged cursor; a first data row
that exhausts the budget fails with the existing message-size refusal, rather
than producing unlimited no-progress continuations. The admission decision doc
comment is restored above `admit`; `recent_demand` retains its own description.

Added regressions cover other-home and hidden-ship oversized ledger records,
pruned/retried/task-custody-changed holds, hidden-worker queue payloads and needs,
withheld invalid charter/settings, selected-sender mail on a hidden-owned task,
nonmutation, first-row VM interruption with no token, and actual enrichment
interruption/token/retry. Both interruption fixtures exceed the fixed SQLite VM
budget rather than relying on machine speed. The public DTO field inventory
above is unchanged; nullable hold references are now useful for retained holds.
Shared-database scans retain cooperative work/time limits and do not claim
physical timing isolation from other host workloads.

#### V5 review round 3: current custody and unobserved approval ancestry

The shared inspection custody reader now takes the caller's required recorded
states and filters them in SQL **before** the 2000-candidate bound: capacity
queue reads select `queued`; typed needs select `queued` and `orphaned`.
Finished and unrelated running jobs no longer consume that bound. Reservation
references still use the exact current job/task/incarnation checks independently
of the queue filter, so a running or retained stale hold is not discarded.
Both restricted workspace readers likewise select nonterminal tasks before
bounding custody. Old `done`/`cancelled` tasks and their retained workspace
queue records cannot exhaust current-task custody. The redundant second hold
recheck in the door is removed; the single ledger-reader call site retains
conservative holds and masks unprovable references inside its observation budget.

Scoped approval liveness uses the native authority traversal but its checkpoint
checks selected-ship ownership **before** loading each ancestor. At the first
foreign or missing parent it stops, keeps the item, and treats the remaining
authority as unobserved. A foreign parent's cancellation, absence or cycle must
not determine either item visibility or error shape, even when the grant lists
that ship too. Native operator reads keep full lifecycle/hash validation;
visible selected ancestors still reject cancellation and invalid cycles.
Every successful scoped needs reply now includes `task_captain_actionability`
in `unknown`, regardless of whether any item has hidden ancestry, alongside
`action_authority:none`. This is an added fixed unknown code, not a new action
or an assertion that the approval can be granted. The DTO field list above is
otherwise unchanged. Shared-host totals, policies, readings and route inventory
remain withheld for **every** named-ship grant; no subtraction, reconciliation,
resource release or provider probe is introduced.

New `door::capacity::history_tests` regressions cover 4000 finished jobs across
all five terminal states followed by current queued/orphaned/running records;
a home with 4000 finished tasks, 4000 finished jobs and 8000 answered mail rows
followed by a current ask and workspace waiter (including continuation drain);
and foreign cancelled/done/open, missing and looping approval parents, with
selected-only, explicit-all-names and unrestricted grants. The tests check
nonmutation, current hold references, no history-derived queue entries, unknown
actionability and native operator lifecycle behavior. Existing pruned/retried
hold tests continue to exercise the single reference-recheck path. No new test
assumes a wall-clock epoch or minimum boot uptime. All readers retain bounded
cooperative SQL/time/byte limits, not unlimited scans or timing isolation.

Bounded observations may still be partial or unavailable for genuinely
excessive current candidates or exhausted cooperative work/time/byte budgets;
this is not an unbounded-history or physical timing isolation guarantee.

### V6a delivered contract and evidence

**Surface and authority.** Three fixed-schema v2/MCP actions, no generic dispatch,
argv, operator identity, payload or route fields:

- `work_rank {ship, reference, rank, idempotency_key}` maps only to native
  `task.rank`; rank is 1..10000 or null (clear), and the task must be open.
- `work_enqueue {ship, reference, brief?, idempotency_key}` maps only to native
  `task.enqueue`; brief is 1..16384 UTF-8 bytes or the native assignment note.
- `work_defer {ship, reference, wait, offer_id?, idempotency_key}` maps only to
  native `task.defer`. Wait is a closed tagged shape: task/request with string
  `ref`, remote with `ref:{ship,task,machine?}`, time/attention with integer
  `until_ms`, or manual. Every wait has a 1..1000 UTF-8 byte reason; only the
  relevant fields are accepted. Offered/claimed work needs the current offer.

All need only `work:manage`: replies are closed `work.action.v1` acknowledgments
(`operation`, selected `ship`/`task_id`, `applied:true`, stable `result_id`, core
provenance), never native backlog, brief, reason, hint or schedule data. Reading
references through `ship_work` still needs `fleet:read`; no scope is inferred or
expanded. Existing production/privileged confirmation, deployment, provider
route and resource admission boundaries are not exposed or changed.

**Exact references and native custody.** `ship_work.work[].reference` is
`{origin,ship,task_id,revision}` (nullable for an unmigrated read-only peer).
Origin binds the home's runtime incarnation and database file identity. Durable
home triggers advance task generations on task changes, meaningful schedule
changes (state, wait, offer, holder, job, owner or native revision) and charter
updates, including change-and-restore in the same clock tick. Reconcile polling,
last-check/lease times and reason-only saves do not invalidate references. Normal
startup replaces the legacy unconditional schedule-update trigger with version 2
on existing homes; repeated startup does not rewrite the schema. Tombstones survive
task deletion; normal Store initialization atomically installs the complete schema
and version marker, never a reader or a guarded action. Actions use the existing
noninitializing captain-action open: no create, migrate, repair, chmod or journal
mode changes on a denied/legacy target. File identity is checked around opening
and origin again before commit. A receiving action takes the home's IMMEDIATE
lock before checking generation,
custody, waits, invoking the existing native implementation, auditing and saving
the result. A changed offer/claim or owner cannot slip between check and write.
Native enqueue already refuses live/uncertain descendant jobs, including directly
started agents. External defer additionally refuses those jobs rather than
impersonating a running crew's park. Native CLI scheduling remains compatible.

**Visibility, routing and freshness.** Grants are upper bounds. Target home and
current task owner must match the selected ship. Hidden/missing task refs and
wait dependencies have the same nonrevealing denial. Check visible ancestry,
descendant custody (for native live-job walks), local task dependency chains,
request participants/task, and remote dependency ship before native execution.
Remote waits preserve native configured-route validation and subscription/outbox
semantics: they do not claim the far task has already been observed. A reference
to a now-hidden owner is refused for a new action. A matching recorded result is
recovered before checking task/dependency existence or visibility: it returns
only the original acknowledgment already authorized to that grant, not fresh
read data. Current grant/resource/visibility revision and home incarnation still
must match; grant edits/revocation deny recovery. Linked calls use only
`fleet.work-action.v1` with a closed three-operation receiver allowlist and a
request-bound delegated context; receivers execute locally without recursion.
The inspection/read allowlist is unchanged and refuses every action. Old peers
return `unsupported`, reverse-only peers refuse, transport uncertainty returns
`outcome_unknown`; none retries as operator. Host credentials/scopes/ships and
visibility revision are freshly checked immediately before action admission and
before release (including replays). **Admission/commit window:** the registry
check is immediately before execution, not atomically coupled to a local home
transaction or a linked round trip. Both local and linked actions use the
admitted immutable context through home commit; a revoke/restriction landing
after that check may therefore still be followed by a mutation. The receiver
validates that context at its own gateway admission; it has no originating
bearer credential or live registry grant to re-resolve. The host release check
hides the acknowledgment after revocation but cannot undo that admitted
mutation. No stronger pre-commit revocation guarantee is claimed. Configured
linked operators and same-user plugins retain the design's stated trust limits.

**Durability and audit.** A global originating-core key pin prevents using the
same grant/resource/key for different operations, homes or peers. The target
home records successful native effects, metadata-only delegated audit and the
stable result in **one transaction**. A matching retry reaches the same home and
recovers its recorded acknowledgment even after commit/response loss or process
restart; it does not reapply. Conflicting arguments or authority generations
refuse. Native delegated defer audit omits client-authored reason/hint text;
`door.work` records grant/tool/exact reference/result and a key hash, never brief,
credentials or raw keys. Native CLI audit behavior is unchanged.

Retry protection lasts **24 hours**, with caps of 256 per grant and 4096 per
originating registry/target home. In-window saturation fails closed with
`retry_capacity`; matching in-window retries still work. Completed home results
older than that window are pruned inside the same IMMEDIATE transaction as
lookup, admission and commit. Originating pins carry creation/finish times and
are likewise pruned under IMMEDIATE admission; successful/uncertain outcomes
retain them, and finishing/replaying conservatively refreshes their age to cover
the home's full retry window. In-flight reservations coordinate equal concurrent
calls without holding a registry transaction across home/network execution. A
crash leaves an uncertain pin until retention expires. Known pre-effect denials
remove their reservation once no concurrent equal call is active and no result
was recorded; a denied call cannot erase another call's successful/uncertain pin.
Grant revoke/delete triggers remove originating pins transactionally with the
registry edit; expired grants retain retry protection until retention ends but
do not consume live registry capacity. Home records are bounded independently
and cleaned by retention (there is no cross-database revoke transaction). Legacy
records without timestamps receive a full retry window at normal startup migration.

Within the retry window, recovery uses the same key/args/reference, never a fresh
key to escape uncertainty. Beyond it no exactly-once recovery is promised: obtain
a fresh observation/reference and use a fresh key for any deliberately new action;
do not blindly retry an uncertain old call. Freshness checks still refuse stale
references even if their result expired. Pre-effect denials leave no home
mutation/result; ordinary bounded registry admission audit remains. Replays are
not new authority and are refused after grant edits/revocation, but a subsequent
hidden/deleted task or dependency alone does not destroy the recorded result.

Schema installation uses the existing Store IMMEDIATE transaction primitive,
with a regression requiring that locking path and stable schema versions across
repeated opens. Ordinary jobs cannot forge an agent launch: the real payload
fixture uses native `agent.start` with an isolated scripted planner to establish
actual subprocess custody, not native autonomous-agent behavior.

**Round-two regression fixtures:**

- `door::idempotency::tests`: 768 successful calls across retry windows for one
  grant; 16 grants/4096 pins and recovery after expiry; in-window capacity and key
  conflicts; over 256 known denials without persistent pins; concurrent equal
  deny/success; crash/uncertain pins; legacy timestamp migration and revoke/delete
  triggers, expiry preservation and no resurrection on post-revoke finish.
- `door::actions::tests`: 512 successful home actions across retry windows;
  4096-result multi-grant home expiry; in-window replay at capacity; unchanged
  home on denial; lost-response restart after task/dependency hidden or deleted;
  `ship_work`'s actual reference-bearing scan followed by several full scheduler
  ticks for each of ready/waiting/offered/claimed/executing then a successful action.
- `scheduling::references::tests`: existing-home trigger replacement and result
  migration; housekeeping saves preserve references, each meaningful field change
  invalidates them, and repeated migration remains structurally idempotent.
- MCP inventory regression checks the 24-hour contract and both registry/home caps.
  Existing real linked subprocess revocation tests cover admission refusal and
  receiver commit before host release refusal; the documented local home window
  uses the same admitted-context semantics, not a new atomic cross-store claim.

Added/extended fixtures:

- `door::actions::tests`: fixed schema/native variant matrix, reserved escape
  fields, unequal grants/hidden targets/dependencies, stale ownership/offer/charter
  generations, same-tick restoration and deletion/recreation, current offer,
  native cycle/time/request
  checks, stopped ancestor, live direct/uncertain job refusal, assignment fallback,
  time/attention/remote waits, long-lived 3000-task history, concurrent equal keys,
  atomic rollback on post-native audit failure, restart/lost-response replay,
  legacy/missing-home denial without migration/creation/chmod (main DB and WAL
  byte equality plus permissions), and separate receiving/read allowlists.
- `tests/fixtures/door_actions.rs` (door binary): actual supervised v2/native/raw
  alias/gateway bypass matrix, work-only acknowledgment, native rank/schedule
  parity, supervisor restart/replay, and refusal while a real direct native
  agent-provenance subprocess is live; real one-way linked subprocess success,
  receiver scope/ship narrowing, read-gateway refusal, unsupported peer,
  revocation before admission/no receiving call, and receiver commit followed by
  host release revocation/discard. No production processes or routes are changed.
- MCP suites: both protocol eras discover only scope-authorized new tools;
  work-only grants have no fleet reads, mandatory key/schema/error behavior and
  real rank/enqueue/defer/replay. Pinned Observer/OAuth lists stay unchanged and
  explicitly exclude the new writes; argument-error scope inventory is updated.
- V5 review nits: `capacity::nit_tests` checks the selected task's own revoked or
  replaced charter despite hidden ancestry, and keeps a closed task's recorded
  workspace waiter with a supervisor in selected queue depth, matching the
  conservative shared reader. A read-only SQLite attachment filters selected task
  custody before bounding workspace queue IDs, so hidden waiters cannot consume
  the selected ship's queue budget (2050-hidden-waiter regression), and lifetime
  closed-task history is not scanned into that budget. Neither read probes/prunes
  processes or writes home.

Not delivered by V6a: cancel/retry (see the following cancellation-only V6b split), resource administration, native launch, session
interaction, production/privileged client-only approval, fleet-wide mail
completeness or real account, client or voice interoperability. Those remain separate gates.

### V6b delivered contract and evidence

**Authorized split: cancellation only.** `job_cancel {ship, reference,
idempotency_key}` stops or requests stopping work under `work:manage`; it grants
no launch, recovery, approval, liaison escalation, route or resource-admin
permission. The exact V4 reference is `{origin,incarnation,ship,task_id,id}`;
obtaining it through `ship_jobs` still requires `fleet:read`. Unknown/extra
arguments (including PID/group IDs, argv, route, policy, estimate, force and
replacement IDs) are refused by core admission, not merely tool discovery.
Native `job.cancel`, dotted aliases, gateway and generic operator RPC calls
stay outside external v2. The later V6b3 split adds only guarded `job_retry`. The MCP description explicitly says **Stop
work** and its annotation is destructive, not read-only or unconditionally
idempotent.

**Native mapping and transaction boundaries.** A noninitializing home open and
IMMEDIATE transaction verify home origin, exact incarnation/task/current owner,
worker visibility, task ancestry, both directions of parent/retry/resume/relaunch
lineage and parked crew/child/resumed references, including the park's task and
ship. Hidden/missing/stale references have the same nonrevealing refusal before
any cancellation request. The graph is bounded (128 jobs, 128 parks per node,
1024 pending references); excessive graphs fail closed. **One-bit ancestry
signal:** an otherwise visible exact target can still return `ship_not_allowed`
when an ancestor, lineage job or park reference is hidden. Comparing an allowed
target's success with that uniform refusal can reveal that at least one required
visibility/custody condition was not satisfied. It does not identify the hidden
ship, job, ancestor, edge or reason; missing, stale and hidden references share
that same refusal. This bounded signal is intentional: do not replace the graph
check with cancellation that can cross unseen custody. A job finishing in the
same incarnation is still the same exact job. Retrying it creates a different
job: cancelling the old job never claims to cancel its successor.

The shared native cancellation-request helper records `cancel_requested` and
native caller identity. That intent, delegated metadata-only `door.work` audit
and an uncertain recovery record commit together. Only then does the existing
native `jobs::cancel` path run, with an incarnation guard, outside any caller
transaction. This preserves native starting/queued/parked/supervised/orphan
behavior, identity-checked signals, descendant escalation and conservative
ledger custody. No signal or shared-ledger release is claimed rollbackable.
A subsequent home transaction records the final acknowledgment and audit. Any
post-intent crash, signal, native, audit, result or commit failure is
`outcome_unknown`, never a definite pre-effect refusal. A supervisor may already
honor the committed intent; killing a wrapper is never proof its payload stopped.

Successful replies are closed `work.action.v1` acknowledgments with stable
`result_id`, `operation`, `ship`, `task_id`, `job_id`, `applied:true`, `outcome`
and core provenance. `applied` denotes the durable request. `cancel_requested`
is not proof of stopping; `stopped` denotes the native terminal outcome for this
exact job, not completion of its task or successor. A `resumed` original job is
not a stopped successor: its acknowledgment stays `cancel_requested`, without
claiming that the resumed work stopped. Surviving or uncertain
payloads retain exactly the native custody/reservation rules. Replies never
include native job records, process IDs, argv, paths, logs, route or authored text.

**Recovery, authority and routing.** Reuses V6a's originating-core key pins and
home result table, 24-hour retention, 256-per-grant/4096-global caps,
transactional revoke/delete pin cleanup, equal-call coordination and
operation/argument/authority/resource binding. Finalization also compares the
original result ID so a delayed native call cannot overwrite a newer admission
after its original recovery row expired. Recovery precedes fresh
job/lineage visibility checks but still checks current admitted authority and
home origin. Completed results replay unchanged (a historical requested result
does not later claim stopped). An uncertain recovery record returns
`outcome_unknown` without resending signals, even if its original caller died
before native follow-through. This is safe outcome recovery, not guaranteed
completion of a supervisor-less/parked cancellation after a crash. Do not use a
fresh key to escape uncertainty. Beyond 24 hours no exactly-once promise is made.
Native supervisors retain their normal restart/reconciliation behavior.

Linked calls use only the existing `fleet.work-action.v1` receiver allowlist,
now rank/enqueue/defer/cancel, with request-bound context and no recursion or
operator fallback. Read gateways remain action-free; unsupported peers return
unsupported. V6a's explicitly documented admission/commit revocation window
still applies. The #515 nit is fixed for all work actions: successful execution
followed by release refusal returns `outcome_unknown`, hides the acknowledgment
and audits `request.unknown`/`outcome_unknown`, not a definite auth/scope denial.
An already-uncertain execution also stays uncertain if release fails. The
bounded generic outcome audit uses `request.unknown` for uncertainty consistently.

**Fixture inventory.** New unit fixtures
cover schemas and escape fields, unequal grants, unchanged denials, stale
incarnation/ownership/ancestry, hidden workers and all lineage directions,
park-child/park-ship restrictions, pre-intent audit rollback, native uncertain
cancellation preserving the request, crash/restart and replay-before-visibility,
equal-call concurrency without duplicate signals, finish/retry/incarnation
races, long-lived 3000-job history and shared retention/caps. Real supervised v2
fixtures exercise work-only cancellation of a native agent-provenance payload
with a recorded escaped descendant ignoring TERM, native eventual settlement and
ledger release, historical replay after restart, linked receiver narrowing,
unsupported/read-gateway refusal and post-commit revocation. MCP tests cover
scope-filtered discovery and closed schemas in both protocol eras, destructive
annotations and real terminal-job cancellation/replay; pinned Observer/OAuth
inventories exclude the action.

**Not delivered in the original V6b artifact:** `job_retry`. The first mate explicitly authorized cancelling
first and deferring retry. Native `job.retry` calls `jobs::start`, whose own
IMMEDIATE home admission transaction, shared ledger, workspace allocation and
supervisor spawn cannot safely be nested in V6a's wrapper. Retry needs a native
admission refactor and durable launch/result recovery; no `jobs::start` change
or partial retry tool is included. Provider backoff, argv/route/estimate/policy,
workspace/charter/recovery/admission and production-deployment gates are not
changed. Resource administration, native launch, session interaction, real
account/voice interoperability and production deployment also remain out of
scope. Scripted native planner fixtures establish real subprocess boundaries,
not autonomous agent behavior.

### V6b2 stage 1 — native retry seams and #516 review nits

**Authorized split; no new door tool.** Stage 1 refactors only the existing
native `job.start` / `job.retry` paths. External `job_retry`, native aliases,
launch overrides and operator dispatch remain denied. Tool inventories, scopes,
MCP schemas/annotations and provider/deployment authority are unchanged. Stage 2
must separately deliver the guarded retry tool and its durable outcome protocol;
this stage does not claim retry-over-MCP or exactly-once launch recovery.

**Internal ordering and ownership.** `jobs::retry::request` extracts the native
stopped-unsuccessful eligibility/recovery-authority and original-route backoff
checks and reconstructs only the old payload. `prepare_start` preserves native
validation, historical ID replay, workspace allocation/trimming, estimates and
charter caps; it retains the workspace lock until the prepared launch is consumed.
`admit_start_in` takes a caller-owned IMMEDIATE home transaction, rechecks native
start authority, custody, plugin binding and workspace ownership, reserves shared
capacity (or records native queue position), then inserts the starting job. No
supervisor is spawned in that transaction. Initial shared-ledger reservation
remains **before home commit**, with native insertion/commit-failure compensation;
it is not rollbackable home state. `launch_start` consumes the prepared value only
after commit, preserving hosted/supervisor spawn, identity-update and audit order.
All seams remain private/native-only. There is no new dispatched command or
reachable launch path.

A future guarded caller must check exact-reference/graph/custody visibility and
revalidate retry eligibility under the admission lock, add its intent, uncertain
result and metadata audit to **the same home transaction**, and compensate ledger
reservation if its additional home writes/commit fail. It must recover the
recorded outcome or `outcome_unknown` on same-key replay, never call launch again.
That caller still needs V6a pins/retention/caps, revocation behavior and the
original result-ID finalization guard. The seams themselves do not supply those
records or permission checks, and expose no client approval/route/admission bypass.

**Existing crash boundaries (unchanged).**

- Ledger reserved but home not committed: ordinary returned errors use existing
  compensation. A process crash can leave a reservation with no home job; no
  automatic home-job settlement exists for that missing row. Native stale-ledger
  inspection and explicit `resource.recover` remain the recovery path, requiring
  force/manual verification when no payload identity was recorded. Stage 1 does
  not repair this cross-database crash window.
- Home committed but supervisor not spawned: a `starting` job has no payload.
  Dispatcher `reconcile` fails it as `supervisor never started` after the native
  start grace and releases its reservation. A supervisor-less `queued` job is
  failed/dequeued; `live_queue` prunes vanished/grace-expired queue positions.
  It is **not** automatically relaunched.
- Spawned but parent identity/ledger update not complete: the supervisor claims
  the starting job and records its own identity before starting a payload. Native
  reconciliation checks process identity; a missing supervisor with surviving or
  uncertain payload remains orphaned with custody/capacity preserved. Proven-dead
  payloads settle/release under existing native rules. If no claim occurred, the
  starting-job rule above applies. No wrapper death proves payload death, and no
  new exactly-once claim is introduced here.

**Review nits.** Opening an explicit/automatic/held crew park rechecks
`cancel_requested` under the home write lock, including a stale pre-cancellation
read, so it cannot add a park/child edge after cancellation's graph admission.
A resumed original is `cancel_requested`, not `stopped`. The one-bit graph
visibility signal is documented above. Unknown receiving work operations return
`unsupported`; originating `job_cancel` additionally translates a V6a peer's
closed-operation `unsupported_arguments` to `unsupported`, without fallback or a
second call. Malformed locally supplied arguments still fail before transport.

**Fixture evidence:** phase tests cover caller-owned commit/rollback with an
additional intent/audit, ledger compensation, no-spawn reconciliation, historical
replay, changed authority, failed insertion, commit-before-launch enforcement,
native spawn-failure cleanup and retry request/status/override parity. The park
regression supplies a stale pre-intent read and asserts refusal without park/job/
audit changes. Cancellation replay tests distinguish resumed work; a real linked
transport fixture emulates a V6a operation refusal.

Run-specific stage-1 evidence (managed job identities, incarnations, counts and
per-step results) is kept in task reports and managed job output, not this public
contract. Scripted subprocess fixtures do not establish native autonomous-agent
behavior, real-account interoperability or production deployment acceptance.

### V6b3 — guarded retry

`job_retry {ship,reference,idempotency_key}` requires `work:manage`; the exact V4
reference and bounded graph/custody/lineage visibility are identical to cancel.
No command, cwd, environment, route, approval, estimate or admission overrides
are accepted. The operator-open home supplies no exemption from native recovery
and launch charter checks. An old agent job may retry only while its task is
still owned by that agent's ship, even if the caller can see both the old and
new owners. This custody check runs under both home locks, after visibility,
and maps a visible native refusal to `execution_failed`.
An old job without `agent_actor` relaunches as Operator work; a native retry
by an agent would instead make the relaunched job that agent's work.
Hidden/missing/stale targets share `ship_not_allowed`.

Recovery lookup runs before fresh visibility and eligibility, under IMMEDIATE.
A new call reconstructs the native retry payload and prepares its workspace
outside the home lock. Under a second IMMEDIATE lock it repeats recovery and
visibility checks, revalidates native eligibility/original-route backoff and
payload, refuses a changed charter, then invokes the same native admission hook
as CLI retry. Admission records a new native starting/queued job (the intent),
an uncertain result and delegated metadata audit in the same home commit. Only
then is the prepared native launch consumed, once. Returned admission/audit
failures explicitly compensate shared capacity, including light, demandless
reservations that stale-reservation inspection omits. A commit error retains
capacity if the home row exists or rollback cannot be proven.

Replies are closed `work.action.v1` acknowledgments with stable `result_id`,
original `job_id`, `retry_job_id`, `applied:true` and `outcome`. `launch_requested`
means native launch follow-through returned, not current payload liveness or
success. A crash/launch/result/commit/release uncertainty is `outcome_unknown`.
Replay returns only the recorded outcome or uncertainty, never launches again;
finalization compares the original result ID. V6a pins, authority binding,
retention, caps, revocation cleanup and its admission/commit window still apply.

The existing native cross-database crash window remains: a process dying after
ledger reserve but before home commit can leave an orphan reservation. Light,
demandless orphan reservations are not enumerated by `stale_reservations`;
inspection is not an automatic cleanup guarantee. Explicit recovery requires
native resource recovery/manual verification. Home-committed but unspawned jobs
are reconciled, not automatically relaunched. Wrapper death never proves payload
death. An uncertain replay is not permission to retry with a new key.

For cancel/retry, the dedicated action gateway first receives a read-only
`capabilities` operation with the same delegated context and selected ship. A
versioned operation list lacking retry returns `unsupported` without mutation.
Only an explicit legacy probe refusal enables the old `unsupported_arguments`
compatibility mapping; an advertised operation's argument error stays an
argument error. There is no operator fallback or second mutation attempt.

MCP marks retry non-read-only, non-destructive and non-idempotent; the mandatory
key provides bounded outcome recovery, not unconditionally idempotent launch.
Run evidence belongs in task reports, not this public contract. Linux fixture
coverage does not claim macOS, account/voice acceptance, deployment or autonomous
native agent behavior.

### V6c — tightening-only shared resource configuration

`resource_configure {machine,ship?,changes,idempotency_key}` is a fixed scoped
v2/MCP action requiring existing `fleet:admin` **and an unrestricted all-ships
grant**. Every named list is refused, even when it includes all current ships.
No scope/preset/grant is expanded; granting this authority to a client is the
captain's own explicit step. Trusted v1 refuses the command. `machine` is a
configured handle (`local` means this core); optional ship selects that local
home's configured ledger, otherwise the machine default ledger is selected.
No path, endpoint, argv, force, provider route, estimate, reset or generic native
setting can be supplied.

The nonempty closed `changes` object accepts only `max_heavy_jobs` (1..1024),
`min_available_memory_bytes` and `min_free_disk_bytes` (unsigned byte counts).
Under the ledger IMMEDIATE lock, the native admission layer validates **the
whole patch before writing**: finite heavy caps may only fall/stay, an unbounded
cap may become finite, and memory/disk floors may only rise/stay. Null/default
reset and any relaxing field refuse the entire patch, never a partial apply.
A requested memory floor must be strictly below measured total host memory;
a disk floor must be strictly below measured total capacity on the configured
ledger filesystem and, when selected, the home filesystem (the smaller total
wins). These are total-capacity bounds, not free-space estimates or promises of
admission. Unmeasurable targets fail closed. Arbitrary future job/workspace
volumes are not inventoried here: native admission always checks the actual
filesystem. Only a job's own disk declaration exceeding that whole filesystem
is a permanent size error; mutable host/charter floor conflicts, including
floor + demand above total, are `Refused` waits, preserving queued jobs rather
than failing them on a policy change. No floor is lowered or bypassed.
An effective no-op still returns `applied:true, changed:false`. Loosening stays
a native operator CLI operation, with no delegated path. Native configure keeps
its existing validation/merge implementation and CLI behavior.

This changes policy, not work. Native charter shares, host caps, CPU/load/ramp,
memory and per-filesystem disk floors, demand estimates, queue fairness,
workspace/approval/deployment and provider backoff remain authoritative. Existing
holds (including those exceeding a newly tightened cap) and queue entries are
untouched. No launch, reconciliation, dequeuing, reservation release, process
probe or resource recovery is performed. Changing a cap promises no admission.

Only `fleet.resource-action.v1` can receive the request-bound context and fixed
`target`/`resource_configure` operations. The receiver independently requires
unrestricted admin authority and local ownership, never traverses links or
falls back to operator dispatch. `target` is **internal**, not a public command
or tool. It opens the existing ledger read-only, takes no write lock and returns
only an opaque identity derived from a random ledger origin and database file
identity, never a path/hostname hash. Native ledger initialization in admission
installs that origin plus bounded outcome/audit tables; a legacy or missing
ledger is refused, never initialized/migrated/repaired by a probe or action.
Action opens do not chmod or change journal mode. Host routing/probe/mutation
are fenced to the same #553 link incarnation; the receiver checks the target
again inside its ledger transaction. Home ownership/configuration lookup runs
before the shared ledger writer; file/origin identity is checked inside it
before effects and commit. This does not claim an atomic lock across home
configuration and ledger policy. A replaced link/ledger cannot accept the
same originating key as a new effect. No route is created or changed.

Originating global key pins bind operation, arguments, target identity, link
incarnation and admitted authority/visibility. Receiver settings, metadata audit
and result commit atomically in **one ledger transaction**, not a home/ledger
cross-store transaction. Matching replay returns the original acknowledgment
even after restart/response loss or subsequent policy tightening, never reapplies.
Conflicts refuse. Both originating pins and ledger results retain 24-hour retry
protection, bounded to 256 per grant and 4096 global/target ledger; in-window
saturation refuses new calls while matching replay remains possible. Target
result rows (including their duplicate audit column) expire together. A separate
`resource_policy_audit` retains the last 4096 policy changes independent of time
and result pruning, atomically with settings/results. It records operation,
grant/resource, opaque ledger, numeric before/after, result ID, key hash and
time; replay does not append a change. Normal native initialization installs
this table for older ledgers and preserves any still-retained result audits;
probes/actions never migrate it. Beyond retention there is no exactly-once promise;
do not use a fresh key to escape an uncertain old call.

Host credentials/resource/visibility are checked before action admission and
before release, including replay. As for V6a, revocation after admission cannot
undo an admitted ledger commit; release refusal hides the acknowledgment and
returns `outcome_unknown`, not no effect. Unclassified transport/commit/result
uncertainty is also `outcome_unknown`. Reverse-only/unsupported peers refuse;
there is no second mutation attempt or operator fallback.

Closed `resource.action.v1` acknowledgments contain operation, machine, nullable
ship, stable result ID, `applied`, `changed`, `outcome:configured` and core
provenance only. No raw admission report/readings are returned. Refusals use
bounded scope/ship/argument/execution/capacity/unsupported/uncertainty codes,
never native error prose or filesystem names. Policy refusals have definite,
value-free closed codes `resource_change_refused_relaxes`,
`resource_change_refused_exceeds_capacity`, or
`resource_change_refused_unsupported_setting` (the latter for native policy
validation; closed-schema errors still use argument refusal). The core, linked
gateway and MCP preserve these distinctions from internal execution faults. Atomic target audit records
grant/resource, opaque ledger, changed field names and numeric before/after,
result ID and key hash; no credentials, raw keys, paths or authored text. Writes
are limited to settings, bounded ledger results/audit, originating registry pins
and ordinary authorization/audit bookkeeping. Homes/jobs/mail/presence/panes and
operator snapshot caches are untouched.

Test map: `door::resources::tests` covers closed schema/authority, lock-free
noninitializing probes, mixed-patch atomicity, native tightening/CLI parity,
no-op/holds/queue preservation, audit rollback, historical replay, target changes
and bounded retention/capacity, measured disk/memory bounds, value-free refusal
classification, separately bounded policy audit surviving result pruning, native
migration and audit rollback. `door::idempotency::tests` checks replacement
ledger/link conflicts and cross-operation keys. `tests/fixtures/door_resources.rs`
uses real supervised v2 and linked gateway subprocesses for scope/raw bypass,
replay/restart, actual ledger replacement, receiver narrowing, unsupported/read
gateway refusal, link replacement/held reply and post-commit release revocation.
A real heavy supervisor is observed re-evaluating its native capacity queue after
the disk floor rises to the largest allowed value; it stays queued, not failed.
The MCP SDK suite exercises discovery, fixed schemas, annotations, value-free
errors and real tighten/refuse/replay in both protocol eras. Gate outcomes belong
in task reports, not this contract. Recovery, broad tuning parity, scoped bundled
voice, mail completeness, real-account acceptance and deployment remain separate.

### V6d — exact terminal-hold recovery over native permits

The delivered native/gateway/MCP contract and fixture map are maintained in
[native-reservation-recovery.md](native-reservation-recovery.md). The two fixed
scoped tools are `resource_recovery_reference {machine,ship,job_id}` (`fleet:read`
plus visibility) and `resource_recover {machine,ship,reference,approval_id,idempotency_key}`
(`fleet:admin` with null/unrestricted ships, independently checked at host and
receiver). Every named ship list refuses. References confer no authority;
clients/chat/voice/models never issue or confirm permits. Issuance remains a
fresh challenge at the target's controlling terminal.

Recovery deletes only one exact eligible terminal job's reservation. Native
home-then-ledger lock order, process absence checks, single-use consumption,
result and separate recovery audit transaction remain authoritative. No task/job
settlement, launch, retry, orphan cleanup, floor/route change or deployment is
reachable. Linked operations use only `fleet.resource-action.v1`, negotiate
recovery explicitly before mutation and pin the same link/target incarnation.
Older configure-only and reverse-only peers refuse without fallback. Global
originating pins and ledger results retain bounded 24-hour retry protection;
replay precedes deleted-hold lookup, but never skips current credential and
visibility checks. Transport/commit/malformed post-dispatch result or post-effect
release uncertainty is `outcome_unknown`, never permission to use a fresh key.
Closed recovery refusal codes and native result DTOs contain no process or
filesystem prose. Hidden/missing targets share `recovery_target_stale`.

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
