# Opt-in local MCP front door (phase 3.1)

**Local-only. Disabled until the owner explicitly registers it.** This trusted
same-user plugin relays to scoped door v2; the Rust core authenticates every
request and authorizes every operation. It is not a sandbox, OAuth server,
remote endpoint, deployment surface or complete captain parity implementation.
It changes neither provider routes nor existing v1 doors. No account login is
needed for the isolated tests. Never give an assistant owner-issuance authority.

## Bootstrap and register

Requires `uv` and Python **3.12–3.14**. The official Python SDK is **mcp 2.3.0**
with **mcp-types 2.3.0**. `requirements.lock` is a universal pip-format lock:
all direct/transitive versions pinned, SHA-256 hashes, conditional platform
markers. Bootstrap uses `uv pip sync --require-hashes`, never system Python or
user site-packages. It fetches only from the package index using uv's default
headers; no project/operator identifying headers are added. Updating the lock
is a deliberate review, not part of startup:

```sh
uv pip compile plugins/mcp/pyproject.toml --universal --generate-hashes --python-version 3.12 --output-file plugins/mcp/requirements.lock
```

Choose a private absolute venv path, normally inside this door's state directory
(next to its registry, under `doors/NAME/state/.venv`). A different private path
is supported through `venv` below. Development/test venvs belong in the task
workspace or `/var/tmp`, never a live fleet. Bootstrap an empty environment:

```sh
sh plugins/mcp/bootstrap.sh /absolute/private/mcp-venv /absolute/path/to/python3
```

Create a private mode-0600 TOML config, replacing the venv path:

```toml
venv = "/absolute/private/mcp-venv"
port = 8767
allowed_hosts = ["127.0.0.1:8767"]
allowed_origins = ["http://127.0.0.1:8767"]
max_body_bytes = 65536
max_in_flight = 16
```

`venv` may be omitted only when bootstrapped at `state/.venv`. No installation
happens at door startup. Unknown config keys (including `host`, `public_url`,
proxy trust and remote modes) refuse startup. The bind address is always
`127.0.0.1`, not configurable in this phase. Host and any supplied Origin must
match the configured strings exactly; wildcards and duplicate security headers
are refused. Missing Origin is allowed for nonbrowser clients, not anonymous
authentication. No forwarded headers are trusted. Use the exact `/mcp` URL;
query-string credentials, OAuth discovery and alternative paths are unsupported.

From the owner's terminal, not an agent/job session:

```sh
privateer door add local-mcp --manifest /absolute/checkout/plugins/mcp/manifest.json --protocol 2 --config /absolute/private/mcp.toml
privateer door client issue local-mcp --name local-reader --scopes fleet:read,mail:read --ships example-ship
privateer door start local-mcp
```

Review the terminal's grant summary and type its requested confirmation. The
token is shown once **on the terminal**, never stdout. Omit `--ships` only when
fleet-wide visibility is deliberately intended. Registration refuses trusted
v1 for this manifest/adapter. Startup logs say **local-only**. No startup hook
automatically enables the plugin. Use `door client list`, `inspect`, `pause`,
`revoke`, `rotate` and `door stop local-mcp` to manage access/lifecycle; see
[scoped door](../../docs/door-scoped.md). Lost tokens require rotation.

## Local account clients: captain-run, untested with real accounts

These are configuration recipes, **not evidence of Claude Code or Codex account
interoperability**. The captain must run the actual clients and record their
build/protocol/result. Crews use only the official SDK client and isolated fleet.
Keep tokens out of checked-in config, shell history, logs and screenshots.

Claude Code (HTTP bearer header; `PRIVATEER_MCP_TOKEN` is set privately by the
captain before this command):

```sh
claude mcp add --transport http privateer http://127.0.0.1:8767/mcp --header "Authorization: Bearer ${PRIVATEER_MCP_TOKEN}"
```

That CLI may persist the expanded header in its private client configuration;
review its permissions and token custody. This is not an OAuth login command.

Codex, in the captain's private MCP server configuration:

```toml
[mcp_servers.privateer]
url = "http://127.0.0.1:8767/mcp"
bearer_token_env_var = "PRIVATEER_MCP_TOKEN"
```

Start Codex with that environment variable privately populated. Neither recipe
uses a real account during development/CI. Hosted clients and remote/public
URLs require phase 4 and explicit owner setup, not a tunnel around these checks.

## Tools and coverage

