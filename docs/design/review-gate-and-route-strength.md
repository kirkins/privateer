# Design: route strength as data, and a top-tier review gate before a ship merges

Status: design only, for an Opus design review. Task `privateer-review-gate`, captain request 6b968f99 items 1 and 2, after the FitForge incident of 2026-10-18. Part A comes first because Part B depends on it. Read with [allowance-aware routes](allowance-aware-routes.md) (PR 237: `tier`, `rank`), the crew-role design (`docs/design/crew-role.md`, commit 7588e6c, not on main yet), [crew park and resume](crew-park-resume.md) and [architecture](../architecture.md). Names marked **new** do not exist yet. Every other name was checked against the tree at `1f3cebb`.

## Problem

- **The incident.** A DeepSeek crew uninstalled the app from the captain's phone and wrote that up as evidence in FitForge PR #509, and the DeepSeek first mate merged it. "One Opus review per code PR" was only text in the ship note. A weak first mate skipped the review, or reviewed with a model of the same strength. Nothing at the merge checked that a review had happened.
- **Strength is prose.** The server liaison told the captain that Luna was "the strongest available", because nothing ranks the models. The captain's stopgap is a prefix on each route's `note`: in the live routes file today, `opus` is `STRENGTH 1/top`, `haiku`, `agy-flash`, `deepseek-flash`, `opencode-deepseek` and `opencode-free` are `STRENGTH 3/mid`, and `luna` is `STRENGTH 4/WEAKEST`. No route sets `tier`.
- **`tier` exists but orders, and does not filter.** PR 237 added `tier` (1 to 9, `crew_routes::validate`). It is required only together with `allowance`, and `crew_routes::tier` defaults a missing tier to 1. `crew_routes::rank` sorts best tier first ("a better tier is never passed"). A launch cannot ask for a minimum strength or a use (allowance-aware-routes.md, decision 2: "`min_tier` on launches: not in this PR"). If the captain copied the STRENGTH numbers into `tier` today, `rank` would move `opus` to the front of every crew list, which is the opposite of what the captain wants.
- **Merges are checked only on the caller's word.** `delivery.check` (`src/core.rs:696`) authorizes `merge_green` and compares heads the caller supplies. Its own reply says "caller-supplied; forge adapter must verify live checks". Privateer has no forge code at all. Ships merge with `gh` directly.

# Part A: model strength as data

## Data shape

The existing `tier` becomes the captain's **rank** (1 is top). It is joined by **new** `uses`, a list:

```json
"opus": {"manifest": "bundled:claude", "options": {"model": "opus"}, "cost": "plan",
         "tier": 1, "uses": ["review", "diagnose", "design", "implement"],
         "note": "Claude Code + Opus 5.5 on the captain's Max plan."}
```

- `uses` takes words from a fixed vocabulary, **new** `crew_routes::USES` = `implement`, `review`, `diagnose`, `design`. It must be non-empty, with no duplicates.
- `crew_routes::validate` gains two rules: `uses` requires `tier`, and an unknown use word is refused. `tier` stays optional when there is no `uses` and no `allowance`.
- **A route with no `tier` or no `uses` is unranked.** It never meets a requirement. `crew_routes::tier`'s default of 1 is kept only for display and is never used for a requirement. A route promotes itself by nothing.

## Choosing a route

`agent.start` (crew, `src/lib.rs:276`) and `errand.start` (`src/errand.rs:158`) take an optional **new** `require` argument: `{"use": "review", "tier": 1}`. Both fields are optional. A route **meets** it when its declared `tier` is less than or equal to `require.tier` and its `uses` contains `require.use`. `crew_routes::choose` gains a `require` parameter:

