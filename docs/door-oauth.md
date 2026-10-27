# Standalone OAuth authorization server (core and MCP HTTP adapter)

The core owns the state machine and owner CLI; the opt-in
[MCP plugin](../plugins/mcp/README.md) now relays HTTP discovery, login, consent,
token and revocation. The limited first real ChatGPT pilot is recorded in the
plugin's [Tested clients](../plugins/mcp/README.md#tested-clients); it is not a
claim of complete captain parity or qualification of every deployment topology.
Local-only remains the default; remote HTTPS requires explicit configuration,
matching core issuer/resource and operator-provided TLS/proxy reachability.
Registration is owner pre-registration only; DCR/CIMD and optional OIDC are not implemented. There is no external token
issuer mode. Existing trusted v1 and owner-issued local v2 bearer tokens retain
their behavior.

## Owner setup and lifecycle

These commands are outside dispatch/RPC/doors/gateways. Agent callers and job
sessions are refused, as for `door client`. Environment checks and a controlling
terminal are defense in depth, **not same-user process isolation**; the harness
must forbid agent lifecycle access. **Run each confirmation command on its own,
finish its prompt, then run the next. Never paste the command list as a block:**
type-ahead input is discarded before prompting on supported terminals.

```text
privateer door oauth-client configure DOOR --issuer https://issuer.example --resource https://resource.example/mcp
privateer door oauth-client add DOOR --name 'Untrusted display name' --redirect-uri https://client.example/callback [--type public|confidential]
privateer door oauth-client edit CLIENT_ID --redirect-uri https://client.example/new-callback
privateer door oauth-client list [DOOR]
privateer door oauth-client inspect CLIENT_ID
privateer door oauth-client rotate-secret CLIENT_ID
privateer door oauth-client remove CLIENT_ID
privateer door oauth-client login-code TRANSACTION
```

Configure, add, edit, rotate-secret, remove and login-code show the exact context on
`/dev/tty` and require typing the action. Mismatches name the expected word and
received known action word; arbitrary input is withheld to avoid echoing secrets. New confidential secrets and login
codes are shown **once on the terminal**, never stdout. List/inspect return no
secret hashes. Client IDs are stable random UUIDs. Public clients have no secret;
confidential clients support both `client_secret_post` and
`client_secret_basic`, always with PKCE. Registration gives **no grant**.

Redirect registration is deliberately conservative: ASCII HTTPS, or HTTP to
literal `127.0.0.1`, `[::1]` or `localhost`; no credentials, fragments, control
characters or backslashes. Non-loopback IPv6 literals, custom URI schemes and
IDNs are not supported in this increment. Multiple redirects use a CSV, or a
JSON array supplied as the single `--redirect-uri` argument when a URI contains
a comma. Authorization and exchange compare the registered bytes exactly;
there is no wildcard, path-prefix match or loopback-port substitution.

`edit ID --redirect-uri URI` replaces the **entire redirect list**, with the same
CSV/JSON-array syntax and validation as add (not repeated flags). It preserves
the client ID, secret, live grants and refresh families: changing a callback does
not change previously owner-approved authority. It invalidates outstanding login
transactions and **unexchanged** codes, so old callbacks cannot finish linking.
Consumed-code replay evidence is retained; replay still revokes the associated
family. The terminal shows old/new redirects and these effects and requires
`edit`; if the redirect list changes during confirmation, retry.

The issuer has no trailing slash or query. Issuer/resource configuration is
explicit and never derived from request or proxy headers. Changing either
requires removing existing applications first. Removing/replacing/downgrading
a door invalidates its OAuth applications and outstanding transactions as well
as its existing grants. Secret rotation invalidates outstanding transactions,
codes and token families. Removed registrations are tombstones, not reusable IDs.

## Core wire contract

The complete pre-auth allowlist is `protocol {}` plus these **exact** names:

- `oauth.authorize-begin`
- `oauth.login-submit`
- `oauth.consent-submit`
- `oauth.token`
- `oauth.revoke`
- `oauth.metadata`

No `oauth.*` or `meta.*` wildcard exists. These requests take no outer `auth`,
return no fleet inventory before owner login and cannot create a grant without
authenticated owner consent. They use the ordinary v2 envelope and return a structured `result`;
OAuth errors are `result.error` (fixed OAuth error strings, never raw internal
errors). The adapter must map these to the appropriate HTTP responses, including
OAuth challenges and no-store handling of credential responses. An outer
protocol/envelope denial remains the existing nonrevealing door error.

