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
named suite, or an unsupported target kind, is an error. Library tests run in
`slow-1`, binary tests and doc tests in `rest`. Both platforms retain all-feature
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
and 1m53s (machines). Moving them suggests roughly 13–15 minutes per job at
those speeds, before cache savings. This is an estimate from one run, not a
measurement of the changed workflow. Compilation and runner variability affect
the result. Each shard now appends Cargo invocation times and exit status to
the Actions job summary. Use those summaries alongside per-suite timings in
the logs to assess later balance; include the extra checks attached to each
shard when comparing whole-job times.
