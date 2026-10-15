# Proposal: attended Herdr injection

Status: proposal for review; injection is not enabled. Boarding and automatic
presence bindings are implemented separately.

A live presence lease records an optional `binding`: the explicit socket path,
its device/inode incarnation, and the exact pane, terminal, workspace and tab
identities returned by a protocol-checked Herdr snapshot. A native session id,
when already observed, is pinned too. Refreshing the same holder preserves its
binding; a replacement holder captures its own context. Lease expiry or release
removes this binding from the dashboard. No labels establish ownership.

This is sufficient for navigation using the existing snapshot/recheck/focus
contract. It is not an atomic authorization to deliver input. In particular,
a new pane may be bound before Herdr has recognized its harness or native
session. Such a binding must not authorize injection.

Injection must be an explicit lease delivery mode, with exactly one presenter.
Do not enable it alongside a native attended-mail extension, mod or hook. An
unknown presenter or mode is not permission to inject.

For injection, extend the lease with an explicitly qualified native session
identity and expected harness. A runtime started in the bound pane must qualify
that identity against its attended child; inherited pane ids alone cannot prove
which agent occupies the terminal. Skip Claude Code: its hooks already present
mail. Keep `serve`'s attended guard before every headless wake, independently of
whether injection succeeds. Losing a binding must never create a competing
headless session while the lease is live.

For each candidate delivery, verify the live holder, lease expiry, socket
incarnation, pane/terminal/location, harness and pinned native session. Reject
unknown or blocked occupants. In a write transaction, claim the next bounded
batch by advancing the existing `presented_through` after comparing the holder
and previous cursor. Build the batch using exactly the same cursor rules as
hooks, including the mailbox acknowledgment cursor. Serialize injection with
hook presentation so neither presents the same batch twice. Never move
`mail.ack` on the captain's or agent's behalf.

The proposed cursor must survive expiry, holder replacement and explicit release.
Today `leave` deletes the presence row and its cursor. An injection implementation
must instead retain presentation progress transactionally in an inactive record,
clearing the live holder and binding, then reuse that cursor on the next lease.
This is a proposed prerequisite; this PR does not change release semantics.

The claim must precede the external effect to retain the existing *at most once
presentation* rule. A crash between claim and prompt can leave mail unpresented;
it remains durable and unacknowledged for an inbox read or a later headless wake.
An uncertain prompt outcome must not be retried. Exactly once delivery across
SQLite and an external terminal requires receiver deduplication with a durable
delivery key; the installed prompt API has no such contract.

The installed Herdr protocol 20 schema, inspected with `herdr api schema`, has
`agent.prompt {target,text,wait?}` and the corresponding
`herdr agent prompt <pane> <text>` command. It has no expected terminal/session
or socket-incarnation precondition. An agent can exit and be replaced between
verification and the prompt. Existing dashboard safety explicitly allows this
non-atomic check only for focus, never input. Therefore this proposal does not
enable injection on that API. It needs a conditional prompt operation or an
equivalent server-enforced occupant lock, plus confirmation of the native
session qualification contract, before implementation.

Qualify with isolated real subprocesses: simultaneous hook/inject claims,
arrival after a stale claim, lease replacement/expiry, restart after claim,
blocked sessions, pane and native-session replacement, socket replacement,
uncertain delivery, and continuing suppression of headless wakes. Tests against
a simulated socket cannot establish a real server's atomic delivery guarantee.
