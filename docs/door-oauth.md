# Standalone core OAuth authorization server (phase 4.1)

This is the **core state machine and owner CLI**, not an HTTP server or a claim
of ChatGPT interoperability. The phase 4.2 adapter must supply HTTP endpoints,
HTTP status/challenge mapping, sanitized pages, Host/Origin/form defenses and
explicit local-only/remote configuration. Registration is owner pre-registration
only; DCR/CIMD and optional OIDC are not implemented. There is no external token
issuer mode. Existing trusted v1 and owner-issued local v2 bearer tokens retain
their behavior.

## Owner setup and lifecycle

These commands are outside dispatch/RPC/doors/gateways. Agent callers and job
sessions are refused, as for `door client`. Environment checks and a controlling
terminal are defense in depth, **not same-user process isolation**; the harness
must forbid agent lifecycle access.

```text
privateer door oauth-client configure DOOR --issuer https://issuer.example --resource https://resource.example/mcp
privateer door oauth-client add DOOR --name 'Untrusted display name' --redirect-uri https://client.example/callback [--type public|confidential]
privateer door oauth-client list [DOOR]
privateer door oauth-client inspect CLIENT_ID
privateer door oauth-client rotate-secret CLIENT_ID
privateer door oauth-client remove CLIENT_ID
privateer door oauth-client login-code TRANSACTION
```

Configure, add, rotate-secret, remove and login-code show the exact context on
`/dev/tty` and require typing the action. New confidential secrets and login
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
return no fleet inventory and cannot create a grant without authenticated owner
consent. They use the ordinary v2 envelope and return a structured `result`;
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
   owner's browser session, not the OAuth client. The adapter must keep browser
   sessions/forms separate from authorization-client state.
5. `oauth.consent-submit` takes `transaction`, `consent_token`, an explicit
   `scopes` array and explicit `ships`. Default both arrays to `[]`: no authority
   and no visible ships. The owner chooses an explicit scope subset and ship
   names; `ships: null` means deliberately unrestricted ships. A bearer token
   cannot call this operation or approve expansion. Successful consent creates
   an application-bound grant in **the existing `door_clients` store** and
   returns `redirect_uri`, `code`, original `state` and RFC 9207 `iss`. The
   adapter constructs/encodes the redirect response; never redirect on validation
   failure. No pre-auth fleet-list API is provided for a ship selector.
6. `oauth.token` takes `grant_type`, `resource`, client authentication, and either
   `code`, exact `redirect_uri`, `code_verifier`, or `refresh_token`. The returned
   `scope` is the **owner-approved subset**, not the requested set. A wrong
   verifier/redirect/client/resource never consumes a valid code.
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
alone (an optional audience must equal `door:NAME`). The internal immutable
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
| `door_oauth_grants` | Application-to-existing-`door_clients` grant binding |
| `door_oauth_codes` | Hashed code, application/grant, immutable authorization binding, expiry and consumed flag |
| `door_oauth_families` | Grant/application/door/resource, absolute family expiry and revocation |
| `door_oauth_access` | Hashed access token, family, expiry and per-token revocation; extension of the core hashed credential resolver, not a separate grant authority |
| `door_oauth_refresh` | Hashed refresh tokens, family and consumed flag; consumed hashes retained for the whole family lifetime |
| `door_oauth_failures` | Hashed per-application/per-source limiter keys, window and attempt count |

Each consent creates a separate independently manageable grant; repeated consent
does not silently widen a previously issued token. Existing owner `door client`
list/inspect/pause/resume/revoke/expiry operate on those grants; OAuth applications
are managed separately. `door client rotate` is refused for OAuth grants: it
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

- Authorization transaction and login code: at most **5 minutes**, with login
  code also bounded by its parent transaction's expiry.
- Authorization code: **60 seconds**, single use, PKCE S256 even for confidential
  clients. Exactly one concurrent successful exchange.
- Access token: **15 minutes**, checked against live grant, family, app, door and
  exact external audience on each request.
- Refresh family: **30 days absolute**, not sliding. Every refresh rotates;
  replay of any consumed refresh hash revokes the entire family. Two simultaneous
  uses have one successful rotation, then the losing replay revokes **including
  the winner's newly returned tokens**. Clients must serialize refresh and log
  in again after ambiguous refresh delivery. Revocation is not cancellation of
  already admitted effects.
- Login attempts: **5 per 5-minute window** per application and per source,
  counting successes as well as failures. Neither new transactions nor changing
  sources bypasses the application limit. Unknown/expired transactions fail
  before login; ingress DoS controls remain the HTTP adapter's responsibility.
- Core argument payload: **16 KiB**. Pending transactions/codes: 1024 each;
  registrations: 1024 including tombstones; grant/family rows: 4096 each; access
  and retained refresh records: 16384 each; limiter keys: 4096. Expired transient
  records prune on bootstrap activity. Capacity exhaustion fails closed, never
  evicts active credentials or consumed refresh evidence.
- Redacted audit uses the existing separate lifecycle/request rings (4096
  lifecycle rows and 1024 request rows per door). Failed login/request events
  cannot evict consent facts. No raw code, token, secret, authorization header,
  source or arbitrary arguments enter audit. Same-user tamper resistance is not
  claimed.

## Evidence and remaining gates

`src/door/oauth/tests.rs` covers the RFC 7636 vector, subset consent, exact
redirect/client/resource/issuer bindings, Basic and POST, code replay, expiry,
login/consent CSRF and single use, per-source/application limits, grant liveness,
revocation, refresh reuse, restart and two-connection exchange/refresh races.
`tests/fixtures/door_oauth.rs` exercises real owner PTYs, CLI, supervisor and
adapter subprocesses, restart mid-login, protected admission through the shared
credential resolver, subset scope responses, rotation/removal, agent/job refusal
and redaction. Existing local-bearer and v1 fixtures remain in `tests/door.rs`.

HTTP pages/endpoints, trusted browser session cookies/form defenses, metadata
publication, `WWW-Authenticate`, remote HTTPS setup, real-client traces and
registration compatibility remain phases 4.2/4.3. This state machine is not an
authorization to expose or deploy a live door. Optional core-verified OIDC remains
phase 4.4. Same-user plugins are still trusted processes, not OS-contained.
