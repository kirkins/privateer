# Move transfer and cleanup

New move coordinators require `move.protocol {}` to return `{"transfer":1}`
from both machines **before export stops a dispatcher**. Upgrade both ends if
this fails. There is no silent fallback to the slow per-file transfer. Legacy
`move.read` and `move.write` remain available to old coordinators, but they do
not acquire the new batching/retry behavior merely by upgrading the receiver.

## Format and bounds

`move.read_batch {id, entries:[{path,offset,length}, ...]}` returns
`{chunks:[{path,offset,hex}, ...]}`. Send those chunks in
`move.write_batch {id,chunks}`. The coordinator verifies each returned range.

Each batch carries at most 256 KiB of raw data and 1,024 entries. Planning
budgets escaped JSON metadata as well as the doubled hex data, with a 768 KiB
serialized body ceiling, leaving room under the gateway's 1 MiB request-line
limit. Large files use multiple offset-addressed chunks; zero-byte files have
an explicit zero-length entry. Calls scale with bytes plus bounded metadata,
not two remote round trips for every tiny file. With one local side, only the
other side needs a remote call per batch.

The existing placement barrier still checks **every file's size and SHA-256**
before laying out the destination. Modes, export snapshots, project cloning,
activation, and release are unchanged.

## Retry and failure policy

Batch reads, batch writes, and rollback discard/abort get three attempts total,
with 250 ms then 500 ms backoff. The peer's normal per-call timeout still
applies (normally 30 seconds); project operations keep their existing longer
budget. Export also gets the 120-second slow-call budget; an export error
enters cleanup custody and retries abort, rather than leaving a stopped source
without an owner. An absent source export is treated as already aborted.
Retried writes overwrite identical offsets, never append. Target
writes serialize with discard and expiry through a registry write transaction,
so a late timed-out writer cannot recreate a discarded stage. A partial batch
is safe to replay, and file integrity is checked before placement.

We deliberately **discard rather than reuse a failed snapshot**. The source
restarts after failure and its state may change. Cleanup custody is recorded
before rollback, discard results are checked, and errors are surfaced rather
than ignored. If rollback cannot complete, the run remains `cleanup`. Running
the same move command again retries that cleanup and asks for one more run to
start a fresh snapshot. A successful discard retains a `discarded` tombstone;
late writes and repeated begin for that move are refused. Interrupted transfers
that never entered rollback may replay their batches against the same frozen
export; they do not skip files based merely on their presence. A target that
reports `placed` resumes at project cloning and activation without recopying or
replacing the verified folder.

Rollback probes target status before undo and again if discard fails. If the
target reports `finished`, the coordinator records `arrived` instead of aborting
the source; resuming the move releases the source. A resumed `cleanup` run also
checks for completed activation before undo. If that status cannot be checked,
cleanup retains custody and does not restart a possibly duplicated source.

Receiving stages expire after **24 hours without begin/write activity**.
Expiry runs lazily on fleet-registry opens, including normal dispatcher passes
and gateway requests. No process/registry activity means expiry waits until the
next open. Only `receiving` stages expire automatically; placed or active ships
are never removed on an age heuristic. Failures removing files are logged and
keep the row `receiving`, eligible for retry; they do not prevent the registry
from opening or other stale stages from expiring. Expiry uses the same lock as
writes and rechecks age after taking it. Deletion still holds the registry write
lock: this keeps failed deletions retryable without introducing a separate
fenced cleanup state, but a large stage may delay concurrent registry writers.

## Cleaning a known orphan on another machine

The **operator of the target machine**, not a crew agent on the source, runs
cleanup there. No new deployment or remote access is implied by these steps.
Use the registry associated with the original receiving gateway (its configured
`PRIVATEER_FLEET`, or the normal default). Obtain the **full** move ID, not just
an eight-character prefix, from its `moves` table, checking `role='import'` and
`state='receiving'`. For example, using the operator's actual registry path:

```sh
sqlite3 /path/to/fleet.sqlite \
  "SELECT id,ship,state,updated_ms FROM moves WHERE role='import';"
```

On the target, send the existing gateway command as a single JSON line, using
the confirmed full ID:

```sh
printf '%s\n' '{"id":"cleanup","command":"move.discard","args":{"id":"FULL-MOVE-ID"}}' \
  | PRIVATEER_FLEET=/path/to/fleet.sqlite privateer gateway
```

Require an `ok:true` response with `result.state:"discarded"`; do not infer
success from the wrapper's exit status. Repeating the request is safe. The
updated receiver removes the import directory before retaining its discarded
row; `move.status` then reports `discarded`. Verify that
`moves/FULL-MOVE-ID/import` beside that registry is gone. Do **not** use
`ship clean-moved`: that deletes a *released source copy*, not an import stage.

An old receiving orphan with a database row will also expire on the first
registry open after installing the updated build if already older than 24
hours. Directory-only debris with no row cannot be age-classified safely and
needs operator inspection; `move.discard` with its confirmed ID explicitly
removes such an import directory too. Never manually remove a placed or active
ship's folder as though it were an orphaned transfer.
