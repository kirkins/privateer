# Architecture

Privateer's flat structure distributes routine decisions while preserving explicit responsibility. A navigator is a conversational role, not the scheduler's exclusive control plane. A department may contact another department directly. Tasks retain an owner, a charter, status, and bounded delegation depth. A handoff requires the current owner's request and the recipient's acceptance.

## Runtime boundary

The Rust runtime handles durable state, process supervision, resource admission, and transport. SQLite stores charter/task state, facts, pending issues, approvals, and audits. A job runs under a recorded task and an admitted resource reservation. A detached supervisor manages its payload process group and records completion after the invoking CLI exits.

Multiple homes on one host share a configured resource ledger. This prevents independent departments from each assuming they can launch the host's last heavy-work slot. Process identity and surviving payloads matter more than a supervisor's liveness; uncertain ownership is preserved for explicit recovery.

Events are facts. Pending notifications are a projection: repeated facts concerning the same task and issue update one pending item. An acknowledgment names the sequence actually observed, so a later fact cannot be silently discarded. Messages name a recipient and a task rather than broadcasting to the whole fleet.

## Plugins and local configuration

Optional executable plugins use JSON lines over stdio. They receive a minimal task context and return a validated command specification; they cannot widen the charter. Harness choice, model choice, provider routing, and private gateway policy live outside the core.

Examples can demonstrate a generic native harness or a gateway-constrained Pi route. A particular account's endpoint, provider alias, allowed models, and credentials belong in private configuration, not an example checked into Git. An unavailable required route is an error, never permission to bypass it. New provider adapters do not require changes to task/event/job storage.

## Peer transport

`peer.put` stores a transport address: name, `ssh` or `local`, absolute executable/home paths, timeout, and response limit. `peer.call` sends one correlated RPC request and returns the remote envelope. Each remote command enforces that remote home's workflow rules. Native SSH supplies host verification and authentication.

SSH runs a remote shell even when invoked through a local argv array. Privateer validates endpoint tokens and POSIX-quotes every remote command argument. Stdin carries JSON; task contents never become shell commands. Request size, output size, diagnostics, and total call duration are bounded. A local transport exercises the identical RPC protocol without SSH credentials.

This milestone does not implement distributed consensus or cryptographic cross-host charter inheritance. Explicitly configured remote homes remain responsible for their own charters. A peer's address is not a superior role or authority credential. Cross-host authorization and custody protocols need further qualification before replacing a fleet.

## Delivery and deployment

Passing-head checks bind delivery evidence to the current artifact. Production approval binds a separate decision to an exact artifact and environment; that approval supplies authority for the single deployment without requiring broader standing deployment rights. These are workflow guards running under one OS identity. Hard isolation from production credentials requires a separately enforced broker or account boundary.

The first milestone has no live forge merge/deployment adapter and no automatic fleet migration. A passing runtime pilot establishes behavior of the machinery; native model-driven execution is a separate qualification step.
