# Opt-in MCP front door (scoped reads/writes, standalone OAuth, explicit remote HTTPS)

**Local-only by default. Disabled until the owner explicitly registers it.** This trusted
same-user plugin relays to scoped door v2; the Rust core authenticates every
request and authorizes every operation. It exposes the core standalone OAuth
server when configured, including an explicit remote HTTPS mode. It is not a
sandbox, deployment surface or complete captain parity implementation.
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
# Optional metadata-only diagnostics for an operator's connector test:
log_requests = false
```

`venv` may be omitted only when bootstrapped at `state/.venv`. No installation
happens at door startup. Unknown config keys (including `host`) refuse startup.
Additional keys: `bind` (default `127.0.0.1`), `public_url` (explicit HTTPS base,
no trailing slash/query/fragment), `trusted_proxies` (array of exact IPs or
private CIDRs, default empty). CIDRs must fit RFC 1918 IPv4 or IPv6 ULA space,
be /8 or narrower, and name a network without host bits. Public CIDRs and
`0.0.0.0/0` / `::/0` are refused; a pinned exact public proxy IP is still allowed.
A non-default bind requires both public_url and trusted_proxies. Host
and any supplied Origin must match configured strings exactly; wildcards and
duplicate security headers are refused. Missing Origin is allowed for nonbrowser
clients, not anonymous authentication. Forwarded headers are ignored unless the
peer IP is explicitly trusted, including loopback. Query-string credentials on
`/mcp` are refused. OAuth discovery/endpoints are described below and in the
[core OAuth contract](../../docs/door-oauth.md).

From the owner's terminal, not an agent/job session. **Run each confirmation
command on its own, wait for its prompt, and type the requested word before
running the next command. Do not paste a multi-command block:** queued lines
are discarded before confirmation (where supported), not executed for you.

```sh
privateer door add local-mcp --manifest /absolute/checkout/plugins/mcp/manifest.json --protocol 2 --config /absolute/private/mcp.toml
```

```sh
privateer door client issue local-mcp --name local-reader --scopes fleet:read,mail:read --ships example-ship
```

```sh
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
URLs require the explicit remote setup below, not a tunnel around these checks.

## Remote setup

