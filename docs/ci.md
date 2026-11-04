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
| slow-3 | ~560 | library half `door::` (323 of 1469 tests, 339s locally), plus crew_park, upgrade_fetch, mcp_broker, review_gate, scheduling, cross_ship_waits, recovery_pilot |
| slow-4 | ~555 | python_suites, upgrade_linked, machines, dashboard_fleet, dashboard, dashboard_yaml, cursor_attended, cross_machine_waits, pi_attention, pi_calm, pi_follow, pi_quiet |
| rest | ~575 | library complement (`--skip door::`), bins, doc tests and the remaining suites, including desk_placement and placement |

The two library halves are complements by construction (323 + 1146 = 1469
listed tests), and each runs as its own cargo invocation so a filter can never
hide a suite's tests. A fifth runner costs one more job of setup, cache restore
and compile (about 5–10 minutes by the fits below), so the expected macOS
whole-job times are roughly 15–18 minutes each against the 25-minute limit —
typical runs under 70% of the limit, with the previous worst case (slow-1 at
90–100%) gone. This is a projection from local timings plus whole-job macOS
anchors, not a measurement of the changed workflow; the per-invocation times
the shard runner appends to the Actions job summary will show the real split.
If shards still run close to the limit, the next levers are a sixth shard or
cutting per-job overhead, not moving suites again.
