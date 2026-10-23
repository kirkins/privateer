# Design: executing an upgrade (`privateer upgrade <sha>`)

Status: PR 1 (fetch and stage, plus the CI release change) is implemented in `src/releases/fetch.rs`, `scripts/release-layout.sh` and `.github/workflows/ci.yml`; PR 2a (adoption, §4 step 3) in `src/releases/adopt.rs`; PR 2b (switch, restart, health, rollback, prune on one machine) in `src/releases/apply.rs` and `src/releases/prune.rs`; PRs 3-5 are not yet. Read with [versioned releases](../releases.md), which describes the layout and `upgrade status` this builds on. Tracks [issue #76](https://github.com/kirkins/privateer/issues/76).

## As built in PR 1

`privateer upgrade fetch COMMIT [--build] [--source CHECKOUT] [--repo OWNER/NAME]` is operator only. It prepares `releases/COMMIT` in the installation the running binary belongs to and prints one JSON object: `commit`, `release`, `from` (`artifact`, `build` or `existing`), `switched: false`, and the artifact's run, id and digest, or the build's reason and source. It never touches `current`, a launcher, a unit or a process. Where it differs from the sections below:

- **Staging** is `<data>/staging/<id>/releases/<sha>/`, not `releases/.staging-<sha>-<id>/`. Under that path the staged binary already reads as release `<sha>` by its path, so the smoke check sees the identity it will have once published. Staging is on the data directory's filesystem, so publishing is one `rename(2)`; it is removed however staging ends.
- **No links at all.** A release holds plain files and directories only: a symlink anywhere, a device, or a file hard-linked elsewhere is refused (stricter than "no links pointing outside"). The layout script refuses links too, so CI cannot publish one. Tarball names are listed and checked (`<sha>` or under it, relative, no `..`) before anything is extracted.
- **SHA256SUMS** covers every file but itself. A freshly unpacked release must match it exactly. An existing `releases/<sha>` is reused when it is a real directory (not a link), every listed file matches and nothing unlisted is present, `__pycache__` included: the bundled plugins never write bytecode beside themselves (each script importing a sibling sets `sys.dont_write_bytecode`, and adapters run with `-B`). Artifacts from before this change carry no SHA256SUMS and are refused; build those.
- **Smoke check**: `bin/privateer upgrade status` must report `running_commit == <sha>`; `init --json {}` and `status --json {}` on a throwaway home, with HOME, XDG directories, `PRIVATEER_FLEET` and the resource ledger all temporary and no service manager; `privateer-dashboard --help` (the dashboard has no `--version`, and older releases neither) and `privateer-mcp-broker --version`.
- **Source build, already here** (§11 put it in PR 3). It runs when `--build` is given, or when this machine's platform has no artifact (no CI build for it, glibc below the floor, musl, no `getconf`) or the commit has none (no push run on main of `.github/workflows/ci.yml`, the release job failed, was cancelled or was skipped, no artifact, or it expired), and only with `--source CHECKOUT`; without one the command is refused and says why. A release job still queued or running is refused ("CI is still building"), not built in its place. A wrong digest, a bad tarball or a failed smoke check is never a reason to build instead. The checkout is the operator's choice and its `origin` is trusted for main; with `--repo`, its origin URL must name that repository. The build fetches `origin main` in the checkout, refuses a commit not on `origin/main`, exports the commit with `git archive` (the checkout's working tree, index and branch are untouched), runs `cargo build --release --locked --features dashboard` in it with `CARGO_TARGET_DIR=<data>/build/target` while holding `<data>/build/lock` over the build and layout (a concurrent source build is refused, naming the holder), and lays the result out with the copy of `scripts/release-layout.sh` built into the running binary, so any commit is laid out and checked as CI does. The build runs in the foreground in its own process group, bounded to an hour: it does not take the machine's heavy slot yet (PR 3).
- **Overrides for tests**: `PRIVATEER_GH` (the `gh` binary), `PRIVATEER_CARGO`, and `PRIVATEER_GLIBC` (getconf's report, empty for none). `uname -sm` and `python3` are the machine's.
- `upgrade status` does not re-check SHA256SUMS yet.

## As built in PR 2a

`privateer upgrade adopt [--dry-run] [--commit COMMIT] [--allow-different-build]` is operator only and does §4 step 3 alone: it creates `current` when there is none (refusing one that selects another release), links the launchers and rewrites the units, then one `daemon-reload`. It restarts no service, but a running dispatcher's next wakes and jobs start through the replaced launcher and so run the adopted release at once (on Linux, hosted panes count as drifted too). [Versioned releases](../releases.md#adopting-a-machine) describes it. Where it differs from §4:

- **Which release.** The running binary's own, or `--commit` naming a release `upgrade fetch` already published and that still checks out; adopt never fetches. Because new wakes take the release at once, it should be the build installed now: an installed `privateer` that is not byte for byte the release's is refused without `--allow-different-build`, since an unversioned binary records no commit and the state schema has one version, so adopt cannot tell older from newer. `--dry-run` reports that refusal (`would_refuse`) beside the full plan instead of failing.
- **Launchers** are looked for in `~/.cargo/bin`, `~/.local/bin` and the running executable's directory, for all three binaries. A real file is replaced only when proven Privateer's: the running image, or a native executable that answers as itself in a sandbox (`upgrade status` naming that file as its `running_executable`, the dashboard's banner, the broker's version). Scripts, links elsewhere, unproven files and launchers in a directory (or file) this user cannot write are reported, not changed.
- **Units** are found by name and description (agents by their exact `--home HOME serve` or `door run NAME` arguments) in the systemd unit and LaunchAgents directories, doors included; a dispatcher's stop policy is applied as `service stop` does. Only a unit running a launcher this adoption moves (same path, or same device and inode) or a release of this installation is repointed; one pinned to another `privateer` is reported and left. launchd gets no call: a changed agent is read at its next bootstrap.
- **History** gains `kind: "adopt"` (absent for upgrades); `upgrade status` adds `launcher_unversioned`. A rerun that finds the work done but no adoption recorded (its reload or history write failed) reloads and records it.

## As built in PR 2b

`privateer upgrade COMMIT --local [--dry-run]`, `privateer upgrade rollback [--local] [--dry-run]` and `privateer upgrade prune [--keep N] [--dry-run]` are operator only. [Versioned releases](../releases.md#switching-to-a-release) describes them. Where they differ from the sections here:

- **Preconditions, never fixed here.** `releases/COMMIT` must exist and pass `verify()`; `current` must exist; every dispatcher or door unit adopt would repoint must already run `current/bin/privateer` (or a launcher linked to it). A unit pinned to another `privateer` is left out of the round and reported. `launcher_unversioned` is reported, not refused: switching is how such a machine moves on.
- **Detached round with a state file.** The command spawns `upgrade _round ID` with `setsid` and waits on `<data>/upgrade/ID.json`, written beside itself and renamed over after every step; the newest 20 are kept. `upgrade status` gains `round` (the newest state, `in_progress` while `upgrade.lock` is held). The lock is the authority: the caller checks it to fail fast, and the runner takes it for the whole round.
- **Which services.** Dispatchers are the fleet registry's ships whose home is here and whose `serve.lock` is held; doors the registry's doors whose lock is held. A detached one is in the round only when its pid file names the live process holding its lock (the same pid/marks/lock check its restart makes) and that process runs a release of this installation. Not running means left stopped, except for a resumed round (below). A dispatcher agent that would not abandon its process group after the stop-policy rewrite refuses the round up front.
- **Restart** is `service::Restartable::restart`: `systemctl --user restart` (refused unless the unit has `KillMode=process`), launchd `bootout` + wait for the lock + `enable` + `bootstrap` (refused unless the agent abandons its process group), or for a detached process SIGTERM after the same pid/marks/lock checks `service stop` makes, a wait for the lock, and a start from `current`. Units missing the safe stop policy are rewritten first, and one `daemon-reload` runs before any systemd unit restarts, whether or not this round changed a file (`restart` checks `KillMode=process` in the file, so the loaded unit must match it).
- **Health** is a new dispatcher generation holding `serve.lock` whose `dispatcher_release` metadata (the image `serve` records beside its generation, new here) names `releases/COMMIT` and carries `passed_ms`, which `serve` adds after the generation's first completed pass, within 60 s (`PRIVATEER_UPGRADE_HEALTH_SECS` in tests). A generation that releases its lock first, or is replaced by another, fails at once. A release older than this record only has to come back. Any failure switches `current` back and restarts every dispatcher again; doors are restarted only after all dispatchers passed, and a door that does not come back is reported (`switched_with_failures`, non-zero exit), not rolled back for.
- **After the switch nothing aborts.** Step saves are best-effort, the registry is opened before the switch, panes and brokers degrade to a summary line, and any remaining error (a history write) takes the rollback path, which restarts the doors already moved and records what it can.
- **Interrupted rounds.** A state without `outcome` whose runner no longer holds `upgrade.lock` is `interrupted` in `upgrade status`. The same commit resumes one that switched: its plan's dispatchers and doors restart (started if stopped), with its `from` as the rollback target, and the old state is closed `interrupted` with `resumed_by`. A round killed during its own rollback is not resumed.
- **Panes** are reported, not relaunched by the round: the existing catch-up path (`host::upgrade::wanted`) already relaunches a pane behind `current` at its next safe point without waking a sleeping ship, which an explicit relaunch request would. No request is written and no lease moves.
- **History.** `UpgradeRecord` gains `outcome` (`switched`, `switched_with_failures`, `rolled_back`, `failed`) and `rollback_of`; switches record `kind: "upgrade"` or `"rollback"`. Doors appear in `ships` as `door:NAME`. `upgrade rollback` follows only `kind: "upgrade"` records that ended `switched` or `switched_with_failures`.
- **Prune** keeps the newest N by directory modification time (publish time), `current`, the rollback target, the command's own release, and any release named by a live process's executable or working directory (resolved by the kernel: `/proc` on Linux, `proc_pidpath`/`proc_pidinfo` on macOS) or by a path in its arguments (`/proc/PID/cmdline` on Linux, `ps` on macOS, as invoked), rechecked before each removal. The macOS calls are compiled by CI's macOS job but exercised by no test here. A removal renames the release aside inside `releases/` and removes it through `std::fs`.
- **macOS** restarts through launchd as above; the tests use only fake `launchctl`/`systemctl`, so the claim that `bootout` leaves wakes running rests on the same reasoning `service stop` does (agents abandon their process group; job supervisors call `setsid`), not on a run on a Mac.

## 1, 3, 5-9 and 12, in summary

The command is two-phase and detached. Every target machine first prepares an immutable `releases/<sha>`, from the CI artifact when its platform has one (checked against the sha256 digest GitHub reports) and from a source build otherwise. Nothing switches unless every machine prepared. Then each machine in turn switches `current` in one rename, restarts its dispatchers (wakes keep running since #91) and then its doors, relaunches idle hosted panes and reports one line per ship. The captain's own upgrade needs no further approval; an agent needs a `production_approval` bound to the commit. Linked machines get new operator-only gateway commands, and gateway frames (1 MiB) cannot carry an artifact. A Linux aarch64 machine had no CI build before this change, and installed units still run the unversioned `~/.cargo/bin` binary (a one-time move comes in PR 2).

## 2. Fetching and checking the artifact

**Picking the platform.**

- `uname -sm` maps to `macos-arm64` or `linux-x86_64`. CI should also build `linux-aarch64` (see §11).
- Linux also needs `getconf GNU_LIBC_VERSION` to report at least the floor CI checked. Musl, no `getconf`, or an older glibc means a source build.
- `python3` must be on PATH, as the bundled plugins require. If it's missing, the machine is refused with that reason.

**Finding the artifact.** Use the REST API through `gh api`. `gh` is already a dependency in `review/pr.rs`, and an override like `PRIVATEER_GH` lets tests swap it out, the same way `PRIVATEER_SYSTEMCTL` works.

1. `GET /repos/kirkins/privateer/actions/runs?head_sha=<sha>&event=push&branch=main`. Pick the run whose `head_sha` matches exactly, whose repository is `kirkins/privateer` (not a fork) and whose release job concluded `success`. A pull-request run never counts.
2. `GET /actions/runs/{id}/artifacts?name=privateer-<platform>-<sha>`. Refuse an artifact marked `expired`; artifacts last 90 days, so `expires_at` is about 90 days out.
3. Download `/actions/artifacts/{id}/zip` into `<data>/staging/<upgrade-id>/`. Its sha256 must equal the API's `digest` (`sha256:…`, confirmed present on current artifacts). This check is new; `gh run download` does not do it.

**Unpacking.**

- Extract the zip with `python3 -I -m zipfile -e`. Python is already required, and Ubuntu servers often lack `unzip`.
- Then extract the tarball with `tar -xzf`. The tarball must hold exactly one top directory, `<sha>/`. Refuse absolute paths, `..`, and links pointing outside it.
- The layout must be complete: every name in `releases::BINARIES` is executable, and `share/privateer/plugins/*/manifest.json` exists.

**Per-file integrity.** CI should also write `<sha>/SHA256SUMS` covering every file. The installer checks it after unpacking. `upgrade status` can check it again later, which catches a release changed at rest. Artifact attestations (`gh attestation verify`) can come later; the digest over TLS from api.github.com is enough for now.

**Smoke check, run before anything moves:**

- the staged `bin/privateer upgrade status` reports `running_commit == <sha>`;
- `bin/privateer --home <tmp> init` followed by `status` succeeds on a throwaway home with `PRIVATEER_FLEET` set to a temp registry;
- `bin/privateer-dashboard --version` runs.

An exec-format error or a missing loader fails here, so a wrong-platform build never reaches the switch.

## 4. Install order

Each step either completes or leaves `current` untouched.

1. **Stage** into `releases/.staging-<sha>-<id>/` on the same filesystem, then check and smoke it.
2. **Publish** by renaming the staging directory to `releases/<sha>`. If `releases/<sha>` already exists, it is reused only when its `SHA256SUMS` checks out (releases never change). A mismatch is an error that names the directory; nothing is overwritten.
3. **Adopt, once per machine** (needed here):
   - Make `~/.cargo/bin/privateer` and `privateer-dashboard`, and `~/.local/bin/*` where they are real files, into symlinks to `current/bin/<name>`. Each is made as `<name>.new`, then renamed over the old one.
   - This keeps every recorded path valid without rewriting it: unit `ExecStart` lines, `peer.executable`, `AGENTS.md`, Claude permission rules.
   - It also rewrites any dispatcher unit or plist whose `ExecStart` isn't through `current`, and applies `dispatcher_stop_policy`, followed by one `daemon-reload`.
   - Adoption is reported as its own line, and `--dry-run` shows it.
   - A later `cargo install` turns a launcher back into a file. `upgrade status` flags that as `launcher_unversioned`.
4. **Switch** `current` with the existing atomic recipe: make `current.new`, then `rename(2)` it over `current`. In Rust that is `std::os::unix::fs::symlink` plus `fs::rename`, which replaces the link itself on both Linux and macOS.
5. **The MCP broker is left alone.** Its stable path and hash must not change at upgrade (`docs/mcp-broker.md`). The summary names each instance whose installed broker differs from the new release's copy, as a captain action: `mcp-broker stop --name N`, then `install`, when its windows are idle.

PR 1 does steps 1 and 2 only.

## 10. Tests

All tests run on disposable `HOME`, `XDG_*`, `PRIVATEER_FLEET` and data directories. None touches the real install.

**Fake artifacts** (PR 1, `tests/upgrade_fetch.rs`):

- A fake `gh` (`PRIVATEER_GH`) serves canned run and artifact JSON plus a zip.
- The zip is built in the test from a copied test binary placed at `releases/<sha>/bin/privateer`. Path-based identity makes it report `<sha>`, as the #79 tests do.
- Cases: wrong digest, expired artifact, path traversal in the tarball, a missing binary, wrong platform (a non-executable file fails the smoke check), and glibc below the floor (`PRIVATEER_GLIBC` override) triggering a build. PR 1 adds a pull request's run, a fork's run, a failed release job, a link, a file outside SHA256SUMS and one changed after it, reuse of an intact release and refusal of one changed at rest.

**Fake build** (PR 1):

- `PRIVATEER_CARGO` points at a script that lays out copies of the test binary, over a fixture git repo with `origin`.
- Cases: a commit not on main is refused, a failing build leaves `current` unchanged, and the checkout's working tree is untouched.

**Fake units:**

- Existing `PRIVATEER_SYSTEMCTL` and `PRIVATEER_LAUNCHCTL` fakes record the order: daemon-reload before restart, dispatchers before doors, `KillMode=process` checked.
- Adoption rewrites `ExecStart` and the launchers atomically.
- With `PRIVATEER_SERVICE=detached`:
  - a **real dispatcher subprocess** with a wake running survives the upgrade, and the wake completes once;
  - mail sent during the restart is delivered exactly once;
  - a dispatcher that exits on the new release triggers auto-rollback.

**Panes:**

- An idle recorded pane is relaunched.
- A busy pane is reported as catching up, and its lease is untouched.

**Fake linked machine:**

- `machine add … transport: local` with a second `HOME` and data directory.
- Cases: probe/prepare/apply over the real gateway; a chunked push when the remote has no `gh`; a prepare failure on the remote means nothing switches anywhere; a gateway without `upgrade.probe` is skipped.

**Approvals:**

- No approval is refused.
- An approval for another commit or machine set is refused.
- A second use is refused.
- An upgrade started from inside a wake doesn't deadlock (the runner is outside the cgroup, the wake survives, and the reply lands in its thread).

**Retention and rollback:**

- Prune keeps a release a live process runs.
- Rollback restores the previous commit and records `rollback_of`.

## 11. PR slicing

1. **Fetch and stage, no switch.** (Implemented.)
   - CI: `scripts/release-layout.sh`, `SHA256SUMS`, and a `linux-aarch64` job on `ubuntu-24.04-arm`. The repo is public, so the arm runner is free, and an aarch64 Linux machine moves onto artifacts.
   - `privateer upgrade fetch <sha>`: platform and glibc check, API lookup, digest, unpack, sums, smoke check, publish to `releases/<sha>`.
   - Tests: fake `gh` cases and the smoke check.
   - Value: removes the copying with no effect on production.
2. **Apply locally.**
   - `privateer upgrade <sha> --local`: adoption, atomic switch, `service::restart`, door restart, pane relaunch, summary, history, auto-rollback, `upgrade rollback`, `upgrade prune`; operator only, run detached with a state file.
   - Tests: fake units, the real-subprocess wake survival, mail delivered once, panes, rollback and prune.
   - Value: does the captain's whole round on one machine. This is the largest PR; adoption split off first as PR 2a (implemented: `privateer upgrade adopt`, `tests/upgrade_adopt.rs`), and the rest is PR 2b (implemented: `upgrade COMMIT --local`, `rollback`, `prune`, `tests/upgrade_apply.rs`; panes are reported and left to their own catch-up rather than relaunched by the round).
3. **Source-build fallback.**
   - `--build` and automatic fallback, worktree build with the heavy slot, the shared layout script.
   - Tests: fake cargo, refusals, `current` unchanged on failure.
   - PR 1 already has `--build`, the automatic fallback, the shared script and these tests; what remains is taking the machine's heavy slot for the build.
4. **Linked machines.**
   - Gateway `upgrade.probe`, `prepare`, `apply`, `rollback`, `progress` and `chunk`; all machines prepared before any switch; per-machine records.
   - Tests: fake local-transport machine, chunked push, all-or-nothing.
   - The captain must install it by hand once on each linked machine.
5. **Agent requests and doors.**
   - `upgrade.start` behind a commit-bound `production_approval`, a Telegram `/upgrade <sha>`, a dashboard action, mail outcomes.
   - Tests: the approval cases and no deadlock from inside a wake.