**All linked machines must run the same Privateer build for delegated sends.**
Upgrade the sender, every receiving gateway, and every collector before relying
on linked MCP/ChatGPT sends. An older collector permanently refuses an unknown
delegated message kind (for example `unknown message kind delegated.send.v1`);
the sender's message is then lost and must be resent after the builds match.
A queued response is not delivery confirmation. See [linked upgrades](../../docs/releases.md#upgrading-linked-machines).

To expose a newly created ship to an already connected ChatGPT client, find its
grant with `privateer door client list DOOR`, then run
`privateer door client edit GRANT_ID --add-ships NEW_SHIP` and type `edit` at the
owner terminal prompt. Or deliberately use `--all-ships` for current and future
ships. Scopes can similarly be widened with `--add-scopes ship:message`.
The existing token uses the change on its next request: **no new OAuth client or
ChatGPT reconnect**. See [grant editing](../../docs/door-scoped.md#edit-a-connected-clients-grant-without-reconnecting)
for exact set/remove flags, restrictions and audit behavior.

This is an **operator recipe, not deployment approval**. The limited real-client
evidence is recorded under Tested clients below; it does not qualify every
proxy topology. A hosted connector needs a reachable public HTTPS hostname. The
sysadmin must provide:

1. A hostname, e.g. `mcp.example.com`, DNS to the proxy/tunnel, a valid public
   TLS certificate and HTTPS termination on port **443**. Do not expose the
   adapter's plaintext HTTP port to the Internet.
2. Routing of **`/mcp`, `/.well-known/oauth-*`, `/authorize`, `/login`,
   `/consent`, `/token`, `/revoke`** to the adapter's HTTP address/port. Preserve
   methods, query strings and `Host: mcp.example.com`; preserve Authorization,
   Content-Type and Cookie/Set-Cookie. Do not rewrite callback URLs or cache
   OAuth/pages/token responses. No `/openid-configuration` is supplied.
3. A proxy that **overwrites** `X-Forwarded-For` with one actual client IP and
   `X-Forwarded-Proto: https`. The immediate TCP peer must match an exact
   trusted_proxies IP or belong to an explicitly trusted private CIDR. Forwarded
   chains are deliberately not accepted: have
   the last-hop proxy validate any upstream chain itself, then supply one IP.
   The public Host and Origin lists must be explicit. Issuer/resource never
   derive from these headers.
4. **No response buffering or caching for SSE**, HTTP/1.1 or better upstream,
   read/send timeouts of at least **300 seconds** for idle compatibility streams,
   and a request body limit of **64 KiB** matching the adapter default. Retain
   the adapter's 10-second upload deadline; it is not a long-upload service.
5. A firewall restricting the adapter port to the intended proxy only when it
   is not loopback. A listener's bind address is not a firewall or TLS.

### Topology A: proxy or tunnel on the same host

Forward HTTPS to **`http://127.0.0.1:8767`**; leave the bind default. The proxy's
actual socket source is `127.0.0.1` in this example (if it uses `::1`, configure
that exact IP and an appropriate explicit bind instead). Use a dedicated public
hostname. Example private mode-0600 `mcp.toml`:

```toml
venv = "/absolute/private/mcp-venv"
port = 8767
bind = "127.0.0.1"
public_url = "https://mcp.example.com"
trusted_proxies = ["127.0.0.1"]
allowed_hosts = ["mcp.example.com"]
allowed_origins = ["https://mcp.example.com"]
max_body_bytes = 65536
max_in_flight = 16
```

For example, an nginx TLS virtual host (supply certificate/key separately) can
route all the listed paths to this upstream with these location settings; do
not forward unrelated hostnames or add a catch-all authentication bypass:

```nginx
# Inside the HTTPS server for mcp.example.com; certificate paths omitted.
client_max_body_size 64k;
location ~ ^(/mcp$|/\.well-known/oauth-[^/]+(/.*)?$|/(authorize|login|consent|token|revoke)$) {
    proxy_pass http://127.0.0.1:8767;
    proxy_http_version 1.1;
    proxy_set_header Host $http_host;
    proxy_set_header X-Forwarded-For $remote_addr;
    proxy_set_header X-Forwarded-Proto https;
    proxy_buffering off;
    proxy_cache off;
    proxy_read_timeout 300s;
    proxy_send_timeout 300s;
}
```

A tunnel must provide the same behavior and stable immediate-peer address. It
must not publish the adapter before core/config setup and explicit deployment
authorization. Trusting loopback means other local processes can forge source
assertions; same-user plugins are already trusted, not sandboxed.

### Topology B: proxy elsewhere, e.g. Kubernetes ingress reaching the host

Suppose the adapter host's private address is `192.0.2.20` and the ingress's
actual egress/source IP is `192.0.2.10` (documentation addresses; replace both).
Route the same public paths to **`http://192.0.2.20:8767`**. Change only:

```toml
bind = "192.0.2.20"
trusted_proxies = ["192.0.2.10"]
```

Keep the same public_url and Host/Origin lists. The host firewall must permit
TCP 8767 **only from `192.0.2.10`**, denying other sources, including other pods
or LAN clients. If source NAT/load balancing changes the peer, pin the exact
actual egress IPs or the smallest private CIDR reserved for the ingress pods;
never trust all interfaces or public ranges. The
proxy-to-adapter hop is plaintext: keep it on a trusted private network or an
authenticated encrypted tunnel. For untrusted transit, add hop encryption;
this plugin does not terminate TLS. Ensure ingress annotations/config disable
SSE buffering and set the above timeout/body/header behavior. In remote mode
the adapter also refuses untrusted non-loopback peers regardless of forwarded
headers. Unspecified bind addresses (`0.0.0.0`, `::`) are forbidden even with
remote configuration; use one explicit private interface address.

### Topology C: Kubernetes ingress on the same host

A host process is not a Kubernetes pod. Use a selector-less Service with a
manually managed EndpointSlice pointing to a **specific reachable private host
IP**, not a ClusterIP, pod IP, loopback or wildcard. For example, if the host's
pod-network gateway is `10.42.0.1` and ingress pods use `10.42.0.0/16` (illustrative
private values only), extend the Topology A config with:

```toml
bind = "10.42.0.1"
trusted_proxies = ["10.42.0.0/16"]
```

Keep `public_url = "https://mcp.example.com"`, the public Host/Origin lists,
port 8767 and `max_body_bytes = 65536`. Discover the real gateway and actual
last-hop pod source range with the sysadmin first; a routed private interface
(e.g. a WireGuard address) is also valid if pods can reach it. Prefer a dedicated
narrow ingress pod range (for example a /24) over the entire cluster CIDR.
**Every process/pod in a trusted range can assert client identity and HTTPS**;
this trust is not limited to pods named Traefik. A host firewall must admit
TCP 8767 only from the selected pod range, on the chosen destination IP, then
drop other sources. Illustrative rules to integrate into an existing nftables
input chain, in order (do not flush or replace the host's ruleset):

```text
ip daddr 10.42.0.1 ip saddr 10.42.0.0/16 tcp dport 8767 accept
ip daddr 10.42.0.1 tcp dport 8767 drop
```

For example, in the `default` namespace (replace namespace, host IP, hostname
and TLS secret with operator-provided values):

```yaml
apiVersion: v1
kind: Service
metadata:
  name: privateer-mcp
  namespace: default
spec:
  # No selector: the endpoint is a process on the host, not a selected pod.
  ports:
    - name: http
      port: 8767
      targetPort: 8767
      protocol: TCP
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata:
  name: privateer-mcp-host
  namespace: default
  labels:
    kubernetes.io/service-name: privateer-mcp
    endpointslice.kubernetes.io/managed-by: privateer-operator
addressType: IPv4
ports:
  - name: http
    port: 8767
    protocol: TCP
endpoints:
  - addresses: ["10.42.0.1"]
    conditions:
      ready: true
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: privateer-mcp
  namespace: default
  annotations:
    traefik.ingress.kubernetes.io/router.entrypoints: websecure
    traefik.ingress.kubernetes.io/router.tls: "true"
spec:
  ingressClassName: traefik
  tls:
    - hosts: ["mcp.example.com"]
      secretName: mcp-example-com-tls
  rules:
    - host: mcp.example.com
      http:
        paths:
          # A dedicated host: forward all paths unchanged, including /mcp,
          # /.well-known/oauth-*, /authorize, /login, /consent, /token, /revoke.
          # The adapter itself returns 404 for every other path.
          - path: /
            pathType: Prefix
            backend:
              service:
                name: privateer-mcp
                port:
                  name: http
```

An operator-managed selector-less Service plus legacy Endpoints can be used
instead on older clusters; do not let a selector controller overwrite the host
endpoint. The host process must be listening before testing pod reachability.

TLS terminates at Traefik; preserve Host and forward a **single sanitized**
`X-Forwarded-For` plus `X-Forwarded-Proto: https`. For direct public TLS at
Traefik, leave `forwardedHeaders.insecure` disabled and do not trust arbitrary
incoming forwarding headers. If another load balancer precedes Traefik, review
that chain explicitly: this adapter refuses comma-separated X-Forwarded-For,
so normalize it securely to one actual client IP at the last hop. Never replace
it with a constant shared IP merely to make discovery pass.

k3s ServiceLB and LoadBalancer Services with `externalTrafficPolicy: Cluster`
can rewrite client source addresses before ingress sees them. All clients then
share one source rate-limit budget. Prefer `externalTrafficPolicy: Local` on
the ingress's public LoadBalancer Service (not the selector-less backend above),
or a hostPort entrypoint that preserves source addresses. Coordinate local
endpoint placement/reachability with the sysadmin and verify that the sanitized
X-Forwarded-For reaching this adapter contains real client IPs.

Traefik streams by default: attach **no response-buffering/caching middleware**.
Review ingress and upstream timeouts so SSE can remain open for at least 300 s;
use a streaming-safe write timeout (300 s or unlimited) and idle timeout at least
300 s. Traefik entrypoint `transport.respondingTimeouts` can set `readTimeout`
10 s, `writeTimeout` 0 s and `idleTimeout` 300 s; apply such static settings only
to an appropriate entrypoint after reviewing their impact on other routes.
Keep ingress upload limits at 64 KiB if supported without response buffering;
the adapter independently enforces 64 KiB and a 10 s receive deadline. Provision
the TLS secret/certificate and DNS separately. This YAML is a reviewable recipe,
not a deployment action or evidence that a particular cluster is configured.

### Matching owner setup and ChatGPT pre-registration

From the owner terminal, after registering the door with the config as in
Bootstrap (use the same name consistently). **Run each confirmation command on
its own, finish its terminal prompt, then run the next; do not paste them as a block.**

```sh
privateer door oauth-client configure local-mcp --issuer https://mcp.example.com --resource https://mcp.example.com/mcp
```

```sh
privateer door oauth-client add local-mcp --name 'ChatGPT reader' --type confidential --redirect-uri https://chatgpt.com/connector_platform_oauth_redirect
```

```sh
privateer door start local-mcp
```

Confirm each CLI summary on the terminal. Copy the client ID and **once-shown
secret privately**, not into source/history/logs. If OAuth is unconfigured,
`door start` warns with the configure command (local-only bearer mode still
works). Remote startup refuses unavailable OAuth configuration/metadata or a core
issuer/resource mismatch and records that bounded, secret-free reason in `door.log`. `public_url` is the issuer;
resource is exactly public_url + `/mcp`. Paths in public_url are supported, but
then route the prefixed endpoint paths and root well-known path-insertion URLs
too (see the core contract); the root-host recipe above is simpler. Only the
resource-matching `/.well-known/oauth-protected-resource[/BASE]/mcp` location
is served, not root protected-resource metadata. AS metadata is at
`/.well-known/oauth-authorization-server[/BASE]`, with root AS discovery only
for a pathless issuer. No nonstandard base-path metadata alias is served.

The captain can then try ChatGPT custom connector / Developer mode using
`https://mcp.example.com/mcp`, the pre-registered confidential ID/secret and
**Token endpoint auth method: `client_secret_post`**, not `none`. Choose
**User-Defined OAuth Client**; DCR and CIMD are not needed. Untick scopes you
will not approve, rather than accepting the form's all-scopes selection.
Leave optional authorization/token URL fields **blank**
so discovery is used. It may request every advertised scope; core accepts the
request, but the owner still chooses only a minimal subset on the consent page.
No extra MCP tools are enabled by OAuth. For each browser transaction run exactly
its displayed `privateer door oauth-client login-code TRANSACTION`, inspect the
client/redirect/resource/requested scopes on the trusted terminal, enter that
one-time code, then approve only the needed scope subset and select ships using
the picker or additional space-separated names, or Deny. All defaults grant
nothing. Ship names appear only after successful owner login, never in discovery
or the pre-login page. **All ships (current and future)** is off by default;
checking it deliberately grants unrestricted ship visibility for the selected
scopes, including ships added later. The picker lists up to 1024 active local/cached linked
ship names; typed names remain available. The core accepts up to 1024 named ships
in total (picker plus typed names); larger selections get a clear error and can
be reduced and retried, or the owner can deliberately choose **All ships**.
Pages say remote HTTPS or local-only. The result redirect includes RFC 9207 `iss`.

The refresh-token family lasts **at most 30 days absolute**, not 30 days after
each use. Repeat the owner terminal login and consent monthly (also after
revocation or ambiguous refresh). This PR does not extend the lifetime. Future
Authelia/OIDC owner login (phase 4.4) is intended to make re-login cheap.

Record actual client/date, protocol and discovery/redirect results without
account details before extending interoperability claims. DCR, CIMD and OIDC are not implemented or advertised;
no optional OIDC discovery document masquerades as OAuth metadata. Rotate/remove
clients and manage their grants with the owner CLI described in the core docs.
Nothing here changes provider routes or authorizes deploying a production door.

### Gemini custom MCP app (captain confirmation pending)

In Gemini's custom MCP app setup, enter the MCP URL and the owner-pre-registered
Client ID. Under **Advanced settings**, filling in the Client ID reveals a
**Copy redirect URI** button. Copy that exact per-user URI privately; do not
publish its account identifier. It is shaped like
`https://oauth-redirect.googleusercontent.com/r/user_bound_custom-mcp-FAKE_ACCOUNT-mcp_example_com`.
Replace the registration's temporary redirect **without changing the Client ID**:

```sh
privateer door oauth-client edit CLIENT_ID --redirect-uri COPIED_URI
```

Run this alone from the owner terminal, review old/new redirects and type `edit`.
The argument replaces the whole redirect list (CSV, or a single JSON-array
argument); use the same validation as add. Existing grants/refresh families
survive, but pending sign-ins and unexchanged codes are invalidated. Start a
new connection from Gemini after editing. Review/approve only the needed scopes
and ships. Login has five minutes; successful login gives a new ten-minute
consent window. Expired pages ask you to retry from the client. Duplicate Approve
submissions replay the same callback for one minute, not a second authorization.
A missing token-request `resource` uses the audience bound at authorize; a
mismatched supplied audience still fails.

The 2026-10-26 connection exposed these fixes; successful post-fix Gemini
interoperability is **not yet confirmed**. Add a Tested clients row only after
the captain supplies discovery, login/consent, token and MCP call results (and
protocol/auth method), without private account details.

## Tested clients

| Client | Date (UTC) | MCP protocol | Registration path | Token endpoint auth |
| --- | --- | --- | --- | --- |
| ChatGPT, Add custom MCP server (captain pilot) | 2026-10-24 | 2026-07-28 | Owner-pre-registered confidential client; User-Defined OAuth Client | `client_secret_post` |

First real-client evidence: discovery from the `/mcp` URL found all endpoints;
ChatGPT reported DCR and CIMD unavailable, neither required. Owner login and
consent succeeded, followed by `/token` 200. `server/discover`, `tools/list` and
`tools/call` all returned 200. The approved grant was `fleet:read mail:read`,
restricted to named ships. ChatGPT showed seven read tools and `mail_ack` under
Write. No client build was recorded. This records that pilot, not automated
account testing or full captain parity; the new picker/diagnostics still need
a post-change real-client check.

## Troubleshooting

- **Remote adapter restarts with no listener:** read `doors/NAME/door.log`.
  `MCP stopped: remote OAuth configuration unavailable` means run
  `door oauth-client configure` with issuer exactly `public_url` and resource
  exactly `public_url` + `/mcp`, then start the door. A mismatch is reported
  separately. Run each confirmation command on its own; never paste a block.
- **Token/revoke exchange fails:** `privateer door client audit` includes
  `oauth.exchange_failure` request-ring rows with known client ID (or null),
  operation, error, fixed parameter and allowlisted grant type. Repeats coalesce
  by minute with a count. Secrets/codes/verifiers/tokens/callback URIs are never
  recorded. Ingress refusals before endpoint admission remain transport-only.
- **Sign-in expired:** restart the connection from the client. The owner login
  deadline is five minutes; login starts a fresh ten-minute consent window.
- **Confirmation fails:** type the exact expected action word only after the
  prompt. The mismatch reports expected/received known words; arbitrary input
  is withheld because a pasted line may contain a secret.
- **Chat says no actions are exposed, with no requests reaching the door:**
  a chat opened before linking finished can retain stale connector state.
  Finish linking, then start a **new chat**, for example via **Try in chat**.
- **Missing captain mail/waiting items:** inspect `mail_delivery.summary` and
  `warning` on fleet status/waiting, and selected-mailbox `coverage` on reader
  results. Delegated coverage uses fixed wording, not machine link names;
  reader coverage describes the selected mailbox, possibly on a peer, not the
  door's own links. This machine does not include items held only at a captain desk elsewhere;
  reverse-only links cannot be collected from here. This pilot had limited
  desk coverage. Coverage repair is phase 2.4c, not part of this setup change.

## Tools and coverage

Runtime visibility is an explicit scoped-v2 opt-in: pass `schema:"runtime.v1"`
to `fleet_status` or `fleet_ships`. With no arguments, legacy output is unchanged.
For one ship, `ship_runtime {ship,machine?}` selects the same closed `runtime.v1`
envelope without a schema argument. It requires `fleet:read`, returns only metadata
and uses the same coverage, census, freshness and authorization checks below.
Trusted v1 does not support this new command.
Optional `machine`/`ship` selectors require the opt-in and resolve only authorized
core directory handles (`local` for this core), never paths/endpoints. Runtime
`coverage.v1` has scope `runtime-domain`, per-machine/ship/facet sources, and
incomplete/unknown-domain markers. V2 queries callable configured links and labels
reused, cached and carried facts; directory-only/reverse-only activity is unknown.
V3 adds checked metadata-only worker/session observations. `working:true` requires
a matching live payload identity on an executing headless job or an explicit
identity-matched hosted/session busy turn; a supervisor or stored running state
alone is not execution. Active first-mate/crew/errand workers, command payloads,
uncertain jobs and live/idle/asleep/blocked/viewer/unknown sessions are distinct.
False requires complete fresh activity evidence plus a readable empty selected-ship
ledger and running dispatcher; otherwise it is `"unknown"`. Missing adapter IDs,
foreign namespaces and stale/cache/carry facts cannot fabricate current activity.
Counts name contributing sources and completeness, not a proved full fleet. Backlog is separate. No authored
progress/ask text or argv/cwd/process/binding/operator records are returned.
The runtime read writes no home/work/presence/mail or durable snapshot cache state;
V2's bounded process-memory cache and existing registry authorization/audit
bookkeeping remain. V3 calls only bounded host metadata methods, never screens,
transcripts or pane arrangement. Native writable initialization adds crew-pane job
incarnation evidence; observation never migrates homes. Unsupported versions
and selector-only legacy calls refuse. See the [V3 contract and limitations](../../docs/design/mcp-fleet-visibility.md#v3-delivered-contract-and-evidence), with V1/V2 history.

| Tool | One core v2 command | Scope |
| --- | --- | --- |
| `fleet_ships` | `ships {}` (legacy), or opt-in `ships {schema:"runtime.v1",machine?,ship?}` | `fleet:read` |
| `fleet_status` | `status {}` (legacy), or opt-in `status {schema:"runtime.v1",machine?,ship?}` | `fleet:read` |
| `ship_runtime` | `ship_runtime {ship,machine?}`; selected-ship `runtime.v1` metadata and coverage | `fleet:read` |
| `fleet_waiting` | `waiting {}` | `fleet:read` |
| `away_status` | `away {action:"status"}` | `fleet:read` |
| `mail_inbox` | `inbox {ship,limit?,continuation?}` | `mail:read` |
| `mail_show` | `show {ship,id}` | `mail:read` |
| `mail_thread` | `thread {ship,id,limit?,continuation?}` | `mail:read` |
| `mail_ack` | `ack {ship?,refs}` | `mail:read` |
| `send_message` | `send {to,body,kind:request\|notice,urgency?}` | `ship:message` |
| `approval_prepare` | `approval {mode:prepare,id,ship?}`; stores/replaces a bound local challenge | `approval:answer` |
| `approval_answer` | `approval {mode:answer,id,ship?,digest,version,action?\|option_id?,note?}` | `approval:answer`; only non-sensitive step asks |
| `reply` | `send {kind:reply,correlation,body}` | `ship:message`; stored approval-bearing asks also need `approval:answer` |
| `ask_liaison` | `send {kind:request,body}` | `ship:message` |
| `dismiss` | `dismiss {id}` | `work:manage` |
| `ship_jobs`, `job_inspect`, `ship_work` | Fixed selected-ship paged/detail reads with exact job/task references | `fleet:read`; authored text separately needs `mail:read` |
| `work_rank`, `work_enqueue`, `work_defer` | Same-named fixed scheduling actions with task reference and mandatory `idempotency_key` | `work:manage` |
| `job_cancel` | `job_cancel {ship,reference,idempotency_key}`: **stops work** through native exact-job cancellation; no PID/argv/route fields | `work:manage` |
| `job_retry` | `job_retry {ship,reference,idempotency_key}`: retry stopped unsuccessful work through native admission, without command/cwd/env/route/approval overrides | `work:manage` plus native recovery/launch authority |
| `away` | `away {action:start\|back}` | `captain:presence` |
| `send_file` | `file.put {to,name,content_file}` (adapter-private staging) | `file:transfer` |
| `get_report` | `report.read {report_id}` | `mail:read` |

Each result includes the unchanged core data as MCP `structuredContent`, also
rendered as JSON text for older clients, with core-authored stable IDs and request
provenance. Continuations are opaque core-owned handles. `mail_ack` writes only
this grant's served reader cursor; it is annotated non-destructive but not
strictly read-only or idempotent (repeat results can differ). Read tools are
annotated read-only, non-destructive and idempotent, except `approval_prepare`:
it reads an ask but stores/replaces a challenge and is **not idempotent**.
`approval_answer` is a non-read-only, non-destructive, non-idempotent write.
Write tools are
not read-only; dismiss, away and job_cancel are destructive (closing/stopping
work or changing fleet-wide presence/catch-up), sends are not. Their
static `idempotentHint` is conservatively false. Scheduling/cancellation require
a key, as does retry; other write keys remain optional. Exact cancellation uses the V4
`{origin,incarnation,ship,task_id,id}` reference. `cancel_requested` is not proof a
payload stopped, and `stopped` is the native terminal result for that job, not its
whole task. Surviving/uncertain descendants keep native custody and capacity.
Durable intent commits before signaling; uncertain crash/signal/result/release
returns `outcome_unknown`. Same-key replay recovers the historical result without
resending signals. Recovery lasts 24 hours, with caps of 256/grant and 4096 per
originating registry/target home. Linked cancellation uses only
`fleet.work-action.v1`, never operator fallback. `job_retry` uses the same exact
reference visibility and recovery bounds, and the same native admission as CLI
retry. Only failed/cancelled/timed-out jobs qualify. Intent, uncertain result and
audit commit before launch; `launch_requested` does not prove payload liveness
or success. Uncertainty returns `outcome_unknown`; same-key replay never launches
twice. Retry is non-destructive, non-read-only and non-idempotent. Do not use a
fresh key to escape uncertainty. Cancel/retry negotiate linked operation support;
a peer without retry support returns `unsupported`. See the [V6b contract](../../docs/design/mcp-fleet-visibility.md#v6b-delivered-contract-and-evidence). All tools have human
titles and `openWorldHint: false`: they access only this fleet's data. It never acknowledges the dashboard or another client.
See [reader coverage and bounds](../../docs/door-reader.md): coverage describes
only the selected mailbox, on this machine or a reachable linked machine, as
complete, unavailable (with retry) or unsupported; other machines are reported
as not queried, and reverse-only links are unavailable. Inventory can include
cached linked ships, not live remote status. Preserve core coverage/error fields rather than interpreting
local success as global observation. Coverage follows the versioned
[`coverage.v1` schema](../../docs/door-reader.md#coverage-schema-coveragev1):
`fleet_waiting` and `get_report` report scope `local`, digest acks
`operation-only`; `complete:true` is never fleet-wide. When a core omits
coverage, `scope`, `complete` or a required `other_machines`, the adapter shows
missing fields as `unreported`, `false` and `unknown` with a note, never as
covered. A missing scope forces `complete:false` even if the core claimed
otherwise; valid v1 `operation-only` results need no `other_machines` and pass
through unchanged. Waiting lists cut to 100 asks, reaching the per-home scan
limit, or skipping a missing visible home are incomplete with reasons. Mail reads (`mail_inbox`, `mail_show`,
`mail_thread`) and `fleet_waiting` also carry the existing `mail_delivery`
summary/warning and desk/link fields. A complete selected mailbox is **not**
complete captain mail coverage; items held only at a desk elsewhere are excluded.
For a linked mailbox, `mail_delivery` describes the machine serving that mailbox.

The new protected `capabilities {}` core v2 command needs a valid current token
but no additional scope; it returns only that client's scopes/ship limits plus
normal request provenance. Every HTTP request (including initialization,
discovery, GET/DELETE and each tool POST) relays the bearer token there. Nothing
validates or caches credentials in the plugin. Discovery filters against that
request's effective scopes, and each tool invocation separately rechecks in the
core. Hidden/unknown tools and authorization/route refusals share a nonrevealing
`not_found` tool error. Pull-mail paging refusals are explicit instead:
`continuation_invalid` (list again without a continuation) and
`reader_window_full` (acknowledge or list earlier inbox mail); see
`docs/door-reader.md`. An admitted core mail-size refusal instead returns an
audited `invalid_arguments` (`body`, `maxEnvelopeBytes`), not "ship not found".
For a visible tool only, schema validation failures return
`invalid_arguments` with a bounded field name and rule, never the supplied value.
The adapter reauthenticates to record `request.deny` with
`invalid_arguments tool=... field=... rule=...` in the core's bounded client audit;
rejected arguments and bearer tokens are never sent to that audit. Audit/core
unavailability returns `unavailable`, not an unaudited validation diagnosis.
Missing/invalid/revoked tokens get 401 plus `WWW-Authenticate: Bearer`.
OAuth metadata is advertised with RFC 9728 challenges; OIDC discovery remains
404. Core authentication-store unavailability returns 503, not an invalid-token
401. Malformed/missing Bearer syntax is rejected before taking an in-flight
slot or receiving a body. No file-download/admin/digest/sensitive-approval or
arbitrary core-dispatch tool exists.

### Arguments and context handoffs

Every write description lists its accepted fields. All tools keep
`additionalProperties: false`, including otherwise redundant `to`/`kind` on
`ask_liaison`: silently ignoring a recipient or kind could hide mistaken routing
or intent. Remove the extra field or choose `send_message` for an explicit
recipient/kind. Unknown/reserved fields (including `from`, `actor`, `auth`,
`address`, `token`, `resource`, raw `payload`, and `task_id`) remain refused.
The scoped core supports `urgency` on send (already exposed), not `task_id`;
`correlation` remains limited to `reply` to preserve its reply/approval rules.

`send_message`, `reply`, and `ask_liaison` accept **1–32000 characters** of body,
up from 4096 to fit ordinary context handoffs, with **at most 48000 UTF-8 bytes**
and **48000 bytes after JSON escaping** (excluding the surrounding quotes).
These separate byte budgets leave 17536 bytes of headroom under the core's
**65536-byte complete serialized mail/envelope limit** for metadata/provenance.
Exceeding either body budget returns an audited `invalid_arguments` naming
`body` and `maxUtf8Bytes` or `maxJsonBytes`, never its value. Exceptionally large
grant ships lists can still exhaust the envelope budget: a typed core size
refusal is audited and reported as `invalid_arguments` / `maxEnvelopeBytes`;
shorten the body or use a report reference. No exception text is inspected or
returned. The adapter's independent default HTTP limit is **65536 bytes** for the
whole request (configurable up to 1 MiB), except `send_file` below; exceeding it returns HTTP 413 before
tool validation, not `invalid_arguments`. Raising HTTP limits never raises the
core mail limit. For larger handoffs, the operator's existing `report.put` plus
report-reference mail path is the appropriate direction; MCP report upload and
arbitrary mail payload authority remain unavailable.

### Files and captain reports (phase 3.2b)

`send_file` accepts only `to`, `name`, `content` (canonical padded base64), and
optional `idempotency_key`. Decoded content is at most **8 MiB** (empty files
allowed), encoded content at most **11,184,812 characters**. Names are one
component, **1–128 UTF-8 bytes**, with no leading dot, slash, backslash or control
characters. Staging is bounded to **two active files / 16 MiB per grant** and
**64 MiB globally**. Random exclusive mode-0600 files live in this door's state
directory, are unlinked on success, failure and replay, and abandoned adapter
staging regular files and symlinks are removed on restart (reserved-name
directories and special files are skipped). Capacity/I/O failures return unavailable.
This is a trusted same-user adapter, not hostile-plugin containment.

The upload-only HTTP ceiling is **11,250,348 bytes**: base64 cap plus 64 KiB
JSON-RPC overhead. Protected requests authenticate with the core **before any
body receive or parsing**; invalid credentials get 401 without reading the body.
Only a grant holding `file:transfer` gets the larger ceiling. Each received chunk
is checked before buffering. Bodies above `max_body_bytes` must be exactly one
JSON-RPC `tools/call` for `send_file`; other methods/tools, batches and malformed
or recursive bodies return HTTP 413. Recursive JSON within the ordinary limit
returns 400. OAuth retains the ordinary limit. The SDK ceiling matches the
upload ceiling; it does not enlarge other tools' admission. A proxy's default
64 KiB limit must be explicitly increased to this envelope to support uploads;
that is operator configuration, not automatic deployment authorization.

Staging precedes core retry admission. The core authenticates and checks the
recipient, opens descriptor-relatively without following symlinks, verifies a
regular single-link file, and fingerprints its actual bytes. Retry identity
uses recipient/name/content digest, **not the random staging pathname**. The
same opened descriptor is rechecked and transferred, never reopened by path.
The durable key reserves before file effects; different bytes under the same
key are refused and uncertain outcomes are not retried automatically. Linked
uploads use the existing dedicated delegated file gateway or fail cleanly.

A success returns `file_id`, `name`, `bytes`, `ship`, `delivery_state:"stored"`,
`notified:false`, `completed:false` and provenance, **never a host path**. Stored
means the receiving home accepted the bytes, not notification or completed work.
Use the explicit two-step pattern:

1. `send_file` to store the file and retain its `file_id`.
2. `send_message` to the same ship, naming the ID (requires `ship:message`).
   The recipient runs `file.describe {file_id}` in its own home to obtain its
   readable path, name, bytes, stored sha256, sender and receipt time. This
   agent-only read checks row ownership, never searches another home, and is
   **not exposed as an MCP tool**. No optional note/body or automatic mail write
   is part of `send_file`.

`get_report {report_id}` requires `mail:read`. A stored report must have an
incoming captain-mail reference from its stored author, with both author and
local active home allowed by the current grant. Unpublished/foreign/unknown IDs
are indistinguishable. It returns at most **256 KiB UTF-8** content and explicit
local-only coverage (`scope:"local"`, `remote:"unavailable"`). Publication scans share a **250 ms**
cooperative SQLite VM deadline across allowed homes (checked every 1,000 VM
instructions), with a **25 ms** busy wait; exhaustion fails closed. This keeps
older publications readable without an arbitrary recent-row cutoff, but is not
an OS/filesystem I/O deadline or a bound on one SQLite JSON function invocation.
HTML is untrusted: sandbox or refuse rendering it. It neither acknowledges mail
nor creates served refs/advances a
cursor. The operator-only `report.get` remains unavailable to agents.

`list_files` and `get_file` are **deferred**: baseline files flow from captain to
ship, not the reverse. Safe captain downloads first need a separate ship file
publication contract with core-owned rows and immutable captain-mail references,
then grant-bound opaque handles and descriptor-safe download race tests. No path
in incoming mail becomes host-read permission. See the [design checkpoint](../../docs/design/mcp-file-transfer.md).

### Writes, retries and asynchronous delivery (phase 3.2a)

Each write (including the existing `mail_ack`) accepts optional `idempotency_key`: a nonempty string of at most
128 printable ASCII bytes (the core also accepts non-control UTF-8 keys up to
128 bytes). The MCP schema excludes non-ASCII and all control characters so its
character limit matches the byte limit. The Rust core's registry table
`door_write_results` binds grant + core command + hashed key to a SHA-256 hash
of normalized arguments and the original result (including provenance), or
execution denial. Errors proven to precede effects release the reservation so
the same key can be retried; generic/uncertain execution failures do not. Send
defaults (`kind`, `urgency`, empty payload) normalize before hashing. Aliases share the `send` command namespace, not independent
retry spaces. The authority/resource binding also enters the hash: a scope or
ship restriction edit conservatively refuses a cached retry, and authentication
and current required scopes are always checked first.

Records expire **24 hours after first admission**, pruned on keyed writes.
Limits are **256 records per grant, 4096 fleet-wide**, with at most **128 KiB
serialized result per record** (about 512 MiB result payload for grants counted
against the global cap, plus metadata and retained expired-grant records).
Unexpired records are never evicted to admit a new key; capacity returns `retry_capacity` without acting, explaining that nothing
ran and a call without a key still works (without retry protection). Revocation
and grant removal purge their records transactionally. Expired-grant records
are excluded from capacity counting but retained for the full retry window,
so renewing a grant cannot re-execute its used keys. Key hashes and argument
hashes persist, not raw arguments/tokens/keys. Results may contain mail text; registry privacy matters.
The command schema and HTTP body bounds apply before reservation.

An immediate registry transaction reserves the key before any effect; competing
clients cannot both execute. A matching completed retry returns the original
result even after adapter/core restart. Changed arguments fail closed. A crash
or result-save failure after reservation leaves **outcome unknown**, returned
as an `outcome_unknown` tool error that says not to resend and to check
`mail_show` or the conversation, refusing re-execution until retention expires;
this is at-most-once admission, not a claim of atomic distributed exactly-once
effects or guaranteed crash-result
recovery. Never work around uncertainty by inventing a fresh key. After the
retention window the key may act again. Without a key, **each call is a new
write**, including sends. `mail_ack` still touches only this grant's reader
cursor; with a key its original acknowledgment result is replayed without
advancing any cursor again.

Sends return `message_id` and `delivery_state`: `queued`, `delivered`, `held`
(or `failed` for a known terminal delivery refusal, `unknown` for an unrecognized
state). Delivered means accepted mail, **never that the recipient did the work**. A retried keyed response is an
immutable original snapshot, not a refreshed delivery check. Cached replies are
audited as `request.replay` / `cached`, not fresh execution. `mail_show
{ship,id}` can inspect outgoing captain conversation rows and their current
`delivery`, `state` and reply fields.

For a reverse-only target (`role=collected`), the result explicitly reports
`route: "reverse_collect"`, the destination `machine` and `ship`, the originating
`queued_on: "this machine"` (not this machine's link name), and, **only if the
grant has `mail:read` and access to that ship**, `outcome_notice_ship` (the local
captain conversation that will receive the notice). The destination machine is
named only for the grant-visible recipient ship. While pending, `delivery_summary` in both the
structured result and JSON text explains that the message is **queued in this
machine's delegated outbox, not delivered**. The destination collects it when
its dispatcher next connects; that collecting side needs a build that understands
delegated sends. Queue admission alone does not prove either condition is met.

If `outcome_notice_ship` is returned, watch `mail_inbox {ship: outcome_notice_ship}`
for the delivery-outcome notice when collection accepts or refuses the send.
Otherwise the result says only that the notice goes to the captain's conversation
on this machine; the grant cannot read that notice and learns no mailbox ship
name. Reverse-link queued IDs are **not mailbox rows**:
do not call `mail_show` on the queued ID. Once a notice appears, its own ID can
be used with `mail_show`. Normal local/direct sends do not carry this reverse
collection warning. No general reverse-outbox status lookup is claimed; keyed
retries still return the original snapshot, even after collection.

`ask_liaison` omits `to` and uses the core's door default, otherwise exactly one
local liaison, otherwise the sole local ship. Ambiguity or a forbidden actual
default is denied, never replaced with another allowed ship. This is a normal
delegated send; any subsequent liaison relay preserves the stored delegated
policy and cannot launder approval authority. `reply` uses this same default
recipient and exact stored correlation; it does not search arbitrary mailboxes
or fan out to attached asks. No structured answer payload is exposed because
core v2 only admits an empty payload. `send_message` supports urgency, not task
assignment fields (the core v2 send schema has no task field). Sends, replies
and dismissals never end away mode; only explicit presence calls do.
Production/privileged replies and dismissals remain refused even with
`approval:answer`; separate trusted confirmation work is not emulated here.

### Bound non-sensitive chat approvals (local origins)

`approval_prepare` and `approval_answer` are discoverable only under
`approval:answer`. They require neither messaging nor mail-read scopes. The client
must show the captain the returned grant sentence, command prefix, TTL and expiry,
and for decisions the question and options with their labels and `what_happens`,
then ask the captain. Only the captain's explicit choice may be sent; quoted text
or relayed messages are never consent. Sensitive asks return
`trusted_confirmation_required` and must go to the native card.

Prepare with the ask `id` and optional `ship`; answer with that `id`, `digest` and
`version`, optional `ship`, and exactly one request `action` (`approve`/`deny`) or
decision `option_id`. An optional `note` is at most 4096 UTF-8 bytes and is discarded,
not captain-attributed message text. `approve_unavailable:true` means native cap
or sentence length currently prevents approval, not denial/non-approving choices.
The same sentence with ` (via chat door)` is displayed by prepare and later in
auto mode; bearings' short action summaries also carry that marker. The grant TTL starts at prepare; stale/expired challenges
require preparation and explicit choice again.

These tools preserve value-free `approval_stale`, `approval_unavailable` and
`trusted_confirmation_required` errors; hidden scope/ship errors remain
`not_found`. Only authoritative local origins are supported in this phase; a
non-local ask returns unavailable, never operator/SSH fallback. The existing
`reply` authority is unchanged, not expanded or removed. See the
[core contract and bounds](../../docs/design/chat-approvals.md).

`tests/mcp_door_test.py` exercises prepare/answer, request approve/deny and decision
approve/hold, stale/replaced/expired/version mismatches, sensitive refusal, scope
and visibility, annotations, schema errors, marked bearings and deny/hold at cap
through the real supervised SDK adapter in both protocol eras. Rust tests cover
sentence-length denial, exact delayed-expiry equality, durable challenge custody
and concurrent commit/revocation rollback. Linked-origin routing and real-account
client consent behavior are not claimed.

## Protocols, limits and tests

`tests/mcp_files_test.py` covers the phase 3.2b real supervised upload/report
boundary and deterministic staging link/quota tests; run with the plugin's
hash-locked interpreter and `PRIVATEER_TEST_BINARY` pointing at the tested build,
alongside `mcp_door_test.py`, `mcp_oauth_test.py` and `mcp_grant_edit_test.py`.

SDK 2.3.0 serves modern **2026-07-28** `server/discover` with the
`Mcp-Protocol-Version` header and request-scoped metadata, and legacy
**2025-03-26**, **2025-06-18** and **2025-11-25** `initialize`/session paths.
Tests propose each revision through the real door, then list and call tools
with the negotiated version header using the SDK's public client APIs. Modern
requests need no session ID or initialize fallback. Modern GET returns 405;
legacy GET is left to the SDK (not blanket-rejected). Negotiation remains the
SDK's: an unsupported legacy initialize proposal negotiates its latest legacy
revision, not a blind echo; unsupported modern revisions and malformed metadata
are SDK protocol errors. This is isolated protocol evidence, not a ChatGPT
account interoperability claim.

Defaults: 64 KiB HTTP bodies (including chunked), with the upload-only envelope
exception above; protected bodies must be UTF-8 JSON with at most 64 nested
arrays/objects (quoted delimiters do not count). Excess nesting returns 400
within the ordinary byte limit, otherwise 413, independently of the interpreter's
recursion ceiling. A 10 s body receive deadline applies;
16 active HTTP requests with immediate 503 instead of an unbounded admission
queue, 15 s core response timeout, 32 pending core requests, 32 legacy sessions,
60 s legacy session idle timeout. Config allows 1–64 active requests and
1 KiB–1 MiB bodies. An idle legacy GET counts against the active limit. There
are no fleet push notifications; revoked idle streams cannot receive new fleet
data, and their next protected request fails. Failed authentication never parses
a body, so its request log has no MCP method metadata. Tokens stay in request-local
transit only; SDK session ownership uses the core's safe client ID, never a
credential. SDK/HTTP access logging and automatic telemetry middleware are
disabled to avoid untrusted-body/header diagnostics. By default only
allowlisted, bounded, secret-free startup/failure diagnostics go to the supervisor's rotating `door.log`; the core
retains its independently bounded redacted audit. Optional `log_requests = true`
adds one JSON line per HTTP request: method, path without query, MCP JSON-RPC
method, protocol-version header, Authorization presence (`yes`/`no`), status and
duration in milliseconds (through stream completion for SSE). No credential,
body, cookie, query, response data or client IP is logged. Methods/versions and
paths are allowlisted; unknown strings become `[other]` so even credential-shaped
metadata is not reflected. Unavailable fields are null, including status on a
pre-response disconnect. MCP method is unavailable on requests rejected before
bounded body receive; logging never reads extra bytes or bypasses admission. OAuth ingress additionally
limits each source per minute: authorize 32, login 20, consent 20, token 60,
revoke 60 and metadata 120. IPv6 sources share a /64 budget. The limiter holds
4096 source/operation keys; new keys at capacity share quarter-rate overflow
buckets per operation and IPv4 /24 or IPv6 /48 prefix. IPv4-mapped IPv6 is
normalized to IPv4 before grouping. The overflow table has at most 1024 active
prefix/operation keys; unseen prefixes fail closed when it fills, without
evicting existing clients or prefix budgets. A flood from one prefix cannot
drain a different prefix's overflow budget; a many-prefix flood can still exhaust
capacity. Both tables expire after one minute.
Source limits run before taking an in-flight slot or receiving the body.
Browser sessions share a 1024-entry cap: five minutes for login, a fresh ten
minutes for consent (fifteen total), plus one minute of same-session/CSRF callback
replay after completion, never beyond the fifteen-minute transaction cap. Restart requires starting a new browser flow. See the core contract for CSRF/cookie/CSP defenses.

```sh
sh plugins/mcp/bootstrap.sh /var/tmp/privateer-mcp-test-venv "$(command -v python3)"
cargo build --locked
PRIVATEER_TEST_BINARY="$PWD/target/debug/privateer" /var/tmp/privateer-mcp-test-venv/bin/python tests/mcp_door_test.py
PRIVATEER_TEST_BINARY="$PWD/target/debug/privateer" /var/tmp/privateer-mcp-test-venv/bin/python tests/mcp_oauth_test.py
PRIVATEER_TEST_BINARY="$PWD/target/debug/privateer" /var/tmp/privateer-mcp-test-venv/bin/python tests/mcp_grant_edit_test.py
```

The tests supervise real adapter subprocesses in an isolated temporary two-ship
fleet, issue unequal scoped grants with real controlling-terminal consent, and
drive the official SDK over HTTP. CI runs the locked bootstrap and suite on
Linux/macOS Python 3.12. Local Python 3.14 qualification and exact exits belong
in the task verification report; CI platform success is not asserted before CI
runs. No provider/account subprocess is started.

Later rounds still need ship-to-captain file publication, download handles and
explicitly mapped admin tools; broader parity reads remain separate. Sensitive approvals remain gated on separate trusted core
confirmation work (2.6). Full remote reader parity, automatic client registration and hostile-plugin
process isolation are separate unfinished phases.
