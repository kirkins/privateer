# Design: allowance-aware routes

Status: design only, for an Opus design review before any code. Task `privateer-allowance-aware-routes` (captain finding, notice 3ea9cf1b, 2026-10-18). Read with [crew park and resume](crew-park-resume.md) (a resume's route policy) and [architecture](../architecture.md). Names marked **new** do not exist yet; every other name was checked against the tree at `49501c5`.

## Problem

A Google Pro allowance (a daily allowance shared by an AGY route and a Gemini Flash route) renewed with headroom left and went unused. The captain listed it on the crew lists, after a free route. Three gaps let that happen:

1. **Fixed priority.** A launch takes the first listed route that is not backing off. An allowance that renews and is listed below a plan route gets only overflow. With two renewing allowances, only the first is drained.
2. **Daily-only reset model.** The only shape the captain can state is `allowance_resets_utc` (one UTC time of day), and only free routes use it. There is no rolling window (Claude's five hours) and no weekly window. A plan route with no stated reset retries on the 30 s to 15 min doubling. A free route with no known reset is probed hourly, doubling up to a day.
3. **Invisible idle allowances.** Bearings list routes in order and mark the ones backing off. Nothing says that an allowance is unused and about to renew.

## Where route choice happens today

- **The routes file.** `crew_routes::path` is the fleet registry path with extension `routes.json`. Its top-level keys are `routes`, the lists `crew`, `errand` and `first_mate`, and `ships.<ship>.<kind>`. A route holds `manifest`, `options`, `cost` (`free|plan|credits`), `note` and `allowance_resets_utc` (`HH:MM`, parsed by `crew_routes::utc_minute`). `crew_routes::validate` rejects unknown keys. `routes.set` (captain only, `crew_routes::set`) replaces the whole file atomically under the registry's write lock. `crew_routes::order` returns a ship's list for a kind: the ship's own list if it has one, otherwise the fleet's.
- **Crew launch.** `agent.start` (`src/lib.rs`, around line 259) calls `crew_routes::choose(store, ship, "crew", requested)`. A launch that passes a `manifest` is checked by `crew_routes::check` instead.
- **Errands.** `src/errand.rs` (around line 158) calls `crew_routes::choose(…, "errand", requested)`, falling back to the ship's own route.
- **First mates.** `crew_routes::first_mate_fallback` (called from `src/activation.rs` and `src/scheduling/delivery.rs`) leaves a first mate on its own route unless that route is blocked. Only then does it take the first available entry on the `first_mate` list.
- **Resume and retry.** `crew_routes::resume_route` (called from `crew::launch_resume`) keeps a parked crew on its launch route while that route is listed and clear. `crew_routes::retry_refusal` (from `jobs::dispatch`, `job.retry`) refuses a replay on a route that is no longer listed.
- **`choose`** walks the candidates in list order (only the named route, if `requested` is given) and returns the first one where `routes::refusal` is `None`. If every candidate is refused, it fails with each refusal listed.
- **Backoff.** On each tick (`activation::tick_with_delivery_budget`), `routes::scan` reads spawn jobs that finished since the home's cursor (`routes_scanned_ms`). A `succeeded` job clears its route. A failed job goes to `routes::classify`. That reads Pi `message_end`, the native `error` and `result` events, AGY `result`, and Claude's `rate_limit_event` (status `rejected`, with `rateLimitType` and `resetsAt`, through `routes::claude_limit`). It returns a `RouteFailure` with `error`, `reset_ms`, `allowance` and `account`. For a route in `crew_routes::free_allowances` (cost `free`), `routes::free_allowance` treats a rate limit or quota as a spent allowance and takes the reset from `allowance_resets_utc`, unless the throttle came within 15 minutes after that time. `routes::record` writes the host ledger table `model_routes(route, failures, until_ms, last_error, updated_ms)`. A stated reset holds the route until then, never shortened by the doubling. With no reset, a spent allowance is probed from `ALLOWANCE_PROBE_MS` (1 h) up to `FREE_ALLOWANCE_BACKOFF_MS` (1 day), and anything else doubles from `FIRST_BACKOFF_MS` up to `MAX_BACKOFF_MS`. Everything is capped at `MAX_STATED_RESET_MS` (8 days), and a reset read from text at `MAX_TEXT_RESET_MS` (2 days). Claude's `five_hour` and `seven_day` windows (`ACCOUNT_WINDOWS`) are written under `routes::account_key` (`harness/provider/*`). A success deletes both rows, so **no record of a past throttle or success survives**.
- **Bearings.** `context::bearings` prints `crew_routes::summary` (each list with cost, note and ` backing off`) and then one line per row from `routes::list`.

## Model

### Allowance shape (routes schema)

A route gains an optional **new** `allowance` object. It holds exactly one of these shapes:

| shape | fields | meaning |
|---|---|---|
| daily | `daily_utc: "HH:MM"` | renews at that UTC time each day; whatever is unused then is lost |
| rolling | `window_hours: 1..=168`, optional `weekly_utc: "Mon HH:MM"` | a window opens at the first use after the previous one closed and reopens `window_hours` later; an optional weekly cap renews at a fixed UTC day and time |
| none | (no `allowance`) | carry-over: a plan or credits route, or a free route the captain has not described |

A route may also hold a **new** `tier: 1..=9` (1 is strongest, default 1). It sits beside `allowance` on the route, not inside it, because it describes quality, not the allowance.

Backward compatibility:

- Files without `allowance` validate and behave exactly as today. `allowance_resets_utc` keeps its meaning for backoff (`free_allowance`'s reset) on free routes, and it does **not** opt a route into reordering (below). A route may not carry both `allowance_resets_utc` and `allowance`; `validate` refuses that combination with a message telling the captain to move the time into `allowance.daily_utc`.
- Unknown keys inside `allowance` are refused, the same way the route object refuses them.
- `routes.show` and `routes.set` round-trip the object unchanged. Only the captain writes this file. This task does not touch the captain's files; it only teaches the runtime to read the shape.

`crew_routes::free_allowances` becomes the **new** `crew_routes::allowances`: route id → `Allowance { shape, cost, tier }` for every route that states a shape or is free. A free route with only `allowance_resets_utc` maps to a daily shape that is flagged legacy, so `scan` behaves exactly as before for it.

### Per-route tracking (host ledger)

A **new** table in the host ledger, created by `routes::init` beside `model_routes`:

```
route_usage(route TEXT PRIMARY KEY, last_succeeded_ms INTEGER, last_throttled_ms INTEGER,
  last_throttle TEXT, window_started_ms INTEGER, capped_until_ms INTEGER, updated_ms INTEGER NOT NULL)
```

- It is written by `routes::record`, in the same statement batch, inside one transaction (today `record` runs separate statements). A success sets `last_succeeded_ms`. A throttle sets `last_throttled_ms` and `last_throttle`. Rows are never deleted on success, which is what the "last" facts need. Clearing the backoff is still the `model_routes` delete.
- `window_started_ms` (rolling shapes only): a success whose job's `created_ms` is at or after `window_started_ms + window_hours` (or with no window yet) starts a window at that `created_ms`. The window starts at launch, not at the end of the run, because that is when the provider begins counting. A Claude stream that reports `rate_limit_event` with status `allowed` and a `resetsAt` for `five_hour` gives the window's exact end. The **new** `routes::classify_usage` reads it on success and stores `window_started_ms = resetsAt − window_hours`, so the declared length only fills in when the provider says nothing.
- `capped_until_ms` is set when a throttle is judged a spent allowance (below). It is what bearings and refusals call "capped", as distinct from "backing off" (an outage or a plain throttle). `model_routes.until_ms` still drives `routes::blocked`, so every existing check (`choose`, `resume_route`, `first_mate_fallback`, delivery holds, `job.retry`) honours a cap without change.
- Keys follow `record`: the route id, or `routes::account_key` for an account window. A success writes both the route row and its account row, mirroring the delete in `record`.
- The ledger is host-wide, so every home on the machine sees the same usage, as it does for backoff. It is bounded at 32 routes by `validate`, plus one account row each.

### How a capped route is skipped until its window reopens

The **new** `routes::allowance_spent(failure, allowance, usage, now)` generalises `free_allowance` to any route with a shape:

- **When a throttle counts as spent.** Free routes keep today's rule: any rate limit or quota means the allowance is spent, because OpenCode's free tier says only "Rate limit exceeded". A shaped plan or credits route is capped only on **allowance wording** (`quota`, `usage limit`, `allowance`, `exhausted`, `hit your limit`, `limit reached`), on Claude's `rejected` window, or on the **third** consecutive plain throttle with no success in between (`model_routes.failures >= 3`). A per-minute rate limit on a paid route therefore costs 30 s, 60 s, then 120 s on the doubling, not a day. Outages (`5xx`, connection errors, `timed out`, `overloaded`) are never a cap. They stay on the short doubling, as for free routes today.
- **How long a cap lasts.** The provider's stated reset wins, as today, and the doubling only raises it. If no reset is stated, the reopen time is computed from the shape:
  - daily: the next `daily_utc`. The existing guard still applies: a throttle within 15 minutes after the declared time leaves the route to the hourly probes, and bearings flag the declared time as suspect.
  - rolling: `window_started_ms + window_hours` when that is still in the future, otherwise `now + window_hours`.
  - weekly: used when the throttle says so (Claude `seven_day`, or text containing `week`), or when a rolling route is capped again with no success since its previous cap reopened. Then the route is held until the next `weekly_utc`. Because the captain declared that time, it is trusted up to `MAX_STATED_RESET_MS`, not capped at the two days allowed for text. Without `weekly_utc`, it falls back to today's probes.
- `record` writes `until_ms` to that reopen time, which replaces the doubling for the cap. Reopening needs no timer: once `until_ms` passes, `blocked` is clear and the next `choose` sees the route. The first success afterwards clears `model_routes` and starts a new window.

### The ordering rule

`choose` keeps its signature and its explicit-route path. Only the order of the walk changes when `requested` is `None`:

1. **Allowlist.** Candidates are the ship's list for the kind (`order`), exactly as today. Nothing unlisted is ever chosen, and a ship's own list still replaces the fleet's.
2. **Available.** A route is skipped while `routes::refusal` is `Some`, whether capped or backing off, exactly as today.
3. **Quality first.** Group by `tier`, best first. A use-it-or-lose-it route is never promoted past a route of a better tier. Every route defaults to tier 1, so in an untiered list only declared shapes move, and only among themselves and ahead of carry-over routes. That is why a weak route the captain gives a shape must also be given a worse tier (see Open questions 1).
4. **Within a tier.** First come use-it-or-lose-it routes: shapes declared through `allowance` (not legacy `allowance_resets_utc`) that are available and have headroom, ordered by **nearest loss**. Carry-over routes follow in list order.
   - daily: the next `daily_utc`.
   - rolling, window open: the window's end.
   - rolling, idle (no open window): `now + window_hours`, the soonest an unused hour starts being lost. With a `weekly_utc` declared, the key is the earlier of that and the weekly reset.
5. **Tiebreak.** Equal keys (within a minute) and every carry-over route keep the captain's list order.

The **new** pure function `crew_routes::rank(names, policy, usage, now) -> Vec<String>` does steps 3 to 5. `choose` then walks its output with the existing refusal loop, so the refusal text and the "every route is unavailable" error are unchanged. Taking `now` as a parameter lets tests hold the clock, which the store cannot (`Store::now_ms` reads the system clock).

**Explicit route honoured.** `requested` bypasses ranking. It is still refused if unlisted (allowlist) or capped (refusal), as today, and the refusal names the reopen time.

**Crew, errands, first mates, resumes.** Crew and errand lists both rank, because each kind has its own list and the captain can give errands, which are short single-turn verdicts, different tiers. `first_mate_fallback` does not rank. A first mate stays on its own route, and its fallback list stays first-available, because moving a first mate's conversation for allowance reasons breaks its warm session. `resume_route` does not rank either: a parked crew resumes on its launch route while that route is listed and clear, since keeping the session beats draining. A resume that falls back to a fresh crew goes through `choose` and is ranked. `check` and `retry_refusal` are unchanged.

### Bearings

`crew_routes::summary` labels gain the shape and its state, and the tier when it is not 1:

```
Routes (crew: claude-plan (plan; 5h window, open, closes in 1h20m), flash-pro (plan; daily 07:00 UTC; idle since the last refresh, refreshes in 3h),
  free-route (free; capped until 07:00 UTC); errand: …; use-it-or-lose-it allowances nearest their refresh go first within a tier, or pass "route":"<name>").
```

Below it comes one **new** line, printed only when something is idle:

```
Idle allowances: flash-pro (daily, refreshes in 3h, last used 26h ago).
```

"Idle" means a declared shape with no success since its last refresh: for daily, `last_succeeded_ms` is before the previous `daily_utc`; for rolling, there is no open window. The existing `Model route … is backing off` lines stay. A capped row reads `capped until <UTC time> (<shape> allowance spent)` instead of `backing off for Ns`, and the `routes::refusal` text changes the same way. Status views (`jobs::dispatch` `model_routes`, `machines.rs` around line 590) add `capped_until_ms` and `last_succeeded_ms` from `route_usage`. Route names in examples are placeholders.

### Interaction with outages and backoff

- An outage on a shaped route uses the short doubling and records `last_throttled_ms` but not `capped_until_ms`. The route drops out of ranking while blocked and returns when clear.
- A route that is both capped and later hit by an outage keeps the later `until_ms`, as `record` already does by recomputing.
- The account rows still block every model on a Claude account. Ranking reads availability through `blocked`, so a capped account takes all its routes out.
- A routes file that cannot be read still must not stop the backoff (`scan` keeps `unwrap_or_default()`). With no file, `rank` is never reached.

## Changes by file

- `src/crew_routes.rs`: `validate` (`allowance`, `tier`, refusing both `allowance` and `allowance_resets_utc`); **new** `allowances` replaces `free_allowances`; **new** `rank`; `choose` walks the ranked list; `summary` labels and the **new** idle line.
- `src/routes.rs`: `init` creates `route_usage`; `record` writes it in one transaction; **new** `allowance_spent` (generalising `free_allowance`); **new** `classify_usage` (Claude `allowed` windows on success); `scan` passes allowances and job `created_ms`; `refusal` and `list` distinguish capped from backing off.
- `src/context.rs` (`bearings`): the idle line.
- `docs/architecture.md` (its routes paragraph) and the module docs at the top of `src/routes.rs` and `src/crew_routes.rs`.

## Test plan

Unit tests on `rank` and `allowance_spent` take `now`. Integration tests in `tests/crew_routes.rs` use isolated homes (`HOME`, `PRIVATEER_FLEET`, `XDG_STATE_HOME`), fake harness manifests, and `route_usage` and `model_routes` rows written with timestamps relative to the real clock.

1. **Two renewing routes are both used across a refresh.** Daily routes A (renews in 2 h) and B (renews in 20 h) are listed B then A. A launch takes A. A is capped and the next launch takes B. Then, simulating the hours passing by rewriting `until_ms` and moving `now` past A's reset in `rank`, A's next loss is about 24 h away while B's is nearer, so B goes first. Over the cycle both get launches, where today only B would drain.
2. **A capped route is skipped until its window reopens.** A rolling 5 h route throttled with allowance wording gets `until_ms = window_started + 5h`, not 30 s. Launches skip it and the refusal says "capped until". With `until_ms` in the past, the next launch takes it, and a success starts a new window.
3. **A plain throttle on a paid shaped route is not a cap** until the third in a row: `until_ms` follows 30 s, 60 s, then the computed reopen.
4. **The explicit route is honoured**: `route: "<carry-over>"` launches it while a renewing route is available, and an unlisted name is still refused.
5. **A carry-over route is not starved**: with every renewing route capped, the tier-1 plan route is chosen. A tier-2 renewing route never outranks it.
6. **Backward compatibility**: an existing file with `allowance_resets_utc` on free routes and no `allowance` chooses in list order, with backoff identical to today's (the existing `routes.rs` tests pass unchanged). A file with both keys is refused.
7. **Weekly**: a rolling route capped twice with no success in between is held until `weekly_utc`, past the two-day text cap.
8. **Bearings** show the idle line for an idle daily route and `capped until` for a capped one.
9. **A resume keeps its route**: a parked crew on a carry-over route resumes on it even when a renewing route is idle.

Each test proves behaviour through commands and ledger state.

## Failure modes

- **A wrong declared reset.** A throttle just after the declared time falls back to probes and is flagged in bearings. A declared time that is too late parks the route needlessly, and only until that time.
- **A per-minute limit misread as a cap.** It is bounded by the three-strike rule for paid routes. Free routes keep today's behaviour.
- **Hard work sent to a weak route.** It is prevented only by `tier`. An untiered weak route with a declared shape is promoted. Bearings show tiers so the captain can see it.
- **Every launch piling onto one allowance.** Intended: it drains until capped, then the next route takes over. Concurrent launches may overshoot a cap by the runs already in flight; each failed run is retried by the first mate as today.
- **Clock skew between homes.** All times come from the same host's clock and ledger, so skew stays out of scope.

## Open questions for the captain

1. **What "acceptable quality" means.** Proposed: an explicit per-route `tier` (default 1), with promotion only within a tier and never past a better one. The alternative is that a route is promoted only past routes listed after its first position in a "quality floor" marker. Is a numeric tier acceptable, and should free routes default to tier 2 instead of 1?
2. **Should the first mate pass difficulty?** A **new** optional `min_tier` on `agent.start` and errands would keep hard work off weaker tiers per launch. It is left out of this round unless wanted.
3. **Legacy `allowance_resets_utc`.** Should it keep its current backoff-only meaning (proposed), or count as a daily shape and so promote free routes? Promoting it changes the order of existing files.
4. **Shared allowances.** One Google Pro allowance serves both the AGY and Gemini Flash routes. Proposed: a **new** optional `allowance.pool: "<name>"` so routes in one pool share `route_usage` and a cap on one caps the pool. Is the pool needed in this PR, or should both routes declare the same shape and be tracked separately for now?
5. **Rolling and idle ordering.** An idle rolling window loses capacity only while unused. Is ranking it by `now + window_hours`, after a daily allowance due sooner, the order the captain wants?
6. **Errands.** Should errand lists rank the same as crew lists (proposed), or stay first-available?
