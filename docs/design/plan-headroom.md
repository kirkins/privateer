# Design: plan headroom, and one retry of a refused first-mate wake

Status: implemented. Captain findings 15 and 16, approved 2026-10-18 with the answers recorded in the task's design note: thresholds of 80 and 95 percent as per-route settings with those defaults; new crew and errands refused at 95 percent with running work left alone; a run budget setting with no default, proposed from recorded usage and leaning conservative; and the retry-once on the fallback, which skips requests already answered. Read with [allowance-aware routes](allowance-aware-routes.md) (the `route_usage` row and the allowance shape), [route strength](review-gate-and-route-strength.md), [crew park and resume](crew-park-resume.md) and [architecture](../architecture.md).

## Problem

Nothing acted before a plan limit. A run that hit one parked its route until the stated reset, and a first mate whose route was parked already woke on the first clear route in its `first_mate` list. That is the right response to a *refusal*, and it is no use against the common case: eighteen first mates share the Claude Max plan, so the account runs into its weekly cap long before any single run is refused. By then the crew are dead, every wake on that account fails, and the only record is a log.

Two gaps follow from that.

- **No reserve.** The ledger knows what Claude reported (how much of the five-hour and weekly windows is spent) but nothing reads it before a refusal. For any other provider nothing is known at all: only Claude reports usage on every run, so for the OpenCode Go plan the one at risk there is no signal but a count of runs.
- **A failed wake repeats itself.** A wake that fails keeps its mail (a failed wake does not advance `woken_through`) and is retried a minute later within the hourly wake budget, on the same route that just refused, resending every message including the ones that attempt already answered.

## Model

### 15: a share spent, and the shares kept for each kind of launch

`routes::spent(usage, budget, now)` turns the latest reading for a route into one number: the furthest spent of the windows Claude reported and of the runs counted against the captain's run budget. Nothing else contributes, and a route with neither is never refused on a share nobody knows. The window that is furthest spent is named in the message, with the time it clears when that is known and still ahead.

Two per-route settings, whole percents from 1 to 100, with defaults of **80** and **95**:

```json
"opencode-deepseek": {"manifest": "bundled:opencode-v2", "options": {"model": "deepseek-v4.1-flash"},
    "cost": "plan", "tier": 3, "uses": ["implement"],
    "allowance": {"window_hours": 5, "runs": 8},
    "crew_headroom_pct": 80, "first_mate_headroom_pct": 95}
```

- **New crew and errands** (`routes::Who::Crew`) are refused at the crew share and take the next route in their list, exactly as a backed-off route is skipped today. A refused launch says which window, the share reached, the share kept, and to take the next route.
- **A first mate** (`routes::Who::FirstMate`) keeps its own route until the first-mate share, then the next wake moves to its fallback in a fresh session, through the path a backed-off route already uses. `crew_routes::first_mate_fallback` and `activation::own_route_blocked` both read that share instead of only the hold.
- **Running work is never stopped.** A plan is spent by the runs already in flight; stopping them wastes them and leaves the mail unanswered. Nothing in this change cancels, pauses or queues a live job.