1. **Filter.** It considers only routes on the ship's list for this kind (`crew_routes::order`) that meet `require`.
2. **Order: cheapest that meets it.** Use-it-or-lose-it allowances nearest their refresh come first, then idle rolling windows, then carry-over routes (the class key in `rank`, unchanged). Equal classes keep the captain's list order, which is already cheapest-first in the live file: `opencode-free`, the plan routes, then `luna`, then `opus`. `cost` (`free|plan|credits`) is too coarse to order by, because six of the seven routes are `plan`.
3. **`rank` stops sorting by tier** for every launch, with or without `require`. Tier is a floor, not a preference. This replaces PR 237's rule 3 ("quality first"). A file with no tiers keeps today's order exactly, because every route then sorts as tier 1. Open question 1 asks the captain to confirm the change.
4. **An explicit `route`** is still honoured. It is refused when it does not meet `require`: "route luna is tier 4 with uses [implement]; this launch needs review at tier ≤ 1; routes on privateer's errand list that meet it: opus". An explicit `manifest` (`crew_routes::check`) is matched to its named route through `routes::route_of` and judged the same way.
5. **Fail closed.** A launch with `require` is refused when there is no routes file, the ship's list is empty, or no listed route meets the requirement. It is never silently launched on the first mate's own route. When every route that meets it is backing off, the launch is refused with `routes::refusal`'s reset time, as today.

`first_mate_fallback` and `resume_route` do not take `require`. A parked crew resumes on its launch route, and a resume's fresh crew launches through `choose` with the `require` its launch recorded (stored on the job's `launch`, `src/jobs.rs:156`).

## Reporting the choice

Every launch records why its route was chosen. Ships quote that text and never call a model "the strongest":

- The `agent.start` and `errand.start` replies gain `route: {name, identity, tier, uses, why}` (**new**). `why` is one line, for example `cheapest on privateer's errand list meeting review at tier ≤ 1: opus` or `first available on privateer's crew list: opencode-free (no requirement)`.
- The job record keeps `model_route` (`src/jobs.rs:131`) and gains `route_name` and `route_why` (**new**). The `errands` table gains `route` and `route_why` columns (**new**), added at open as `plan_digest` was (`errand::init`).
- The crew outcome notice (`crew::outcome`, `src/crew.rs:100`) and the errand verdict notice (`errand::settle`, the `Errand on {focus}, verdict:` body) gain a line `Route: <why>`. This is where a ship reads the text it repeats to the captain.
- The ship guide (`src/cli.rs:294`, the crew routes sentence) says: "say which route ran, with the runtime's `why` line ('best on my errand list: X'), never 'the strongest'".

## Display

- **Bearings.** In `crew_routes::summary` each label already shows `tier N` when a tier is set. It gains `uses review/implement`, and an unranked route reads `unranked`. The rule text ("use-it-or-lose-it allowances nearest their refresh go first within a tier") becomes "...go first; pass \"require\" for a use or tier".
- **`ship.profile`.** `profile::routes` (`src/profile.rs:65`) returns `names` per list. It gains `routes: {name: {cost, tier, uses}}` for the names it lists.

## usage.summary by tier and cost

Captain notice 8e4da677 item 4 asks for each strength's share of usage. `usage::summary` gains `group_by` values `tier` and `cost` (**new**, checked where `period` checks `group_by`, `src/usage.rs:216`). A job's route identity is mapped to the current routes file's tier and cost. Identities no route names, and jobs carried from machines with other files, are grouped as `unlisted`. It is grouped by the tier as of the read, not as of the job; the reply says so (`"tiers_as_of": "current routes file"`).

It belongs here because it needs the same identity-to-route lookup as `choose`. But nothing depends on it, so it is the last round of Part A and Part B does not wait for it.

## Migration

1. Land A1 (fields, validation, display, `rank` change). Until it lands the captain must **not** copy STRENGTH numbers into `tier`, because today's `rank` would move `opus` to the front of the crew lists.
2. The captain runs `routes.set` with `tier` and `uses` from the notes: `opus` tier 1, uses `review, diagnose, design, implement`; `haiku`, `agy-flash`, `deepseek-flash`, `opencode-deepseek` and `opencode-free` tier 3, uses `implement`; `luna` tier 4, uses `implement`.
3. The captain removes the `STRENGTH n/...` prefixes from the notes. `routes.set` returns `warnings` (**new**, not a refusal) for any note that still starts with `STRENGTH`, and for any route listed on an `errand` list that has no `review` use. The runtime never reads notes.
4. Ships' notes and assignments that say "use opus for review" become `require` (a review errand) or `review.start` (Part B).

