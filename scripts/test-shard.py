#!/usr/bin/env python3
"""Run one shard of `cargo test --locked --all-features`, for CI jobs in parallel.

Each named shard is the integration suites listed for it below. `rest` is
every other target cargo metadata lists (the binaries, every other
integration suite) and the doc tests, so a suite added later lands in `rest` and
is never skipped. Together the shards run exactly what
`cargo test --all-features` runs.
"""
import json
import os
import subprocess
import sys
import time

# Balance by whole-job time, including slow-1's MCP/pilot checks and slow-2's
# clippy. See docs/ci.md for the measured baseline and how to review it again.
SHARDS = {
    # slow-1 keeps the MCP door checks, the dashboard smoke, the pilot and the
    # minimal-build check (see test-shards.yml), so it takes only light suites.
    "slow-1": ("activation", "attended", "files", "cli", "input", "watch"),
    "slow-2": ("door", "hosted", "permissions", "workspaces", "plugins",
               "tool_bundles", "crew_routes", "mail", "fleet", "jobs"),
    "slow-3": ("crew_park", "upgrade_fetch", "mcp_broker", "review_gate",
               "scheduling", "cross_ship_waits", "recovery_pilot"),
    "slow-4": ("python_suites", "upgrade_linked", "machines", "dashboard_fleet",
               "dashboard", "dashboard_yaml", "cursor_attended",
               "cross_machine_waits", "pi_attention", "pi_calm", "pi_follow",
               "pi_quiet"),
}
# The library is one `cargo test --lib` run's worth of time on its own, so it is
# split in two by module-path filter: LIB_SHARD runs the tests whose full name
# contains one of LIB_INCLUDE; LIB_SKIP_SHARD runs the complement (one --skip
# per prefix). The filters are substrings of the full test path and a prefix
# ending in `::` matches exactly that module's tests, so the two halves
# partition the library: every test matches the include list or is skipped.
LIB_SHARD = "slow-3"
LIB_INCLUDE = ("door::",)
LIB_SKIP_SHARD = "rest"
USAGE = "usage: test-shard.py " + "|".join([*SHARDS, "rest"]) + " [--print]"


def targets():
    out = subprocess.run(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
                         check=True, capture_output=True, text=True).stdout
    return [t for p in json.loads(out)["packages"] for t in p["targets"]]


def lib_guard(shard_names):
    """The library halves must be a partition run by two different shards."""
    if LIB_SHARD not in shard_names or LIB_SKIP_SHARD not in shard_names:
        raise SystemExit("test-shard: no shard pair runs the library")
    if LIB_SHARD == LIB_SKIP_SHARD:
        raise SystemExit("test-shard: the library halves share a shard")
    if not LIB_INCLUDE:
        raise SystemExit("test-shard: the library include list is empty")
    bare = sorted(p for p in LIB_INCLUDE if not p.endswith("::"))
    if bare:
        raise SystemExit(f"test-shard: library prefixes must end in '::': {' '.join(bare)}")


def commands(shard):
    shard_names = [*SHARDS, "rest"]
    lib_guard(shard_names)
    found = targets()
    # A new kind of target (an example, a bench) would fall outside every
    # shard; refuse it until the shards say where it runs.
    other = sorted(t["name"] for t in found if t["kind"] not in (["lib"], ["bin"], ["test"]))
    if other:
        raise SystemExit(f"test-shard: no shard runs {' '.join(other)}")
    tests = sorted(t["name"] for t in found if t["kind"] == ["test"])
    named = [name for suites in SHARDS.values() for name in suites]
    duplicates = sorted(name for name in set(named) if named.count(name) > 1)
    if duplicates:
        raise SystemExit(f"test-shard: suites assigned twice: {' '.join(duplicates)}")
    missing = sorted(set(named) - set(tests))
    if missing:
        raise SystemExit(f"test-shard: no such suite: {' '.join(missing)}")
    base = ["cargo", "test", "--locked", "--all-features"]
    suite_args = [arg for name in SHARDS.get(shard, ()) for arg in ("--test", name)]
    lib_half = ["--lib", "--", *LIB_INCLUDE] if shard == LIB_SHARD else None
    lib_rest = ["--lib", "--", *(a for p in LIB_INCLUDE for a in ("--skip", p))] \
        if shard == LIB_SKIP_SHARD else None
    if shard in SHARDS:
        # A filter applies to every binary in one cargo run, so a filtered
        # library half never shares a run with unfiltered suites: a suite test
        # that happened to match the filter would otherwise run in no shard.
        selected = []
        if lib_half:
            selected.append(base + lib_half)
        if suite_args:
            selected.append(base + suite_args)
        return selected
    if shard == "rest":
        selected = []
        if lib_rest:
            selected.append(base + lib_rest)
        bins = ["--bins"]
        bins += [arg for name in tests if name not in named for arg in ("--test", name)]
        selected.append(base + bins)
        # Doc tests cannot share a run with other targets.
        selected.append(base + ["--doc"])
        return selected
    raise SystemExit(USAGE)


def main(argv):
    if len(argv) not in (2, 3) or (len(argv) == 3 and argv[2] != "--print"):
        raise SystemExit(USAGE)
    for command in commands(argv[1]):
        print("+ " + " ".join(command), flush=True)
        if len(argv) == 2:
            start = time.monotonic()
            code = subprocess.run(command).returncode
            elapsed = time.monotonic() - start
            report = f"{argv[1]}: {elapsed:.1f}s (exit {code}) — `{' '.join(command)}`"
            print(report, flush=True)
            if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
                with open(summary, "a") as output:
                    output.write(f"- {report}\n")
            if code:
                return code
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