| Tool | One core v2 command | Scope |
| --- | --- | --- |
| `fleet_ships` | `ships {}` | `fleet:read` |
| `fleet_status` | `status {}` | `fleet:read` |
| `fleet_waiting` | `waiting {}` | `fleet:read` |
| `away_status` | `away {action:"status"}` | `fleet:read` |
| `mail_inbox` | `inbox {ship,limit?,continuation?}` | `mail:read` |
| `mail_show` | `show {ship,id}` | `mail:read` |
| `mail_thread` | `thread {ship,id,limit?,continuation?}` | `mail:read` |
| `mail_ack` | `ack {ship?,refs}` | `mail:read` |

Each result includes the unchanged core data as MCP `structuredContent`, also
rendered as JSON text for older clients, with core-authored stable IDs and request
provenance. Continuations are opaque core-owned handles. `mail_ack` writes only
this grant's served reader cursor; it is annotated non-destructive but not
strictly read-only. It never acknowledges the dashboard or another client.
See [reader coverage and bounds](../../docs/door-reader.md): coverage describes
only the selected mailbox, on this machine or a reachable linked machine, as
complete, unavailable (with retry) or unsupported; other machines are reported
as not queried, and reverse-only links are unavailable. Inventory can include
cached linked ships, not live remote status. Preserve core coverage/error fields rather than interpreting
local success as global observation.

The new protected `capabilities {}` core v2 command needs a valid current token
but no additional scope; it returns only that client's scopes/ship limits plus
normal request provenance. Every HTTP request (including initialization,
discovery, GET/DELETE and each tool POST) relays the bearer token there. Nothing
validates or caches credentials in the plugin. Discovery filters against that
request's effective scopes, and each tool invocation separately rechecks in the
core. Hidden/unknown tools and denied calls share a nonrevealing not-found tool
error. Missing/invalid/revoked tokens get 401 plus `WWW-Authenticate: Bearer`.
There is no OAuth metadata/challenge advertisement. No send/reply/dismiss,
files/admin/presence mutation/digest or arbitrary core-dispatch tool exists.

## Protocols, limits and tests

SDK 2.3.0 serves modern **2026-07-28** request-scoped discovery/POST metadata
and legacy **2025-11-25** initialize/session/optional GET stream separately.
Tests exercise both with the SDK's own client. Modern GET returns 405; legacy
GET is left to the SDK (not blanket-rejected). Unknown revisions and malformed
metadata are SDK protocol errors, not a custom framing implementation. Other
2025 revisions the SDK may implement are not separately qualified here.

Defaults: 64 KiB HTTP bodies (including chunked), 10 s body receive deadline,
16 active HTTP requests with immediate 503 instead of an unbounded admission
queue, 15 s core response timeout, 32 pending core requests, 32 legacy sessions,
60 s legacy session idle timeout. Config allows 1–64 active requests and
1 KiB–1 MiB bodies. An idle legacy GET counts against the active limit. There
are no fleet push notifications; revoked idle streams cannot receive new fleet
data, and their next protected request fails. Tokens stay in request-local
transit only; SDK session ownership uses the core's safe client ID, never a
credential. SDK/HTTP access logging and automatic telemetry middleware are
disabled to avoid untrusted-body/header diagnostics. Only constant local-only
startup/failure diagnostics go to the supervisor's rotating `door.log`; the core
retains its independently bounded redacted audit.

```sh
sh plugins/mcp/bootstrap.sh /var/tmp/privateer-mcp-test-venv "$(command -v python3)"
cargo build --locked
PRIVATEER_TEST_BINARY="$PWD/target/debug/privateer" /var/tmp/privateer-mcp-test-venv/bin/python tests/mcp_door_test.py
```

The tests supervise real adapter subprocesses in an isolated temporary two-ship
fleet, issue unequal scoped grants with real controlling-terminal consent, and
drive the official SDK over HTTP. CI runs the locked bootstrap and suite on
Linux/macOS Python 3.12. Local Python 3.14 qualification and exact exits belong
in the task verification report; CI platform success is not asserted before CI
runs. No provider/account subprocess is started.

Phase 3.2 still needs structured writes/files/admin/liaison, durable idempotency
and parity reads. Sensitive approvals remain gated on separate trusted core
confirmation work (2.6). Remote reader coverage, OAuth/remote enrollment and
hostile-plugin process isolation are separate unfinished phases.
