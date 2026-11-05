# Continuous integration

[Privateer CI](../.github/workflows/ci.yml) runs on pull requests, main pushes
and manual dispatch. A newer run of the same PR cancels its superseded run
using [workflow concurrency](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency).
Each push and dispatch uses a unique group, preserving main's per-commit
[release artifacts](releases.md).

## Documentation and research

Every run checks Markdown heading structure, empty links and undefined reference
labels in root Markdown files and `docs/**/*.md`. The selected rules live in
[the Markdown config](../.markdownlint-cli2.jsonc); they leave prose wrapping and
table formatting to the author. These checks validate reference syntax, not
external URL reachability or link fragments.

The offline [CI helper](../scripts/ci.py) parses JSON under `docs/` and compiles
Python source under `scripts/research/` without importing or executing it. This
catches malformed measurements and syntax errors without installing research
dependencies, downloading tokenizer data or invoking agents. It does not
reproduce experiments or validate their conclusions.

A PR containing only root Markdown files, `docs/**/*.md`,
`docs/research/**/*.json` and `scripts/research/**/*.py` uses these lightweight
checks. All other paths, including workflows, plugins, runtime fixtures,
`docs/examples` JSON and new kinds of research programs, require both platforms'
Rust suites. Empty PR diffs also require the full suite. Detection uses the Git
merge base and NUL-delimited paths with rename detection disabled, so deleted
runtime files, moves into docs and large PRs cannot hide runtime changes.

The existing required checks, `runtime (ubuntu-latest)` and
`runtime (macos-latest)`, pass only after the lightweight checks succeed and
either their runtime suites pass or the classifier explicitly selects a
lightweight PR and the runtime jobs are skipped. Detection failures, cancelled
checks and unexpected skips fail the gates. Main pushes and manual dispatch
always run the full suites and retain their release behavior.

Run the lightweight checks locally with:

```sh
python3 -m unittest discover -s scripts -p 'test_ci*.py'
python3 scripts/ci.py check-docs
npx --yes markdownlint-cli2@0.24.0 '*.md' 'docs/**/*.md'
```

## Cargo cache and shards

The pinned `Swatinem/rust-cache` action restores Cargo downloads and dependency
build outputs. Its key includes the toolchain, target architecture, manifests,
lockfile and compiler environment. Test shards share a cache per runner OS;
only successful main `slow-2` jobs save it, avoiding competing partial writers.
PRs restore the main cache. Release jobs have separate keys per platform and
save only on main. Privateer's own binaries are rebuilt from the checkout.

The [shard runner](../scripts/test-shard.py) assigns every integration target
exactly once. Newly added integration suites go to `rest`; a missing or duplicate
named suite, or an unsupported target kind, is an error. The library is split
in two complementary halves by module-path filter (`door::` and its skip
complement), so no single shard carries the whole library; binary tests and doc
tests run in `rest`. A shard pair that stops partitioning the library is an
error, and [the shard tests](../scripts/test_ci_shards.py), which the docs job
runs on every PR, also check that the workflow matrix lists every shard. Both
platforms retain all-feature
tests, Linux's crew environment and the existing MCP, dashboard, pilot, lint
and minimal-build checks.

