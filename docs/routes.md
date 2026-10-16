# Route catalog

This catalog records the fleet's named model routes and the evidence behind
their use. The live source is the captain-owned
`/home/kirkins/.local/state/privateer/fleet.routes.json`; the payload below was
read with `privateer routes show` on 2026-10-16. Cost labels describe the
accounting class configured for a route, not a promise that a provider will be
available.

## Current live routes

| Route | Harness / provider / model | Cost | Retention | Guidance and qualification |
| --- | --- | --- | --- | --- |
| `luna` | `bundled:codex` / native Codex account / `gpt-6-luna` | `credits` | No zero-retention claim is recorded. | Cheap ChatGPT credits; **not free**. The live route note names it as the default implementation crew route. Use for low-cost implementation work. |
| `deepseek-flash` | `bundled:pi` / `opencode-go` / `deepseek-v4.1-flash` | `plan` | No zero-retention claim is recorded. | The live route note identifies the OpenCode Go plan ($16 per 5 hours, $40 per week). It is the first errand route and the second crew route. Use for routine errands and as crew fallback when `luna` is unavailable. |

These entries are present in the live routes file. This catalog does not infer
additional data-retention terms from their cost class or provider name.

## Proposed routes, not live

| Route | Harness / provider / model | Cost | Retention | Guidance and qualification |
| --- | --- | --- | --- | --- |
| `space-bunny-free` | `bundled:opencode` / OpenCode native route (no provider option) / `space-bunny-free` | `free` | Described as zero-retention; not verified in a successful live run. | **Pending qualification.** The free allowance is understood to be shared across free models and to reset at about 24 hours. Both recent paid and free OpenCode V1 live runs ended with upstream `UnknownError: Unexpected server error`; neither established successful turns or free-allowance exhaustion. Do not select it until those behaviors are qualified. |
| `agy` | `bundled:agy` / native AGY account / `gemini-3.8-flash-medium` | `free` (proposed accounting label) | No zero-retention claim is recorded. | **Pending integration.** AGY 1.4.0 exposed this model and an explicitly selected headless `agent.start` recorded a successful route. A separate fixture run returned a short response. This evidence does not qualify provider throttling/refusal behavior or fleet route integration. Keep it out of launch order until integration and accounting are reviewed. |

The OpenCode V1 evidence is in the private handoff
`opencode-v1-route/qualification.md`; the AGY evidence is in
`agy-gemini-qualification/report.md`. These are operator-home records, not
repository fixtures. The proposed JSON below includes both pending entries but
leaves the live `crew` and `errand` orders unchanged, so applying it will not
make either pending route selectable. The captain can add an entry to an order
after its qualification is complete.

## Selection and fallback

The captain owns the routes file. Crew and errands try their configured names
in order, skipping a route while its recorded provider backoff is active. A
failed launch is not automatically retried on another route; the next launch
can use the next available listed route. If every route in that kind's list is
backing off, Privateer refuses the launch and reports the route reasons. A
route omitted from a ship's list is not a fallback for that ship. First-mate
wakes continue to use their ship's configured first-mate route.

Use the current order as written: crew tries `luna`, then `deepseek-flash`;
errands try `deepseek-flash`, then `luna`. Pending routes in the proposal are
not fallbacks. Only the captain can apply a reviewed proposal with
`privateer routes set FILE.json`.

## Captain proposal

[`route-catalog.proposed.json`](route-catalog.proposed.json) is a complete
`routes set` payload based on the live file, with `space-bunny-free` and `agy`
added as documented proposals. It is not applied by this change. The captain
should review qualification and accounting, then decide whether and when to
add either name to a crew or errand order.
