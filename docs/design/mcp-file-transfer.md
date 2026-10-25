# MCP phase 3.2b: bounded upload and captain reports

Design checkpoint for #452, agreed before implementation. This increment adds
`send_file` and `get_report`, not file downloads or administration.

## Authority and deliberate deferrals

The baseline `payload.files` contract is **captain to ship only**. Ship sends
are refused; linked non-captain attachments are stripped. Incoming path metadata
is not a safe file-publication contract. `list_files`/`get_file`, grant-bound
opaque handles and download races are deferred until a separate design provides
ship-authored core-owned file rows and immutable captain-mail references. Never
turn a path in incoming mail into permission to read the host.

`send_file(to,name,content,idempotency_key?)` requires `file:transfer` and an
allowed recipient. It relays scoped `file.put`, storing bytes without notifying
the recipient or claiming work completion. No note/body is accepted. Follow it
with `send_message` naming the file ID. The recipient uses a new read-only
`file.describe {file_id}` command, restricted to its own ship's rows in its own
home, to find the readable file. This command is not an MCP tool and has no
cross-home or gateway fallback.

`get_report(report_id)` relays a new door-only scoped read under `mail:read`.
Both the incoming captain-mail reference's sender and the stored report's
author must match and be allowed by the grant. Only local active allowed homes
are searched. No arbitrary `report.get` operator bypass or remote fan-out is
introduced. Reports retain their existing 256 KiB UTF-8 content cap; HTML is
untrusted content. No read acknowledges mail, creates served refs or advances a
mail cursor. All calls use fresh core grant admission and normal audit outcomes.

## Bounds and staging

- Upload: 8 MiB decoded; canonical padded base64, at most 11,184,812 characters.
- Envelope: 11,250,348 bytes, including JSON-RPC metadata. MCP ingress
  authenticates with the core before receiving any protected body bytes.
  Only a grant holding `file:transfer` earns that ceiling; all others keep the
  ordinary limit. Check each chunk before buffering. A body over the ordinary
  limit must then parse as exactly one `tools/call` of `send_file`. Batches,
  malformed/recursive JSON, OAuth and every other method/tool retain the
  ordinary limit. UTF-8 JSON is limited to 64 nested arrays/objects, checked
  before parsing independently of Python's recursion ceiling (quoted delimiters
  do not count). Excess nesting within the ordinary limit returns 400, otherwise
  413, not an uncaught exception. The SDK ceiling matches; the ordinary limit is
  not globally increased.
- Name: 1–128 UTF-8 bytes, one component, no slash/backslash, leading dot or
  control characters.
- Staging: at most two files / 16 MiB per grant and 64 MiB globally. Admission
  precedes file creation; release follows cleanup even on errors and replay.
  Random exclusive 0600 files are created descriptor-relatively in the door
  state directory with no symlink following. The adapter is trusted same-user
  code, not a sandbox. Process restart removes its own abandoned staging files
  and symlinks, but skips directories and special files in the reserved namespace.
- Report publication scans: a shared 250 ms cooperative SQLite VM deadline over
  all allowed homes, checked every 1,000 VM instructions, with a 25 ms busy wait.
  Exhaustion fails closed, not a partial report or a cursor update. This preserves
  older publications rather than imposing a recent-row cutoff; it does not bound
  OS/filesystem I/O or one SQLite JSON function invocation.

Stage first, then retry admission: fresh staging paths must not enter the
idempotency fingerprint. After core authentication and recipient checks, open
once through the existing descriptor-relative handoff, hash the actual bounded
bytes, and normalize `file.put` retries by target/name/content digest rather
than the source pathname. Reserve the durable key before transfer and pass the
same descriptor to execution. A replay still stages and cleans its file. A key
with different bytes is refused; an uncertain write never permits re-execution.
Core results expose only file ID, bounded metadata and `delivery_state:stored`,
not paths. Remote transfers reuse the dedicated delegated gateway, never fall
back to operator dispatch.

## Evidence required

Real supervised MCP tests cover scope/ship filtering, grant edits/revocation,
strict arguments, names/base64/caps, cleanup, duplicate/conflicting/restart keys,
large-body method/batch/malformed rejection, report reference/author checks and
unchanged cursors. Core tests cover descriptor rename/symlink/hard-link behavior,
recipient discovery ownership, and linked transfer success or clean failure.
Existing scoped door, OAuth, MCP, bytecode guard, formatting and clippy checks
remain the regression targets. No deployment is authorized by this design.