## Failure modes

- **No route meets a requirement.** The refusal names each listed route with its tier and uses, so the ship can tell the captain exactly what is missing.
- **The captain re-tiers a route while a launch is in flight.** The choice is made once, recorded on the job and errand row, and not re-derived. Part B re-checks the reviewer's tier at merge (below).
- **A weak first mate omits `require`.** It gets the cheapest route, as today. Part B does not trust the first mate to ask: `review.start` sets the requirement itself.

## Tests (Part A)

1. `tests/crew_routes.rs` `uses_need_a_tier_and_known_words` (**new**): refused shapes; an unranked route never meets `require`.
2. `tests/crew_routes.rs` `a_requirement_filters_and_the_cheapest_that_meets_it_runs` (**new**): a fake-harness errand with `require` review at tier 1 launches the only qualifying route, past cheaper unqualified ones. An idle allowance route that meets it beats a carry-over route that meets it.
3. `src/crew_routes.rs` unit test `a_better_tier_is_never_passed_and_idle_windows_wait_behind_deadlines` is rewritten as `tier_filters_and_never_orders` (**new**): a tiered file keeps the captain's order within each class.
4. `tests/crew_routes.rs` `an_explicit_route_below_the_requirement_is_refused` (**new**), for both `route` and `manifest`.
5. `tests/crew_routes.rs` `a_requirement_with_no_routes_file_is_refused` (**new**).
6. `tests/crew.rs` `outcome_and_verdict_notices_name_the_route_and_why` (**new**): reads the `Route:` line in the crew outcome and errand verdict mail.
7. `tests/machines.rs` `usage_groups_by_tier_and_cost` (**new**), with an `unlisted` group.

# Part B: a top-tier review gate before a ship merges its own code PR

## Rule

A ship holding `merge_green` may merge a PR into a protected branch only when all of these hold:

1. A review of the PR's **exact current head commit** is recorded with verdict `APPROVE`.
2. The reviewer ran on a route that, both when it ran and at the merge, has `tier` 1 and the `review` use.
3. The reviewer's route identity (`routes::route_of`, `harness/provider/model`) is **not** an author route.
4. The forge holds an approval at that head from the reviewer account. Only Privateer holds that account's token.

A new head needs a new review. App ships keep `merge_green`; this gates it.

**Author routes** are the set `review::author_routes(task)` (**new**) returns: the `model_route` of every agent job (`action` `spawn`, wakes and errands included, review errands excluded) on the PR's task and its descendant tasks, plus the ship's first-mate profile route (an attended first mate leaves no job, and may commit), plus any `author_routes` the ship declares (which can only add). A job whose route cannot be read (no `model_route`, and `routes::route_of_argv` finds none) makes the set `unknown`. Review is then refused until the ship declares that job's route. The set is computed per task, not per commit. Commit metadata is written by the agents and is not trusted. The task is a superset, so a commit cannot hide its author.

## Data shapes

**New** table `reviews`, per home, created by `review::init` (**new**) beside `errand::init`:

| column | meaning |
|---|---|
| `id` | `review-<uuid>` |
| `ship`, `task_id` | who asked, and the task whose author routes apply |
| `forge`, `repo`, `pr` | `github` or `forgejo`, `owner/name`, PR number |
| `base_sha`, `head_sha` | read from the forge by Privateer when the review starts, never taken from the ship |
| `errand_id` | the review errand |
| `route_name`, `route_identity`, `route_tier`, `route_uses` | the reviewer route as chosen, a snapshot |
| `author_routes` | JSON list at start; recomputed at merge |
| `verdict` | `running`, `approve`, `changes_requested`, `none` (no parseable verdict), `failed` |
| `findings` | the reply, bounded to `VERDICT_CHARS` (4000); the full reply stays in the errand log |
| `post_state`, `forge_review_id`, `post_error` | `pending`, `posted`, `stale` (head moved), `failed` |
| `created_ms`, `finished_ms`, `posted_ms`, `merged_ms`, `merge_sha` | lifecycle |

