# Architecture

Privateer's flat structure distributes routine decisions while preserving explicit responsibility. A navigator is a conversational role, not the scheduler's exclusive control plane. A department may contact another department directly. Tasks retain an owner, a charter, status, and bounded delegation depth. A handoff requires the current owner's request and the recipient's acceptance.

## Runtime boundary

The Rust runtime handles durable state, process supervision, resource admission, and transport. SQLite stores charter/task state, facts, pending issues, approvals, and audits. A job runs under a recorded task and an admitted resource reservation. A detached supervisor manages its payload process group and records completion after the invoking CLI exits.

Multiple homes on one host share a configured resource ledger, and admission decisions are made under its lock against the same readings, so independent departments cannot each assume the machine is free.

Heavy jobs, and any job that passes an `estimate` (`cpus`, `memory_bytes`), are admitted from live readings rather than a fixed slot count: the load average, CPU count and available memory, plus the estimates of jobs admitted within the last `ramp_secs` (default 60), whose use does not show in those readings yet. A job is admitted while load plus recent estimates stays below `max_load_per_cpu` (default 1.0) per CPU, and while available memory minus recent estimates and its own stays above `min_available_memory_bytes` (default 1 GiB). Heavy jobs without an estimate count as every CPU and 2 GiB (`heavy_estimate`), so two big builds cannot start together, while a later one is admitted once the first turns out to be light. `max_heavy_jobs` adds an optional host-wide slot cap and a charter's `max_heavy_jobs` an optional per-charter share. `resource.configure` changes these host-wide settings for every home, and is open to agents so they can tune the machine to their person's wishes, from using all of it to leaving room for the person's own work; `resource.status` shows the settings beside the live readings. Light jobs without an estimate are not subject to admission.

A job started with `wait_for_capacity: true` joins a host-wide, first-come-first-served queue in the ledger instead of being refused. Its detached supervisor waits there, re-checking cancellation and authority about once a second, and starts the payload once the job reaches the head of the queue and fits; `max_wait_secs` (default 3600) bounds the wait. Requests subject to admission cannot bypass a non-empty queue, and entries whose supervisor has gone are dropped. Nothing runs while a job is queued, so cancelling it, or losing its supervisor, simply ends it.

Admission governs work started through `job.start`. A harness started by `agent.start` is light by default, since it mostly waits on its model; its brief asks it to run builds and test suites that load the machine as heavy jobs with `wait_for_capacity`, so those are spaced out and queued. Work a harness runs directly in its own shell is outside admission. Process identity and surviving payloads matter more than a supervisor's liveness; uncertain ownership is preserved for explicit recovery. A job whose supervisor and recorded process group are both provably gone settles automatically: to its leader's recorded result when descendants outlived the leader, otherwise to `failed`. `resource.recover` explicitly releases a reservation no job can settle, such as one left by a crash between the ledger and home commits. It never releases a live supervisor or process group, and a reservation with no recorded payload identity requires `force: true`.

Events are facts. Pending notifications are a projection: repeated facts concerning the same task and issue update one pending item. An acknowledgment names the sequence actually observed, so a later fact cannot be silently discarded. Messages name a recipient and a task rather than broadcasting to the whole fleet.

History is kept until an operator prunes it. `history.prune` (`older_than_days` or `before_ms`, optionally `dry_run`) removes event facts older than the cutoff that no longer back a pending notification, audit rows older than the cutoff, and finished jobs whose reservations are released, together with their log directories. Pending notifications, including unread messages, and live or still-reserved jobs are kept. Pruned event and job ids may be reused afterwards. Nothing is deleted automatically; run it by hand or from cron.

## Plugins and local configuration

Optional executable plugins use JSON lines over stdio. They receive a minimal task context and return a validated command specification; they cannot widen the charter. Harness choice, model choice, provider routing, and private gateway policy live outside the core.

Examples can demonstrate a generic native harness or a gateway-constrained Pi route. A particular account's endpoint, provider alias, allowed models, and credentials belong in private configuration, not an example checked into Git. An unavailable required route is an error, never permission to bypass it. New provider adapters do not require changes to task/event/job storage.

## Peer transport

`peer.put` stores a transport address: name, `ssh` or `local`, absolute executable/home paths, timeout, and response limit. `peer.call` sends one correlated RPC request and returns the remote envelope. Each remote command enforces that remote home's workflow rules. Native SSH supplies host verification and authentication.

SSH runs a remote shell even when invoked through a local argv array. Privateer validates endpoint tokens and POSIX-quotes every remote command argument. Stdin carries JSON; task contents never become shell commands. Request size, output size, diagnostics, and total call duration are bounded. A local transport exercises the identical RPC protocol without SSH credentials.

This milestone does not implement distributed consensus or cryptographic cross-host charter inheritance. Explicitly configured remote homes remain responsible for their own charters. A peer's address is not a superior role or authority credential. Cross-host authorization and custody protocols need further qualification before replacing a fleet.

## Delivery and deployment

Passing-head checks bind delivery evidence to the current artifact. Production approval binds a separate decision to an exact artifact and environment and records an approver other than the task owner. That approval supplies authority for the single deployment without requiring broader standing deployment rights: the production `delivery.check` that allows it consumes it. These are workflow guards running under one OS identity.

## Operators and agents

Harnesses launched by `agent.start`, and any job an agent starts, run with `PRIVATEER_ROLE=agent` and `PRIVATEER_ACTOR` set to the task owner. Under that role, operator commands (`init`, `charter.put`, `charter.revoke`, `approval.grant`, `approval.consume`, `peer.put`, `resource.recover`) are refused, `actor`/`from` arguments must name the agent itself, and jobs can be started only for the agent's own tasks. `peer.call` passes the role to the remote home as `--role agent --actor NAME`, flags that can only narrow a caller, never widen it. This stops a confused or overeager agent from widening its charter or approving its own deployment. It is not a security boundary: an agent that deliberately clears its environment or edits the state database is outside it. Enforced separation needs a separate OS account or broker. Hard isolation from production credentials requires a separately enforced broker or account boundary.

The first milestone has no live forge merge/deployment adapter and no automatic fleet migration. A passing runtime pilot establishes behavior of the machinery; native model-driven execution is a separate qualification step.