1. `oauth.metadata {}` returns RFC 8414 data, including issuer, endpoint URLs
   under that configured issuer, S256, `code`, authorization_code/refresh_token,
   auth methods and advertised scopes. Its `protected_resource` member is the
   separate RFC 9728 resource document. Do not publish unsupported registration
   endpoints. `authorization_response_iss_parameter_supported` is true.
2. `oauth.authorize-begin` takes `client_id`, `redirect_uri`, `resource`,
   `response_type: "code"`, nonempty `state`, `code_challenge_method: "S256"`,
   `code_challenge`, and optional space-separated `scope`. It returns a
   `transaction`, `csrf`, `expires_in` and `next: "login"`. Unknown scopes fail;
   **any subset of advertised scopes, including all of them, is accepted**.
3. The owner runs `login-code TRANSACTION`. The terminal shows that specific
   application's untrusted display name, client ID, exact redirect/resource and
   requested scopes, with empty default scopes/ships. The issued login code is
   bound to this transaction, never a general-purpose login or fleet credential.
4. `oauth.login-submit` takes `transaction`, `csrf`, `login_code` and `source`.
   The trusted adapter supplies a canonical network source, never an unvalidated
   forwarded header/client assertion. Login atomically consumes the code and
   returns the consent summary and a **new** `consent_token`; the original CSRF
   credential cannot approve consent. This new credential goes only to the
   owner's browser session, not the OAuth client. The same response includes
   `known_ships` (up to 1024 active registered ship names, sorted), only after the
   login code is accepted. A successful login also returns `expires_in` for a
   fresh ten-minute consent window, capped at fifteen minutes from authorize.
   No names are returned by metadata, authorize-begin,
   failed login or client redirects. The adapter must keep browser sessions/forms
   separate from authorization-client state.
5. `oauth.consent-submit` takes `transaction`, `consent_token`, an explicit
   `scopes` array and explicit `ships`. Optional `decision` is `approve` (default)
   or `deny`. Owner denial consumes the transaction, creates no grant, and returns
   a bound `redirect_uri`, `error: "access_denied"`, `state` and `iss`; unlike a
   validation error, this authenticated denial can be redirected. Default both
   approval arrays to `[]`: no authority
   and no visible ships. The owner chooses an explicit scope subset and ship
   names; `ships: null` means deliberately unrestricted ships. A bearer token
   cannot call this operation or approve expansion. Successful consent records
   an expiring authorization and returns `redirect_uri`, `code`, original `state`
   and RFC 9207 `iss`. The application-bound grant is created in **the existing
   `door_clients` store** only when that code is successfully exchanged. The
   adapter constructs/encodes the redirect response; never redirect on validation
   failure. No pre-auth fleet-list API is provided for a ship selector.
6. `oauth.token` takes `grant_type`, optional `resource`, client authentication, and either
   `code`, exact `redirect_uri`, `code_verifier`, or `refresh_token`. The returned
   `scope` is the **owner-approved subset**, not the requested set. A wrong
   verifier/redirect/client/resource never consumes a valid unused code. When
   `resource` is omitted, exchange/refresh use the code/family's bound audience;
   a different supplied audience is refused. Authorize still requires it. Replay
   of a consumed code revokes its issued token family, including refresh siblings;
   consumed-code evidence is retained for the full family lifetime.
7. `oauth.revoke` takes client authentication, `token` and optional
   `token_type_hint`. Unknown tokens, wrong-owner tokens and unknown hints do not
   disclose existence. Access-token revocation affects that token; refresh-token
   revocation invalidates the entire family and its access tokens (RFC 7009).

Client authentication is either `client_id` plus optional `client_secret`
(POST), or `client_secret_basic` containing **only the base64 credential payload**
from the HTTP Basic header. Basic's username/password are form-URL-decoded per
RFC 6749. Reject mixing the methods, including an extra POST `client_id` with
Basic. Public clients supply just their client ID and cannot use Basic or a
secret. There is no client_credentials/password/implicit grant.

Protected OAuth requests must include the exact external audience:

```json
{"type":"request","id":"r1","command":"ships","args":{},"auth":{"token":"opaque-access-token","resource":"https://resource.example/mcp"}}
```