The rebalance uses [successful run 40794051041](https://github.com/kirkins/privateer/actions/runs/40794051041)
at `78f337e228b2adad03b2eb77a1ee54844e9e29c0`. These are whole-job durations,
including setup and compilation, excluding time queued for a runner:

| Shard | Linux baseline | macOS baseline | Reassignment |
| --- | ---: | ---: | --- |
| slow-1 | 5m59s | 10m19s | Takes library tests from rest |
| slow-2 | 6m10s | 12m03s | Keeps its tests and fmt/clippy |
| slow-3 | 7m11s | 10m18s | Takes desk_placement and machines from rest |
| rest | 8m47s | 21m36s | Keeps binaries, other integration suites and doc tests |

On macOS the moved suites took about 4m26s (library), 2m25s (desk placement)
and 1m53s (machines).

**Rebalance, 2026-11-02.** By [run 41499939051](https://github.com/kirkins/privateer/actions/runs/41499939051)
at `0e535467ebaf574cebac71ed73a37200fcb19501`, macOS library tests had grown to
10m38s and `slow-1` ran 23m04s against its 25-minute limit (it was cancelled on
main and twice on one PR that day). Whole-job macOS times were slow-1 23m04s,
slow-2 13m59s, slow-3 18m08s and rest 16m53s (Linux 17m06s, 6m48s, 12m12s and
8m58s). `crew_park` (4m12s on macOS) moved to `slow-2` and `dashboard_fleet`
(1m32s) to `rest`, which should bring the macOS jobs to roughly 17–19 minutes
each. The MCP checks stay in `slow-1`: a job of their own would add a fifth
macOS runner per run, and macOS queueing is already the longer wait. Moving them suggests roughly 13–15 minutes per job at
those speeds, before cache savings. This is an estimate from one run, not a
measurement of the changed workflow. Compilation and runner variability affect
the result. Each shard now appends Cargo invocation times and exit status to
the Actions job summary. Use those summaries alongside per-suite timings in
the logs to assess later balance; include the extra checks attached to each
shard when comparing whole-job times.

**Rebalance, 2026-11-04.** After the 11-02 rebalance `slow-1` kept growing:
on [run 41827570345](https://github.com/kirkins/privateer/actions/runs/41827570345)
(da6c78f4) macOS whole-job times were slow-1 22m35s, slow-2 18m28s, slow-3
20m09s and rest 17m15s — four jobs sharing about 78 minutes of work against a
25-minute limit each, with slow-1 cancelled at the limit on 11-03 (PRs 677 and
680). Moving suites between four shards cannot fit that total under about
70% of the limit, so the workflow gained a fifth shard, `slow-4`. Every
integration suite was timed locally on the 11-04 checkout (one `cargo test
--test <suite>` per suite, and `--lib` per top-level module), and the suites
were dealt out so all five shards carry roughly equal measured work:

| Shard | Measured work (local, s) | Content |
| --- | ---: | --- |
| slow-1 | ~550 | MCP door checks, dashboard smoke, pilot, minimal-build check, plus activation, attended, files, cli, input, watch |
| slow-2 | ~560 | fmt and clippy, plus door, hosted, permissions, workspaces, plugins, tool_bundles, crew_routes, mail, fleet, jobs |
| slow-3 | ~560 | library half `door::` (321 of 1430 tests, 339s locally), plus crew_park, upgrade_fetch, mcp_broker, review_gate, scheduling, cross_ship_waits, recovery_pilot |
| slow-4 | ~555 | python_suites, upgrade_linked, machines, dashboard_fleet, dashboard, dashboard_yaml, cursor_attended, cross_machine_waits, pi_attention, pi_calm, pi_follow, pi_quiet |
| rest | ~575 | library complement (`--skip door::`), bins, doc tests and the remaining suites, including desk_placement and placement |

The two library halves are complements by construction (CI listed 321 + 1109
= 1430 tests), and each runs as its own cargo invocation so a filter can never
hide a suite's tests. A fifth runner costs one more job of setup, cache restore
and compile (about 5–10 minutes by the fits below), so the expected macOS
whole-job times are roughly 15–18 minutes each against the 25-minute limit —
typical runs under 70% of the limit, with the previous worst case (slow-1 at
90–100%) gone. This is a projection from local timings plus whole-job macOS
anchors, not a measurement of the changed workflow; the per-invocation times
the shard runner appends to the Actions job summary will show the real split.
If shards still run close to the limit, the next levers are a sixth shard or
cutting per-job overhead, not moving suites again.

**Rebalance, 2026-11-04, second pass.** PR 717's first run with `slow-4`
([run 41848822540](https://github.com/kirkins/privateer/actions/runs/41848822540)
at `da76f98c`) timed the new macOS `slow-4` out at 25m49s. The Linux sums of
each shard's Cargo invocations were slow-1 7m32s, slow-2 8m52s, slow-3 12m10s,
slow-4 12m22s and rest 7m44s; macOS `slow-1` ran its whole job in 11m01s.
Inside slow-4 the Linux times were upgrade_linked 310s, python_suites 220s,
machines 83s, dashboard_fleet 47s and every other suite under 10s. macOS runs
those two heavy suites at roughly 2.2 times their Linux time (crew_park's
measured 4m12s from 11-02 matches 2.2 x ~115s), so upgrade_linked and
python_suites together were about 18-20 minutes of suite time before setup:
they must never share a shard again. The shard runner and the shard tests now
refuse a map that puts them together or leaves either of them out of the named
shards (into `rest`), the same way they refuse a duplicate.

With the pair split apart, five shards cannot hold the measured work with
every projected macOS job under about 17 minutes (~70% of the 25-minute
limit), so the workflow gained a sixth shard, `slow-5`, on both platforms —
the cost is one more Linux and one more macOS runner per runtime run.

| Shard | Linux work in | Projected macOS | Content |
| --- | ---: | ---: | --- |
| slow-1 | 7m32s + 2m17s moved in | ~15m50s | MCP door checks, dashboard smoke, pilot and minimal-build check, plus activation, attended, files, cli, input, watch, machines, dashboard_fleet |
| slow-2 | 8m52s (unchanged) | ~14-16m | fmt and clippy, plus door, hosted, permissions, workspaces, plugins, tool_bundles, crew_routes, mail, fleet, jobs |
| slow-3 | 12m10s less crew_park | ~13-16m | library half `door::`, plus upgrade_fetch, mcp_broker, review_gate, scheduling, cross_ship_waits, recovery_pilot |
| slow-4 | 310s + <40s of pi suites | ~16m50s | upgrade_linked, plus pi_attention, pi_calm, pi_follow, pi_quiet |
| slow-5 | 220s + ~115s | ~16m20s | python_suites, plus crew_park |
| rest | 7m44s + <40s moved in | ~14-16m | library complement, bins, doc tests, the remaining suites plus dashboard, dashboard_yaml, cursor_attended, cross_machine_waits |

Projections use macOS ≈ 2.2 x Linux suite time + about 4 minutes of setup;
slow-1 anchors on its measured 11m01s plus 2.2 x 130s of moved suites, and
slow-3's row is bounded by the same map running green on macOS at `da76f98c`
with crew_park still aboard — a shard's invocation sum also carries compile
time that does not scale 2.2x, which is why the raw sums overstate slow-3.
This is a projection, not a measurement of the changed workflow; the
per-invocation times in the Actions job summaries will show the real split,
with slow-4 and slow-5 the shards to watch first. If shards still run close
to the limit after this, the next levers are a seventh shard or cutting
per-job overhead, not moving suites again.

**Rebalance, 2026-11-04, third pass (measured).** The first macOS jobs on
the six-shard map that got runners (run 41859340431, attempt 2) measured
slow-2 at 19m28s whole-job (78% of the limit; suites: door 414s, hosted
180s, workspaces 149s, plugins 61s, the rest under 50s each) and slow-4 at
5m53s (upgrade_linked 288s on macOS, close to its Linux time, not 2.2x).
`hosted` and `workspaces` moved from slow-2 to slow-4, projecting macOS
slow-2 at about 14 minutes and slow-4 at about 11-12 minutes.

## Sandbox qualification

[The sandboxed-launch contract](design/codex-sandboxed-launch.md) is qualified
by tests that run first-mate CLI calls inside a Codex-like sandbox:
`tests/sandboxed_launch.rs` and the review-errand case in
`tests/review_gate.rs`. On Linux they use `bwrap` with its own PID namespace
and `/proc`; on macOS, `/usr/bin/sandbox-exec` with Codex's base Seatbelt
rules. The test-shard workflow installs `bubblewrap` on Linux runners and
lifts Ubuntu's AppArmor restriction on unprivileged user namespaces, which
`bwrap --unshare-user` needs. Both steps are CI only. It also sets
`PRIVATEER_SANDBOX_QUALIFICATION=1`, so a runner that cannot make the sandbox
fails these tests instead of skipping them, as they do on a machine without
one.
