# Design: allowance-aware routes

Status: revised after the Opus design review of `a0efe89` (verdict: changes needed). This revision makes the review's must-fixes 1 to 9 and records the first mate's answers to the open questions. Task `privateer-allowance-aware-routes` (captain finding, notice 3ea9cf1b, 2026-10-18). Read with [crew park and resume](crew-park-resume.md) (a resume's route policy) and [architecture](../architecture.md). Names marked **new** do not exist yet; every other name was checked against the tree at `49501c5`.

## Problem

A Google Pro allowance (the `agy-flash` route: AGY running Gemini Flash) renewed with headroom left and went unused. The captain listed it on the crew lists, after a free route. Three gaps let that happen:

1. **Fixed priority.** A launch takes the first listed route that is not backing off. An allowance that renews and is listed below a plan route gets only overflow. With two renewing allowances, only the first is drained.
2. **Daily-only reset model.** The only shape the captain can state is `allowance_resets_utc` (one UTC time of day), and only free routes use it. There is no rolling window (Claude's five hours) and no weekly window. A plan route with no stated reset retries on the 30 s to 15 min doubling. A free route with no known reset is probed hourly, doubling up to a day.
3. **Invisible idle allowances.** Bearings list routes in order and mark the ones backing off. Nothing says that an allowance is unused and about to renew.

## Where route choice happens today

- **The routes file.** `crew_routes::path` is the fleet registry path with extension `routes.json`. Its top-level keys are `routes`, the lists `crew`, `errand` and `first_mate`, and `ships.<ship>.<kind>`. A route holds `manifest`, `options`, `cost` (`free|plan|credits`), `note` and `allowance_resets_utc` (`HH:MM`, parsed by `crew_routes::utc_minute`). `crew_routes::validate` rejects unknown keys. `routes.set` (captain only) replaces the whole file atomically. `crew_routes::order` returns a ship's list for a kind: the ship's own list if it has one, otherwise the fleet's.
- **Crew launch.** `agent.start` calls `crew_routes::choose(store, ship, "crew", requested)`. A launch that passes a `manifest` is checked by `crew_routes::check` instead.
- **Errands.** `src/errand.rs` calls `crew_routes::choose(…, "errand", requested)`, falling back to the ship's own route.
- **First mates.** `crew_routes::first_mate_fallback` leaves a first mate on its own route unless that route is blocked; only then does it take the first available entry on the `first_mate` list.
- **Resume and retry.** `crew_routes::resume_route` keeps a parked crew on its launch route while that route is listed and clear. `crew_routes::retry_refusal` refuses a replay on a route that is no longer listed.
- **`choose`** walks the candidates in list order and returns the first one where `routes::refusal` is `None`.
- **Backoff.** On each tick, `routes::scan` reads spawn jobs that finished since the home's cursor (`routes_scanned_ms`). A `succeeded` job clears its route; a failed job goes to `routes::classify`, which returns a `RouteFailure` (`error`, `reset_ms`, `allowance`, `account`). For a free route, `routes::free_allowance` treats a rate limit or quota as a spent allowance and takes the reset from `allowance_resets_utc`, unless the throttle came within 15 minutes after that time. `routes::record` writes `model_routes(route, failures, until_ms, last_error, updated_ms)`, **overwriting** `until_ms` with `now + backoff` (it never compares with the existing row). A success deletes the route row and its account row, so no record of a past throttle or success survives.
- **Bearings.** `context::bearings` prints `crew_routes::summary` and then one line per row from `routes::list`.

## Model

### Allowance shape (routes schema)

A route gains an optional **new** `allowance` object holding exactly one shape:

| shape | fields | meaning |
|---|---|---|
| daily | `daily_utc: "HH:MM"` | renews at that UTC time each day; whatever is unused then is lost |
| rolling | `window_hours: 1..=168`, optional `weekly_utc: "Mon HH:MM"` | a window opens at the first use after the previous one closed and closes `window_hours` later; an optional weekly cap renews at a fixed UTC day and time |
| none | (no `allowance`) | carry-over: a plan or credits route, or a free route the captain has not described |

A route may hold a **new** `tier: 1..=9` (1 is strongest). **`tier` is required whenever `allowance` is set**, so no route is promoted by default: shaping a route makes the captain say how good it is. A route without `allowance` may omit `tier` and counts as tier 1. Free routes get no lower default.

`validate` refuses:

- unknown keys inside `allowance`, or no shape, or both shapes;
- `allowance` without `tier`, and a `tier` outside 1..=9;
- `allowance` together with `allowance_resets_utc` (the message tells the captain to move the time into `allowance.daily_utc`);
- two routes that share an identity (`harness/provider/model`) but declare different shapes, because the runtime keys allowances by identity.

`allowance_resets_utc` keeps today's meaning: backoff only, on free routes, through `free_allowance`. It does **not** opt a route into ranking, so existing files keep their order and backoff.

`crew_routes::free_allowances` is joined by a **new** `crew_routes::allowances(store)`: route identity → `Allowance { shape, cost, tier }` for every route with an `allowance`. `scan` uses `allowances` first and falls back to `free_allowances` for legacy free routes.

### Per-route tracking (host ledger)

A **new** table in the host ledger, created by `routes::init`:

```
route_usage(route TEXT PRIMARY KEY, last_succeeded_ms INTEGER, last_throttled_ms INTEGER,
  last_throttle TEXT, strikes INTEGER NOT NULL DEFAULT 0, window_ends_ms INTEGER,
  capped_until_ms INTEGER, updated_ms INTEGER NOT NULL)
```

- **Only shaped routes get rows** (review 6). `scan` records a usage row only for a route identity in `allowances`, so the table is bounded by `validate`'s 32 routes. Rows not updated in 30 days are also pruned on each scan, so identities the captain has since removed do not linger.
- **Idempotent times** (review 5). Every time in the row comes from the job: `last_succeeded_ms` and `last_throttled_ms` are the job's `finished_ms`, and the window starts at its `created_ms`. Each is written with `MAX(old, new)`, never from `Store::now_ms()`. The per-home cursor commits separately from the host ledger, so after a crash `scan` may read the same jobs again; re-reading them changes nothing. `updated_ms` is the job's `finished_ms` too.
- **One transaction.** `record` writes `model_routes` and `route_usage` inside one transaction, reading the usage row it decides from in the same transaction.
- **Rolling windows.** A success whose job `created_ms` is at or after `window_ends_ms` (or with no window yet) opens a window ending at `created_ms + window_hours`: the provider starts counting at launch, not at the end of the run.
- **Claude's own windows** (review 8). For a Claude route, on success `scan` reads only the **tail** (the last 64 KiB) of the job's stdout log, and only for routes whose harness is `claude`. The last `rate_limit_event` with status `allowed` carries `rate_limit_info.unifiedWindows.five_hour` and `.seven_day`, each with `utilization` (0 to 1) and `resetsAt` (epoch seconds). The **new** `routes::claude_windows` reads both. `five_hour.resetsAt` replaces the computed `window_ends_ms`, so the declared length fills in only when the provider says nothing. The two utilizations are shown in bearings. Other harnesses' logs are not read on success.
- **Caps are never stale** (review 7). `capped_until_ms` is cleared on success, and every reader compares it with `now`; bearings and refusals never show a cap that has passed.

### When a throttle caps a shaped route

The **new** `routes::allowance_spent(failure, allowance, usage, created_ms, now) -> (RouteFailure, strike)` decides, for a route with an `allowance`. Each throttle is one of three kinds:

- **Outage**: 5xx, `overloaded`, `timed out`, connection errors. Never a cap and never a strike: the short doubling, as today.
- **Allowance wording** (`quota`, `usage limit`, `allowance`, `exhausted`, `hit your limit`, `limit reached`), Claude's `rejected` window, or any rate limit on a **free** route (OpenCode's free tier says only "Rate limit exceeded"). The allowance is spent: a cap.
- **Plain throttle** (`429`, `rate limit`) on a plan or credits route. A **strike**, counted only when the job's `created_ms` is later than the route's `last_throttled_ms` (review 2). Concurrent runs that hit one per-minute 429 in the same burst all started before the first of them failed, so the burst counts once. Outages do not count, and a success resets `strikes` to 0. The first two strikes stay on the short doubling (30 s, then 60 s). The **third** strike moves the route to the hourly probe ladder (1 h, 2 h, doubling to a day), **capped at the shape's reopen time** (review 3), not straight to the end of the window.

**How long a cap lasts.** The provider's stated reset wins, as today, and the doubling only raises it. With no stated reset, the reopen time comes from the shape:

- daily: the next `daily_utc`. The existing guard still applies: a throttle within 15 minutes after the declared time leaves the route to the hourly probes.
- rolling: `window_ends_ms` when it is still in the future; otherwise the probe ladder capped at `now + window_hours` (the window was started outside Privateer, so its end is unknown).
- weekly (review 4): only on evidence. Either Claude reports a structured `seven_day` limit (whose own `resetsAt` already holds the account), or a stated reset is longer than `window_hours`. Then, when `weekly_utc` is declared, the route is held until the next `weekly_utc`, trusted up to `MAX_STATED_RESET_MS` because the captain declared it. A second cap without a success in between is **not** evidence; it goes to the probes, so one bad probe never parks a route for a week.

**`record` keeps the later `until_ms`** (review 1). Its upsert becomes `until_ms = MAX(model_routes.until_ms, excluded.until_ms)`, so an outage reported by a run that was in flight when a 13-hour cap was set cannot shorten the cap to 60 s. A success still deletes the row, which is how a cap clears early.

A cap sets `capped_until_ms` to the row's resulting `until_ms`. `model_routes.until_ms` still drives `routes::blocked`, so every existing check (`choose`, `resume_route`, `first_mate_fallback`, delivery holds, `job.retry`) honours a cap without change. Reopening needs no timer: once `until_ms` passes, `blocked` is clear and the next `choose` sees the route.

### The ordering rule

`choose` keeps its signature and its explicit-route path. Only the order of the walk changes when `requested` is `None`:

1. **Allowlist.** Candidates are the ship's list for the kind (`order`), exactly as today. Nothing unlisted is ever chosen.
2. **Available.** A route is skipped while `routes::refusal` is `Some`, exactly as today.
3. **Quality first.** Stable-sort by `tier`, best first. A shaped route is never promoted past a route of a better tier. Untiered routes count as tier 1, and a file with no `tier` and no `allowance` keeps its order exactly.
4. **Within a tier**, three classes in this order:
   1. Shaped routes **with a deadline**, by nearest loss: daily, the next `daily_utc`; rolling with an open window, the window's end.
   2. **Idle rolling** routes (no open window). An idle window loses nothing until it is first used, so it ranks after every deadline but ahead of carry-over. Keyed by the next `weekly_utc` when declared (or Claude's `seven_day.resetsAt` when known); otherwise list order.
   3. **Carry-over** routes (no `allowance`), in list order.
5. **Tiebreak.** Equal keys (to the minute) keep the captain's list order.

The **new** pure function `crew_routes::rank(names, policy, usage, now) -> Vec<String>` does steps 3 to 5. `choose` walks its output with the existing refusal loop, so the refusal text and the "every route is unavailable" error are unchanged. Taking `now` as a parameter lets tests hold the clock.

**Explicit route honoured.** `requested` bypasses ranking. It is still refused if unlisted or blocked, and the refusal names the reopen time when capped.

**Crew and errands rank alike.** Each kind has its own list, so the captain can give errands different tiers. `first_mate_fallback` does not rank: moving a first mate's conversation for allowance reasons breaks its warm session. `resume_route` does not rank: a parked crew resumes on its launch route while it is listed and clear. A resume that falls back to a fresh crew goes through `choose` and is ranked. `check` and `retry_refusal` are unchanged.

### Bearings

`crew_routes::summary` labels gain the shape, the tier, and the state:

```
Routes (crew: claude-plan (plan; 5h window, tier 1, open until 14:20 UTC), flash-pro (plan; daily 07:00 UTC, tier 2),
  free-route (free) capped until 07:00 UTC; …; use-it-or-lose-it allowances nearest their refresh go first within a tier, or pass "route":"<name>").
```

A list with no shaped route reads exactly as today. Below the summary comes one **new** line, printed only when something is idle:

```
Idle allowances: flash-pro (daily 07:00 UTC, refreshes in 3h, last used 26h ago).
```

"Idle" means a shaped route on one of the ship's lists that is not blocked and has no success since its last refresh: for daily, `last_succeeded_ms` is before the previous `daily_utc`; for rolling, there is no open window. The existing `Model route … is backing off` line reads `Model route … is capped until <UTC time> (allowance spent: …)` for a capped row, and `routes::refusal` changes the same way. `routes::list` (status views in `jobs::dispatch` and `machines.rs`) adds `capped_until_ms` when the cap has not passed. Route names in examples are placeholders.

### Interaction with outages and backoff

- An outage on a shaped route uses the short doubling and records nothing in `route_usage` but the throttle time; it is neither a cap nor a strike.
- A route that is capped and later hit by an outage keeps the later `until_ms` (`record`'s `MAX`).
- The account rows still block every model on a Claude account. Ranking reads availability through `blocked`, so a capped account takes all its routes out.
- A routes file that cannot be read still must not stop the backoff (`scan` keeps `unwrap_or_default()`).

## Changes by file

- `src/crew_routes.rs`: `validate` (`allowance`, `tier`, the refusals above); **new** `Allowance`, `allowances` and `rank`; `choose` walks the ranked list; `summary` labels and the **new** idle line.
- `src/routes.rs`: `init` creates `route_usage`; `record` keeps the later `until_ms` and writes usage in one transaction; **new** `allowance_spent`, `claude_windows`; `scan` passes allowances and job times and prunes old usage; `refusal` and `list` distinguish capped from backing off.
- `src/context.rs` (`bearings`): the idle line and the capped wording.
- `docs/architecture.md` (its routes paragraph) and the module docs at the top of `src/routes.rs` and `src/crew_routes.rs`.

## Test plan

Unit tests on `rank` and `allowance_spent` take `now`. Integration tests in `tests/crew_routes.rs` use isolated homes and write `route_usage` and `model_routes` rows with timestamps relative to the real clock.

1. **Two renewing routes are both used across a refresh** (`rank`). Daily A (renews in 2 h) and B (renews in 20 h), listed B then A: A goes first. After A's reset (`now` moved past it), A's next loss is about 24 h away and B's is nearer, so B goes first.
2. **A capped route is skipped until its window reopens.** A rolling route with an open window, throttled with allowance wording, is held until the window's end. A launch skips it and the refusal says "capped until". Once that time passes, the cap is neither shown nor honoured, and the next launch takes it.
3. **A plain throttle on a paid shaped route is not a cap**: strikes 1 and 2 give 30 s and 60 s; strike 3 gives the hourly probe, capped at the shape's reopen.
4. **One burst counts once**: three concurrent plain throttles (all created before the first failed) make one strike.
5. **`record` keeps a later `until_ms`** when an outage follows a cap.
6. **The explicit route is honoured**: `route: "<carry-over>"` launches it while a renewing route is available, and an unlisted name is still refused.
7. **A better tier is never passed**: a tier-2 renewing route stays behind a tier-1 carry-over route; an idle rolling route ranks after a daily deadline and before carry-over.
8. **Backward compatibility**: a file with `allowance_resets_utc` on free routes and no `allowance` chooses in list order with today's backoff (the existing `routes.rs` tests pass unchanged). A file with both keys, `allowance` without `tier`, or one identity with two shapes is refused.
9. **Weekly needs evidence**: a stated reset longer than `window_hours` holds a rolling route until `weekly_utc`; a second cap with no stated reset goes to the probes.
10. **Claude windows**: an `allowed` event's `five_hour.resetsAt` sets `window_ends_ms`.
11. **Bearings** show the idle line for an idle daily route and `capped until` for a capped one, and no stale cap.
12. **Idempotent scan**: recording the same success twice leaves the same row.

## Failure modes

- **A wrong declared reset.** A throttle just after the declared time falls back to probes. A declared time that is too late parks the route needlessly, and only until that time.
- **A per-minute limit misread as a cap.** Bounded by the burst-aware three-strike rule and the probe ladder for paid routes. Free routes keep today's behaviour.
- **Hard work sent to a weak route.** Prevented by the required `tier`. The captain sees tiers in bearings.
- **Every launch piling onto one allowance.** Intended: it drains until capped, then the next route takes over. Concurrent launches may overshoot a cap by the runs already in flight; their outages cannot shorten the cap (`record`'s `MAX`).
- **A promoted route that fails without throttling** (expired auth, a broken manifest) is never backed off, because only throttles back a route off, and it catches every launch. This exists today for the first listed route; promotion makes it reach further. The crew's own failure report is the signal; backing off on repeated non-throttle failures is follow-up.
- **Shaping a plan that others share** lets crew drain it. A plan the captain or first mates also use (Claude's weekly caps across models, a route also on `first_mate` lists) is drained by crew once shaped. Shaping is the captain's call, made in the file; bearings show the utilization Claude reports.
- **Clock skew between homes.** All times come from the same host's clock and ledger.

## Decisions (first mate's answers to the open questions)

1. **Tier**: numeric, required whenever `allowance` is set; no default promotion, no lower default for free routes.
2. **`min_tier`** on launches: not in this PR.
3. **Legacy `allowance_resets_utc`**: backoff only.
4. **Pools** (shared allowances, such as two routes on one OpenCode Go plan): not in this PR; follow-up.
5. **Idle rolling windows** rank after shaped routes with a deadline and ahead of carry-over, keyed by `weekly_utc` when declared.
6. **Errand lists** rank like crew lists.

Open, from the review: `scan` reads only `action='spawn'` jobs, so a first mate's wake on a shaped plan does not count as use. Follow-up if first mates share shaped routes.