`auth.resource` is a credential binding, not an identity assertion.
`args.resource` and all other reserved identities remain forbidden. OAuth access
tokens without a resource, with another resource or through another door fail
closed. Local owner-issued bearer tokens continue to work with `auth.token`
alone; an added `auth.resource` is refused, including `door:NAME` (unchanged
from the local-bearer contract). The internal immutable
delegated context retains `door:NAME` for existing machine gateway compatibility;
the external resource is checked at every origin admission against the token
family and current configured authority. Provenance distinguishes the stable
application (`client`) from the consent grant (`grant`); local issuance retains
its historical one-ID representation. The authenticated machine wire gains an
optional `application` UUID for OAuth contexts; older peers that refuse unknown
fields fail closed rather than downgrade. Access tokens are never forwarded.

## Durable records and security decisions

All tables are in the existing private fleet registry. SQLite immediate
transactions serialize login/consent/code exchange/refresh/revoke with credential
admission. No credential cache survives a request.

| Table | Purpose |
| --- | --- |
| `door_oauth_servers` | Owner-configured issuer and exact external resource per door |
| `door_oauth_apps` | Stable ID, door, untrusted name, exact redirects, client type, hashed secret, registration timestamps and removal tombstone |
| `door_oauth_transactions` | Application/door, immutable redirect/resource/requested scopes/state/issuer/S256 binding, hashed CSRF/login credentials, login status and expiry |
| `door_oauth_grants` | Application-to-existing-`door_clients` grant binding, with immutable external resource and issuer (preserved after config changes/expiry) |
| `door_oauth_codes` | Hashed code, application/preallocated grant ID, immutable authorization and ship binding, expiry and consumed flag; no live grant until exchange |
| `door_oauth_families` | Grant/application/door/resource, absolute family expiry and revocation |
| `door_oauth_access` | Hashed access token, family, expiry and per-token revocation; extension of the core hashed credential resolver, not a separate grant authority |
| `door_oauth_refresh` | Hashed refresh tokens, family and consumed flag; consumed hashes retained for the whole family lifetime |
| `door_oauth_failures` | Hashed per-client operation, transaction/login-code and door-wide login brake keys, window and attempt count |

