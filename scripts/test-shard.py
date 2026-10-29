#!/usr/bin/env python3
"""Run one shard of `cargo test --locked --all-features`, for CI jobs in parallel.

Each named shard is the integration suites listed for it below. `rest` is
every other target cargo metadata lists (the library, the binaries, every other
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
    "slow-1": ("crew_park", "dashboard_fleet"),
    "slow-2": ("door", "hosted", "permissions", "workspaces"),
    "slow-3": ("python_suites", "upgrade_linked", "desk_placement", "machines"),
}
LIB_SHARD = "slow-1"
USAGE = "usage: test-shard.py " + "|".join([*SHARDS, "rest"]) + " [--print]"


def targets():
    out = subprocess.run(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
                         check=True, capture_output=True, text=True).stdout
    return [t for p in json.loads(out)["packages"] for t in p["targets"]]


def commands(shard):
    if LIB_SHARD not in SHARDS:
        raise SystemExit("test-shard: no shard runs the library")
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
    if shard in SHARDS:
        selected = ["--lib"] if shard == LIB_SHARD else []
        return [base + selected + [arg for name in SHARDS[shard] for arg in ("--test", name)]]
    if shard == "rest":
        selected = ["--bins"]
        selected += [arg for name in tests if name not in named for arg in ("--test", name)]
        # Doc tests cannot share a run with other targets.
        return [base + selected, base + ["--doc"]]
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