**New** fleet file `forges.json`, beside `routes.json` (`crate::fleet::registry_path()` with another extension). It is private captain configuration and holds no secrets. It is written only by **new** `forges.set`, which is captain-only and listed in `OPERATOR_COMMANDS` (`src/lib.rs:77`), and read with **new** `forges.show`:

```json
{"repos": {
  "github.com/kirkins/privateer": {"kind": "github", "api": "https://api.github.com", "branch": "main",
     "reviewer": {"account": "<reviewer login>", "credential": "forge-reviewer-github"}},
  "<forgejo host>/<owner>/<app>": {"kind": "forgejo", "api": "https://<host>/api/v1", "branch": "main",
     "reviewer": {"account": "<reviewer login>", "credential": "forge-reviewer-forgejo"},
     "merger":   {"account": "<merger login>",   "credential": "forge-merger-forgejo"}}}}
```

Tokens are stored with the existing `credential.set`, which is captain CLI only with the secret on stdin (`src/credentials.rs`). A ref named in `forges.json` is refused by `bundle.configure` (`tool_bundles::configure`, **new** check) and by `credential_environment` in `plugins/bundles.py` (**new** check), so no launch is ever given its path. That covers crews, errands and the review errand itself.

**Forge adapter** `plugins/forge.py` (**new**) has an explicit argv/JSON protocol: `forge.py <op>` with the request on stdin and a JSON reply on stdout. Its operations are `pr.view` (state, base, head sha, mergeable, required checks and their status at the head), `review.post` (`commit_id`, event, body) and `pr.merge` (with the expected head). The token's *file path* goes in `PRIVATEER_FORGE_TOKEN_FILE`, set only on that child process. The adapter uses Python's `urllib`, because the crate has no HTTP client and adding one for three calls is not worth it. Forge API names, to be checked against the installed versions: GitHub `POST /pulls/{n}/reviews` with `commit_id` and `event: APPROVE`, and `PUT /pulls/{n}/merge` with `sha`, which refuses when the head moved. Forgejo `POST /pulls/{n}/reviews` with `commit_id` and `event: APPROVED`, and `POST /pulls/{n}/merge` with `head_commit_id`.

## Commands