Each exchanged consent creates a separate independently manageable grant; repeated consent
does not silently widen a previously issued token. Existing owner `door client`
list/inspect/edit/pause/resume/revoke/expiry operate on those grants; OAuth applications
are managed separately. The owner can [edit scopes or ships in place](door-scoped.md#edit-a-connected-clients-grant-without-reconnecting)
with `door client edit GRANT_ID`, including scopes beyond the application's
original request. This requires fresh owner terminal confirmation, not a bearer
request or a new OAuth client/ChatGPT connection. Existing access tokens use the
edited live grant on their next request; later refreshes report its current
scopes. Browser re-consent still creates a separate grant, not an in-place edit.
`door client rotate` is refused for OAuth grants: it
must not manufacture a long-lived local bearer or imply that changing a dummy
local hash invalidates OAuth access. Use refresh rotation, grant revocation or
application secret rotation. Application removal revokes all its grants. An OAuth
application cannot access the owner lifecycle CLI. Lifecycle audit records both
registration (explicitly no grant) and owner consent. This increment records
owner notifications as durable consent/lifecycle audit events, not an additional
mail delivery surface.

All secrets are opaque, using two OS-random UUID v4 values (244 random bits) with
a purpose-specific `pv_*_` prefix. Only SHA-256 hashes persist, with constant-time
secret checking via `subtle`. Indexed bearer lookup is not timing-hidden (as for
local issuance). PKCE uses SHA-256 and the reviewed `base64` crate; no bespoke
cryptographic primitive/codec is introduced.

- Owner login: **5 minutes** from authorize; the login code is also bounded by
  its parent transaction's expiry. Successful single-use login starts a fresh
  **10-minute consent window**, with an absolute transaction cap of **15 minutes**
  from authorize. Login cannot slide that window a second time.
- Authorization code: **60 seconds**, single use, PKCE S256 even for confidential
  clients. Exactly one concurrent successful exchange; a losing replay revokes
  the winner's family. Clients must not retry an ambiguously delivered exchange.
- Access token: **15 minutes**, checked against live grant, family, app, door and
  exact external audience on each request.
- Refresh family: **30 days absolute**, not sliding. Every refresh rotates;
  replay of any consumed refresh hash revokes the entire family. Two simultaneous
  uses have one successful rotation, then the losing replay revokes **including
  the winner's newly returned tokens**. Clients must serialize refresh and log
  in again after ambiguous refresh delivery. Revocation is not cancellation of
  already admitted effects. The owner must repeat terminal login and consent
  monthly; refresh does not extend this lifetime. Future Authelia/OIDC login
  (phase 4.4) should make re-login cheap; no lifetime change is made here.
- Login failures: **5 per transaction and owner-issued code** for its 5-minute
  lifetime. Reissuing a code resets only that code's budget. A bounded door-wide
  brake permits 1000 failed guesses per 5 minutes; it never blocks a correct
  code on a fresh transaction. A stranger's failures cannot lock out another
  owner transaction, even for the same application and source. Unknown/expired
  transactions fail before login; ingress DoS controls remain the HTTP adapter's
  responsibility (the core does not trust arbitrary source strings as keys).
- Begin work: **32 per client per minute**, with at most **32 pending transactions
  per client**. Revoke work: **60 per authenticated client per minute**. A public
  client ID alone proves nothing: unknown/wrong-owner token revokes use a separate
  door-wide 60/minute bucket; owned tokens use the client bucket. Failed client
  authentication has its own door-wide 60/minute bucket. A stranger cannot spend
  the legitimate public client's real revoke budget. Over-budget requests return
  `slow_down`; budgets commit on invalid payloads. The HTTP adapter additionally
  applies source limits.
- Core argument payload: **16 KiB**, except **192 KiB** for consent so up to
  **1024 named ships** (including maximum-length identifiers) can be approved.
  Named selections past 1024 return a clear error without consuming consent;
  reduce the selection or deliberately choose **All ships (current and future)**.
  This does not increase the adapter's separate HTTP body limit: submitting
  1024 maximum-length names over HTTP needs about 180 KB and exceeds its default
  64 KiB limit (413). Such long selections require an explicitly larger
  `max_body_bytes` (and matching proxy limit), or a smaller named selection.
  Pending transactions/unexchanged codes: 1024 each; registrations: 1024 including
  tombstones; live grants/family rows: 4096
  each; access and retained refresh records: **4096 each per application** across
  all its live families (at most 1024 registered application/tombstone rows);
  limiter keys: 4096. Revoked families do not count against token capacity;
  their access/refresh rows are reclaimed on the next bootstrap request. Revoke
  an old family before relinking if its retained-refresh budget is exhausted.
  Other applications keep minting. Consumed refresh hashes of live families
  are never evicted early to free capacity.
  Consumed codes remain bounded by the family cap, not the pending-code cap.
  Unexchanged authorizations expire without accumulating grants. Expired transient
  records prune on bootstrap activity. Capacity exhaustion fails closed, never
  evicts active credentials or consumed refresh evidence.
- Redacted audit uses the existing separate lifecycle/request rings (4096
  lifecycle rows and 1024 request rows per door). All pre-login events, including
  begin, no-op revoke, failed login and slow_down, coalesce by door/event/fixed
  reason/minute with a count; client IDs never enter these grouping keys.
  Failed token/revoke exchanges additionally write `oauth.exchange_failure` in
  the request ring: door, registry-known client ID (or null), timestamps/count,
  and a JSON reason with `operation` (`token`/`revoke`), OAuth `error`, fixed
  `parameter` (or `unspecified`/`unknown`) and allowlisted `grant_type`
  (`authorization_code`/`refresh_token`/`missing`/`other`). Repeats coalesce by
  door/client/reason/minute, including known clients; the ring stays at 1024.
  Only registered IDs are retained, never arbitrary supplied IDs or grant types.
  Adapter-admitted malformed token/revoke requests relay a refusal-only
  `transport_error` enum (`method`, `query`, `content_type`, `form`,
  `authorization`) for core auditing. Rejections before endpoint admission
  (body size, ingress/source limits, host/origin checks) remain transport-only;
  unavailable core storage cannot promise an audit write.
  Only an actual live-token revocation writes a revoke lifecycle row. Anonymous
  requests cannot evict consent, revocation or rotation lifecycle facts. No raw
  code, token, secret, authorization header,
  source or arbitrary arguments enter audit. Same-user tamper resistance is not
  claimed.

## Evidence and remaining gates

`src/door/oauth/tests.rs` covers the RFC 7636 vector, subset consent, exact
redirect/client/resource/issuer bindings, Basic and POST, code replay, expiry,
login/consent CSRF, single use and explicit owner denial, transaction/code failure
isolation, global-brake owner bypass, per-client work/pending limits, 15,000-call
anonymous audit floods, 4100 unexchanged consents, grant liveness, revocation,
refresh reuse, restart and two-connection exchange/refresh races.
`tests/fixtures/door_oauth.rs` exercises real owner PTYs, CLI, supervisor and
adapter subprocesses, restart mid-login, protected admission through the shared
credential resolver, subset scope responses, rotation/removal, agent/job refusal
and redaction. Existing local-bearer and v1 fixtures remain in `tests/door.rs`.

## HTTP adapter (phase 4.2)

The MCP plugin relays the six bootstrap commands without outer authentication;
protected MCP requests relay `auth.resource` for `pv_access_` OAuth credentials,
never retry without that binding, and preserve local owner-bearer behavior.
Issuer/resource never come from request headers. Remote startup fetches core
metadata and refuses unavailable or mismatching configuration, logging an
allowlisted, bounded reason without exception payloads/secrets. `door start`
warns when an MCP door has no configured OAuth authority, explaining that
`public_url` requires `door oauth-client configure`; local-only bearer mode
remains available. Local-only OAuth
requires configuring the core with `http://127.0.0.1:PORT` and that URL + `/mcp`.
Without it, local owner-bearer MCP still works; discovery returns unavailable.

At issuer-relative `/authorize` (GET), core validation must succeed before a
login page is rendered. It names the transaction and exact owner login-code
command. `/login` (POST) consumes that transaction's owner-issued code, then
redirects to `/consent` (GET). `/consent` (POST) sends explicit selected scopes
(default none), checked known ship names plus typed space-separated names
(default none), or explicit Deny. The unchecked **All ships (current and future)**
choice maps deliberately to `ships: null`, with a warning that newly added ships
will also be visible for the selected scopes. The picker appears only after
owner login. No pre-login ship inventory is exposed.
Only core-bound success/owner denial redirects to the client, with encoded
`code` or `error`, `state` and RFC 9207 `iss`. Validation errors never redirect.

`/token` and `/revoke` accept only POST form-urlencoded bodies; Basic and POST
client authentication are passed to the core without mixing. Unknown authorize,
token and revoke parameters are ignored after duplicate rejection, per RFC 6749.
Exchange and refresh may omit `resource` (the code/family's bound audience is
still checked); authorization requires it, and a different supplied audience
always fails. A supplied narrower refresh `scope` is ignored and the current
owner-approved grant scopes are reissued. Refresh itself never expands authority;
an owner terminal edit can change the live grant independently of refresh. Responses are
no-store; errors are JSON, with 400 normally, 401/Basic challenge for failed Basic
authentication, and 429 for `slow_down`. Protected MCP 401s advertise the exact
RFC 9728 resource metadata URL (and `invalid_token` when appropriate).

Metadata is relayed from `oauth.metadata` at the matching RFC 9728 resource-path
insertion location and RFC 8414 issuer-path-insertion location. Issuer paths
are supported (e.g. issuer `https://example.com/door` has discovery at
`/.well-known/oauth-authorization-server/door` and resource discovery at
`/.well-known/oauth-protected-resource/door/mcp`). The resource always ends in
`/mcp`, so root protected-resource discovery returns 404 rather than describing
a mismatched resource. Root AS metadata is served only for a pathless issuer;
no nonstandard base-path alias is served. There is deliberately **no
`/.well-known/openid-configuration`** (404): this is OAuth, not OIDC. There are
no discovery-supplied registration endpoints, DCR, CIMD or URL-fetch surface.

Pages use escaped server-rendered HTML and strict CSP (no external
resources, base URLs or framing), no-store and `Referrer-Policy: same-origin`.
This keeps browser form POST Origins intact; `Origin: null` remains forbidden.
Consent uses a fixed CSP-hashed script that records the clicked Approve/Deny
button in a click listener before submit, so browsers without
`SubmitEvent.submitter` (Safari before 15.4) still bind the owner's decision.
Implicit Enter submission fires a click on the default button, which the
listener records. A submit with no submitter and no recorded click (for example
`requestSubmit()` from an extension) adds no decision, and a POST that carries
no decision is denied, never approved. The script then disables the buttons,
without `unsafe-inline`. After consent, an escaped forwarding page uses meta
refresh/link navigation, not a form redirect; callback redirect chains therefore
do not require broadening `form-action 'self'`.
A random browser CSRF credential is separate from OAuth client state AND core
transaction credentials. It is bound to an HttpOnly SameSite=Lax cookie; remote
cookies are Secure with a `__Host-` prefix. Login rotates the form CSRF value.
The adapter holds at most 1024 browser sessions: five minutes for login, then
up to ten minutes for consent (fifteen total). Login renews the HttpOnly cookie.
Missing/expired sessions show **Sign-in expired** and ask the owner to retry from
the client. Successful consent/denial retains the bound forwarding result for
**60 seconds** — never beyond the fifteen-minute transaction deadline — with
its cookie and CSRF credential. Same-session, same-CSRF
repeat POSTs re-serve that result under a per-session async lock, never a second
core consent or code; another session/CSRF cannot replay it. Completed entries
share the 1024-session cap. Restart loses browser sessions and requires a new
authorize flow (core durable records expire normally). No credential or browser-session database is written by Python.

Per-source ingress limits per minute are authorize 32, login 20, consent 20,
token 60, revoke 60, metadata 120. Sources are canonical peer IPs unless the
peer matches an exact IP or private CIDR in `trusted_proxies`; that peer must
overwrite X-Forwarded-For with one IP and X-Forwarded-Proto with `https`.
CIDRs must be within RFC 1918 IPv4 or IPv6 ULA space and /8 or narrower, with
no host bits; public CIDRs and default routes are refused. Exact pinned IPs
(including a specific public proxy) remain supported. Trust the smallest actual
ingress range: every pod in it can forge client/HTTPS assertions. Chains/invalid
assertions fail closed.
Nothing trusts loopback implicitly. In remote mode untrusted non-loopback peers
are refused even with spoofed forwarding headers; unspecified binds (`0.0.0.0`,
`::`) are always forbidden. IPv6 sources share a /64 budget. The limiter has at
most 4096 active source/operation keys; at capacity new keys share quarter-rate
buckets per operation and coarse prefix (IPv4 /24, IPv6 /48). IPv4-mapped IPv6
is normalized before grouping. A single prefix cannot drain another prefix's
overflow budget. Overflow has at most 1024 active prefix/operation keys; new
prefixes fail closed if that table fills (a distributed many-prefix flood can
still exhaust it). Neither table evicts live budgets; both expire in one minute.
Source admission precedes the in-flight slot and body receive. Existing transport limits still apply (body 64 KiB default,
10-second receive deadline, 16 active requests, bounded core pending requests).
Query strings are at most 8 KiB and form parameters at most 128, except consent
forms allow 1040 fields for all 1024 picker ships plus scopes and controls.
Over-limit forms return a clear error and can be retried with a smaller selection;
duplicate security headers/parameters are refused. Responses, including SSE, stream
through without adapter buffering.

See the plugin's **Remote setup** for same-host loopback, remote proxy, and
same-host Kubernetes ingress topologies, exact paths, headers, firewall
requirements and confidential ChatGPT pre-registration. The Kubernetes recipe
binds an explicit private gateway/interface IP and uses a selector-less Service
plus EndpointSlice, a CIDR-trusted last hop, TLS ingress and streaming-safe
limits. This also supports a routed private host interface reachable from pods;
no listener, pod source range or production route is discovered automatically.
`tests/mcp_oauth_test.py` exercises a handwritten HTTP client and the pinned
MCP SDK OAuth client against a real supervised door in an isolated fleet,
plus configuration/limiter tests. These are not personal-account interoperability
or live TLS/ingress tests. The captain's 2026-10-24 ChatGPT connector pilot is
recorded separately in the plugin README; the new setup UI still needs a
post-change real-client check. Phase 4.3 tests confidential pre-registration with the ChatGPT
redirect, `client_secret_post`, requesting all advertised scopes, S256 and
RFC 9207 `iss` on success and owner-denial redirects; it is not an actual ChatGPT
connector test. The plugin README documents the qualified MCP negotiation paths
and optional allowlisted metadata-only request logging (off by default).
Optional core-verified OIDC remains phase 4.4. Same-user plugins remain trusted processes,
not OS-contained. None of this authorizes exposing or deploying a live door.