A route the captain has not named in the routes file (a ship's own first-mate profile) takes the defaults: 80 and 95.

### The run budget

`allowance.runs` is the number of runs the window holds. It has **no default**, because no runtime can tell what a plan allows; the captain states it. A route with no budget is never measured by runs.

A run is counted against the budget when it reached the provider: every success, and every failure whose log reads as a limit or an outage. A run that failed before the model answered (a broken harness, an expired credential, a tool error) spends nothing and counts nothing. The count and the window that holds it are two columns on `route_usage`, `runs` and `runs_window_ends_ms`, written in the same transaction as everything else the job records. The window opens at the run's own `created_ms` and ends at the shape's reopen (`window_hours` after the start, or the next `daily_utc`), so the count and the backoff agree on when the allowance renews. A window that has closed starts the count at zero, and a run budget with no shape cannot be declared.

The `route_usage` row is written for a route that declares an allowance **or** that reported its plan windows, so a Claude route with no declared shape still keeps the reading its account reports. That is the change the shared Claude Max plan needs: the ledger holds one latest reading per account, and every home sees it.

### 16: fall back before failing, and retry the wake once

A wake that failed while its log reads as a **limit or an outage** (not a task error) is retried once, at once:

- **Once.** `wakes.retry_of` names the wake a wake is retrying. A wake that is itself a retry is never retried, and no wake retried twice.
- **At once, and outside both waits.** The retry is decided before `retry_backoff_secs` and `max_wakes_per_hour` are read, and passes them. The ordinary waits still apply to every other failure.
- **On the fallback.** `crew_routes::retry_fallback` is the captain's `first_mate` order without the route that just failed, whatever its state now. A fresh session: a resumed session belongs to the route it was warmed on. With no clear fallback there is no retry, and the wake waits as it did.
- **Nothing is answered twice.** The requests that attempt answered are read out of the mailbox, where a reply marks the request it answers answered, and are left out of the retry's mail. The retry's prompt names the wake it replaces, the error that ended it, and the requests already answered, and still covers the mail through the same sequence: a wake covers mail through its sequence whether or not this first mate read it, so nothing is asked again.

A task error is not retried early. The same first mate with the same brief on another route would fail the same way, so it keeps its mail and waits out the ordinary backoff, as before.

## Changes by file

- `src/routes.rs`: `Who`; `Spent` and `spent`; `refusal_for`, `held_off` and the headroom refusal; `route_usage` gains `runs` and `runs_window_ends_ms` (migrated under the write lock); `Allowance.runs` counted in `record_job`; Claude windows recorded for every Claude route.
- `src/crew_routes.rs`: `Limits` (the two shares and the run budget), `limits`, `limits_of`; `runs` in the allowance shape and its validation; `choose` refuses at the crew share, and `resume_route` is blocked only by a hold; `first_mate_fallback` and the new `retry_fallback` at the first-mate share; `RouteState.spent` shows the share in bearings and in the fleet snapshot.
- `src/activation.rs`: `own_route_blocked` reads the first-mate share; `Last`, `Retry` and `retryable`; the wake row records `retry_of`; the retry is taken outside both waits and skips its answered mail; `wake_text` tells the retry what it is retrying.
- `src/lib.rs`: a launch is judged as the kind it is, so a first mate's own wake is not refused at the crew share.
- `src/snapshot.rs`: each route row carries `used`, the share spent and the window it is from.

## Tests

Unit tests: the share is the furthest window and names it, and a closed window or an unstated budget counts for nothing; runs count whether the run succeeded or was refused, and the count resets with its window; a Claude reading is kept for a route that declares no allowance; the defaults are 80 and 95 with no budget, and a run budget needs a whole number of runs.

Integration, with an isolated routes file and ledger (`PRIVATEER_FLEET`, `PRIVATEER_RESOURCE_HOME`) and fake harnesses:

- A fake Claude Code that reports a share of each window on every run: crew are refused past 80 percent and take the next route, the refusal names the window and both shares, bearings and the snapshot show the share, and a first mate is still inside its own reserve at 90 percent.
- A run budget of three: the third run starts, the fourth takes the next route, the refusal says "run budget (3 of 3 runs)", and a closed window counts from nothing.
- Bounded settings: percents outside 1 to 100 and budgets outside 1 to 10000 are refused, and two names for one model must agree on the budget as well as the shape.
- A first mate with a budget on its own route: it wakes there, keeps the route at four fifths, moves to the fallback in a fresh session past nineteen twentieths, and returns when the window renews.
- A wake that answers one request and is then refused by the provider: it is retried at once on the fallback with `retry_backoff_secs` of an hour and `max_wakes_per_hour` of one, the retry's prompt names the wake and the answered request and does not carry it, and the answered request is never answered twice.
- A retry that fails is not retried again: the wake after it is an ordinary one, with no retry line in its prompt.

## Not in this change

- **A pace-based check**: percent used against percent of the window elapsed. This is the planned next step, and it is the one that would catch a plan being drained too early rather than too late. It needs the window's start, which a run budget records lazily (`runs_window_ends_ms`) and Claude reports (`resetsAt` gives only the end).
- **Stopping or queueing running work** at the high share. The captain's answer was not to.
- **A default run budget**, which no runtime can invent.
- **Per-account budgets** for routes that share one plan under different names. The share is per route identity today, as `route_usage` is keyed.

## Failure modes

- **The run budget counts finished runs only**, so parallel launches can overshoot it.
- **Runs that end on a task error or a cancel are not counted**, so failed attempts before reaching the provider do not consume budget.
- **Across a window edge the budget can let through twice its value** if the plan window is rolling.
- **A budget set too low** refuses work that would have succeeded. It is the captain's number, it is one line in their file, and bearings say how much of it is used.
- **A budget set too high** is a cap reached before Privateer knows, which is what this change is for; the failure then looks as it does today, a refused run.
- **A run that reaches the provider and reports no usage** still counts, because counting is by run and not by reported tokens.
- **A counted run that never started** (queued for capacity, then cancelled) does not reach `scan`, so it counts nothing.
- **Two routes on one plan under different names** each get their own budget and their own count, so the reserve is per name, not per plan. The captain states one budget per name today.
- **A wake that failed on a task error but whose log reads like a limit** is retried once. The classification is the same one the backoff already uses, and a retry costs one wake.