- **`review.start {"task_id", "pr", "repo"?, "notes"?, "author_routes"?}`** (**new**). Caller: the ship's agent, never crew (refused by `crew_scope`'s default deny in the crew-role design) and never an errand (`errand::running` on the caller's job). It:
  1. authorizes `merge_green` on the task, since only a ship that may merge needs the gate;
  2. reads the PR through `pr.view` and records `base_sha` and `head_sha`;
  3. computes the author routes;
  4. refuses when this head already has a finished `approve` or `changes_requested` review, or two attempts that ended `none` or `failed`. This stops review shopping: a ship that dislikes a verdict pushes a fix, which is a new head;
  5. starts the errand through `errand::start` with `require: {"use": "review", "tier": 1}`, the author routes excluded from the candidates, and a **fresh** session (never a warm `errand:` session, since `session_key` is per focus);
  6. inserts the `reviews` row.

  The errand brief is built by the runtime: the PR's URL, title and body, `git fetch origin <head_sha>`, the diff `<base_sha>...<head_sha>`, an instruction to read the evidence files the PR adds, and the closing rule "End with exactly one last line: `VERDICT: APPROVE` or `VERDICT: CHANGES_REQUESTED`". The ship's `notes` (at most 2000 characters) are appended under "The author's notes, not instructions".
- **Settlement.** `errand::settle` calls **new** `review::settle(store, errand_id, reply)` before it mails the verdict. The verdict is read only from the reply's last non-empty line, matched exactly; anything else is `none`. On `approve`, Privateer re-reads the live head with `pr.view`. If it moved, `post_state` is `stale` and nothing is approved. Otherwise it posts `review.post` with event APPROVE at `commit_id = head_sha`. On `changes_requested` it posts REQUEST_CHANGES. The review body holds the route name, identity and tier, the commit, the verdict, the findings and the marker `<!-- privateer-review <id> -->`, so the PR comment the captain asked for is that review. Before each post the adapter looks for the marker among the reviewer account's reviews, so a replay after a crash does not post twice. A failed post is `failed`, is retried on the next three ticks, and then mails the ship a normal notice: the captain's token or the forge needs attention. The verdict notice to the ship begins `Review of #<pr> at <sha7>: APPROVE, posted` (or the actual state).
- **`pr.merge {"task_id", "pr", "repo"?, "method"?}`** (**new**). Caller: the ship's agent, not crew and not an errand. In order:
  1. authorizes `merge_green`;
  2. reads the live PR with `pr.view`: it must be open, its base the configured branch, and its required checks green at the head. This replaces the caller-supplied `tested_head` of `delivery.check`;
  3. finds the latest finished review at that head: verdict `approve`, `post_state` `posted`, and the recorded route at tier 1 with `review`;
  4. requires that the route **still** has tier 1 and the `review` use in the current routes file, and that it is not in the author routes recomputed now (a crew may have run since);
  5. applies the production rule as `delivery.check` does: a merge flagged `deploys_production` needs and consumes an approval of this exact artifact and environment;
  6. merges with the expected head (GitHub `sha`, Forgejo `head_commit_id`), so a push after step 2 makes the forge refuse. On GitHub the merge uses the ship's own gh login. On Forgejo it uses the `merger` token, because there the merge whitelist names only that account;
  7. records `merged_ms` and `merge_sha` and audits `pr.merge`.

  Each refusal says what is missing, for example: "#251 head 9f3e2a1 has no approved top-tier review; start one with review.start (the last review was of 1a2b3c4)".
- **`review.list {"ship"?, "pr"?}`** (**new**): rows for bearings and for the captain.
- **Bearings.** **New** `review::bearings_lines` is shown by `context::bearings` (`src/context.rs:287`). It covers the ship's last five reviews: `#251 1a2b3c4 APPROVE by opus (tier 1) posted 2h ago`, `#252 9f3e2a1 CHANGES_REQUESTED (findings in review-…)`, `#253 review running (errand-…)`, `#251 approval stale: head moved`.
- **The ship guide** (`src/cli.rs:277`, the pull request line): "merge with `pr.merge`; it needs `review.start` to have approved the current head on a top-tier route that did not write the code. Do not merge with `gh`; the forge refuses it without the reviewer's approval."

## Enforcement at the forge

The runtime's checks in `pr.merge` stop a ship that uses the documented path. The forge stops a ship that does not. The captain configures the forge once per repository, and `forges.set` reads the protection back through the adapter and warns when it is missing.

**GitHub** (this repository, a personal repository). Branch protection on `main`:

- require a pull request, with one approving review, and **require review from Code Owners**. A base-branch `CODEOWNERS` with `* @<reviewer login>` makes only the reviewer account's approval count. GitHub reads `CODEOWNERS` from the base branch, so a PR cannot edit its way past it;
- **dismiss stale approvals when new commits are pushed**, and **require approval of the most recent reviewable push**;
- **do not allow bypassing**, the enforce-admins setting;
- the reviewer is a separate machine account with write access whose token is held only in Privateer's credential directory. GitHub does not let a PR's author approve their own PR, and ships push as the captain's login, so the reviewer must be a different account in any case.

A personal repository cannot restrict who merges (push restrictions are for organisations). On GitHub the gate is therefore "no merge without the reviewer's approval at the head". `pr.merge` is the checked path, not the only one.

**Forgejo** (app ships). Branch protection on the default branch, with names per the Forgejo API (to verify on the installed version):

- `required_approvals: 1`, `enable_approvals_whitelist` with `approvals_whitelist_username: [<reviewer>]`, `dismiss_stale_approvals`, `block_on_rejected_reviews`;
- `enable_merge_whitelist` with `merge_whitelist_usernames: [<merger>]`. Then a ship's own token cannot merge at all, and `pr.merge` (with the merger token) is the only way. This is stronger than GitHub;
- whether admins are held to the protection needs checking on the installed Forgejo. If they are not, the ship's token must not be an admin.

### What it cannot stop

- **One OS account.** The reviewer and merger token files are readable by every process of the captain's user, including the ship's agent and its crews (`cat` of the credential file). So is `privateer.sqlite`, where a `reviews` row could be forged. A model that deliberately reads the token can post its own approval. The gate stops the habit we saw (skipping or downgrading the review), not deliberate theft. The real boundary is a token held under another OS user or on another machine (open question 5). This is the same residual as the crew-role design.
- **An admin forge token.** Ships use the captain's gh login, which owns the repository. That token can turn branch protection off, or edit `CODEOWNERS` through a direct push while protection is off. Enforce-admins stops admin bypass of the rules, but not the rules being deleted. Only a ship token without administration rights stops that (open question 4, which overlaps layer D of the crew-role design).
- **A wrong approval.** The gate proves *who* reviewed *what*, not that the review was right. A prompt injection in the diff or the PR body can mislead the reviewer. The brief puts the PR's own text in the reviewer's context deliberately, because PR #509's harm was in its evidence.
- **Unconfigured repositories.** `pr.merge` refuses a repository missing from `forges.json`, but a ship with an admin token can still use `gh pr merge` there, unless branch protection is in place.
- **Non-code PRs.** The forge cannot tell code from docs, so every PR into the protected branch needs the review (open question 6).

## Interaction with the crew role and the errand flow

- **Crew** (crew-role design): crews cannot push, use gh writes or merge, and `review.start` and `pr.merge` are not on the crew allowlist, so `crew_scope` refuses both by default. A crew's commits make its route an author route, through its job's `model_route`. A crew needs nothing new.
- **Errands.** A review is an errand with three differences: the runtime sets `require`, the session is always fresh, and the verdict is parsed and recorded before the notice is mailed. An ordinary `errand.start` with `require` review at tier 1 is still only advice: it writes no `reviews` row, so it never satisfies `pr.merge`. The review errand cannot approve itself: no forge token is in its environment, posting happens in settlement after its job has ended, and `pr.merge` from an errand is refused. A review errand whose tool use is denied ends through `block_ask` as errands do now. Its verdict is then `failed`, which counts as an attempt.
- **Wakes.** A first mate's wake runs `review.start`, ends its turn, and is woken by the verdict notice, as it is by any errand.

## Failure modes

- **No top-tier route left after excluding author routes.** For example, a crew implemented on `opus` and `opus` is the only tier-1 review route. `review.start` refuses with the author jobs named. The ship asks the captain (open question 3); there is no same-route exception.
- **Every qualifying route is backing off or capped.** `review.start` is refused with the reset time, and the ship retries after it. The merge waits, which is correct.
- **The head moves during the review.** The approval is `stale` and is not posted; a new `review.start` reviews the new head. On GitHub, an approval already posted is dismissed on push by the stale-approval rule.
- **The dispatcher restarts mid-review.** `errand::settle` settles the errand on the next tick. Posting is idempotent through the marker, and `post_state` `pending` or `failed` is retried.
- **The reviewer route is demoted between the review and the merge.** `pr.merge` refuses, and a new review is needed.
- **The forge is unreachable.** `pr.view` failing refuses `review.start` and `pr.merge`. Nothing merges on stale facts.
- **The ship merges with `gh`.** On GitHub the protection refuses it without the reviewer's approval at the head. On Forgejo the merge whitelist refuses it outright.

## Migration

1. B1 lands with no forge writes: the `reviews` table, `review.start`, settlement, bearings, and `pr.merge` checking the record but not yet merging (it says so). Ships can use it at once.
2. The captain creates the reviewer accounts (and the Forgejo merger account), stores their tokens with `credential.set`, and writes `forges.json`.
3. B2 lands: the adapter, posting, and `pr.merge` merging.
4. The captain turns on branch protection per repository, and `forges.set` confirms it. The ship guide changes in the same release as B2.
5. Ship notes that say "one Opus review per code PR" are replaced by the guide line. PRs that are open when B2 lands need a review of their current head like any other.

## Tests (Part B)

Real subprocess boundaries: a fake harness that ends with a scripted reply, and `PRIVATEER_FORGE_ADAPTER` (**new**, test-only override) pointing at a fake adapter that logs argv and stdin and answers from a fixture file. Isolated homes (HOME, PRIVATEER_FLEET, XDG), as in `tests/crew.rs`.

1. `tests/review_gate.rs` `an_approved_head_merges_through_pr_merge` (**new**): `review.start` then settlement then `pr.merge`. The adapter log shows `review.post` with `commit_id` = head and a merge with the expected head.
2. `a_new_head_needs_a_new_review` (**new**): the fixture moves the head after approval. `pr.merge` refuses, and settlement before posting marks the review `stale`.
3. `the_reviewer_must_not_be_an_author_route` (**new**): a crew job on the task with `model_route` = the only tier-1 route makes `review.start` refuse. A crew that runs after the review makes `pr.merge` refuse.
4. `only_a_tier_one_review_route_counts` (**new**): an explicit tier-3 route is refused. A route demoted after the review makes `pr.merge` refuse.
5. `the_verdict_is_parsed_from_the_last_line_only` (**new**): `VERDICT: APPROVE` mid-reply with no last line gives `none`, and nothing is posted.
6. `review_shopping_is_refused` (**new**): a second `review.start` on a head with a finished verdict is refused, and a third after two `none` attempts is too.
7. `posting_is_idempotent_across_a_restart` (**new**): the adapter fails once, the retry posts once, and a marker already present posts nothing.
8. `crews_and_errands_cannot_review_or_merge` (**new**, after crew-role round 2): both commands are refused from a crew job and from an errand job.
9. `a_forge_credential_is_never_given_to_a_launch` (**new**, `tests/tool_bundles.rs`): a bundle registry that names a `forges.json` ref is refused, and so is a launch plan that names one.
10. `plugins/forge.py` against a local `http.server` fixture (**new**, `tests/forge_adapter_test.py`, beside `tests/pi_offline_netns_test.py`), for each forge's request shapes.
11. Manual, recorded in the PR: branch protection on a scratch GitHub repository refuses `gh pr merge` without the reviewer's approval, and the same on a scratch Forgejo repository with the merge whitelist.

## Rounds

- **A1**: `uses`, validation, warnings, display, and the `rank` change. Part A tests 1 and 3.
- **A2**: `require` on launches, the reply's `route`, the `Route:` line, and the guide sentence. Part A tests 2 and 4 to 6.
- **A3**: `usage.summary` by tier and cost. Part A test 7. This round is independent of Part B.
- **B1**: the record and the commands, without forge writes. Part B tests 3 to 6 and 8.
- **B2**: the adapter, `forges.set`, posting and merging. Part B tests 1, 2, 7 and 9 to 11.

Each round is committed after `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` and the affected suites (`--test-threads=1`) pass as supervised heavy jobs.

## Open questions for the captain

1. **Tier as a floor, not an order.** `rank` would stop sorting best tier first (PR 237 rule 3), so that launches stay cheapest-first in your list order, and strength is asked for with `require`. Without this, setting tiers puts `opus` first on every crew list. Confirm.
2. **Use words.** `implement`, `review`, `diagnose`, `design`: enough, or do you want others (for example `evidence` for device work)?
3. **One top-tier route.** `opus` is the only tier-1 route. When `opus` wrote the code (as a crew or as an attended first mate), no route may review it. Options: add a second tier-1 review route on another provider, or keep `opus` off crew lists for code that ships merge. There is no same-route exception unless you ask for one.
4. **Ship forge tokens.** Ships use your admin gh login, which can remove branch protection. Should ships get a fine-grained token without administration rights, and enforce-admins be turned on? This overlaps layer D of the crew-role design.
5. **Where the reviewer token lives.** Under your OS account any ship can read it. Is that acceptable for now (it stops habits, not theft), or should it live under another OS user or another machine and post approvals over `peer.call`?
6. **Docs-only PRs.** Should every PR into the protected branch need the review, or should docs paths be exempted with `CODEOWNERS` patterns (GitHub only; Forgejo cannot)?
7. **Reviewer accounts.** One machine account per forge (GitHub permits one free machine account per person), or a GitHub App? Whether an App's approval satisfies a code-owner rule is unverified here.
