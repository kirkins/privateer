# Architecture

Privateer's flat structure distributes routine decisions while preserving explicit responsibility. A navigator is a conversational role, not the scheduler's exclusive control plane. A department may contact another department directly. Tasks retain an owner, a charter, status, and bounded delegation depth. A handoff requires the current owner's request and the recipient's acceptance.

## Runtime boundary

The Rust runtime handles durable state, process supervision, resource admission, and transport. SQLite stores charter/task state, facts, pending issues, approvals, and audits. A job runs under a recorded task and an admitted resource reservation. A detached supervisor manages its payload process group and records completion after the invoking CLI exits.

Multiple homes on one host share a configured resource ledger. This prevents independent departments from each assuming they can launch the host's last heavy-work slot. Process identity and surviving payloads matter more than a supervisor's liveness; uncertain ownership is preserved for explicit recovery. A job whose supervisor and recorded process group are both provably gone settles automatically: to its leader's recorded result when descendants outlived the leader, otherwise to `failed`. `resource.recover` explicitly releases a reservation no job can settle, such as one left by a crash between the ledger and home commits. It never releases a live supervisor or process group, and a reservation with no recorded payload identity requires `force: true`.

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
