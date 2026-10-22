# Versioned releases and upgrade status

This is the release-identity foundation for [issue #76](https://github.com/kirkins/privateer/issues/76), not the fleet upgrader. `privateer upgrade status`, `privateer upgrade fetch` and `privateer upgrade adopt` (below) are available; switching, draining/restarting, pruning, rollback, and approval-bound upgrade requests are not implemented yet ([the design](design/upgrade-execute.md)). Do not use an ad hoc symlink switch as a safe fleet upgrade.

## Layout

The installation data directory is `$XDG_DATA_HOME/privateer`, defaulting to `~/.local/share/privateer`:

```text
privateer/
  releases/
    <full-lowercase-git-object-id>/
      bin/privateer
      bin/privateer-dashboard
      bin/privateer-mcp-broker
      share/privateer/plugins/
  current -> releases/<full-lowercase-git-object-id>
```

Both SHA-1 and SHA-256 full Git object IDs are accepted; branch names, abbreviations, and paths are not release identities. `releases::Release` defines the layout and the stable `current/bin/{privateer,privateer-dashboard}` entry points. A future installer must publish complete immutable releases (all three binaries, the dashboard built with its feature, and their bundled plugins), then switch `current` atomically. `privateer-mcp-broker` is never run from a release: `privateer mcp-broker install` copies it to the one unversioned path `$XDG_DATA_HOME/privateer/mcp-broker/bin/privateer-mcp-broker`, so upgrades leave its path and hash alone (see [the MCP broker](mcp-broker.md)). To switch it by hand, make a new link beside it and rename it over `current` in one rename(2): with GNU coreutils, `ln -sfn releases/<id> current.new && mv -T current.new current`; on macOS, `ln -sfh releases/<id> current.new && mv -h current.new current`. Plain `mv current.new current` follows the old link on macOS and moves the new link *into* the old release directory, leaving `current` unchanged, and `ln -shf releases/<id> current` is not atomic (there is a brief moment with no `current`). `python3 -c 'import os; os.replace("current.new", "current")'` is an atomic rename on either system. The `-n`/`-h` on the first `ln` keeps a `current.new` left by a failed attempt from swallowing the new link. Stable launchers such as `~/.local/bin/privateer` and service ExecStart paths should address `current/bin/privateer`, not a commit directory; `upgrade adopt` (below) makes them do so once per machine.

The OS-reported **real executable path**, not the launcher symlink or source build path, identifies the running release. A release binary loads plugins **only** from its own `share/privateer/plugins`. It neither follows `current` for adapters nor falls back to the build checkout, ancestor directories, or `PRIVATEER_PLUGINS`. Missing release plugins are an error. Thus moving `current` cannot change the Python code under an old dispatcher.

Agent control argv, generated instructions, and newly persisted peer/service commands go through `current/bin/privateer`. New dashboard launches from a release similarly use `current/bin/privateer-dashboard`, without falling back to an unrelated PATH binary. On Linux, internal job supervisors still reexec the running image through `/proc/self/exe`; their protocol must match their dispatcher. Keep the old release's files while its processes run. The retention default for the future orchestrator is three releases; there is **no pruning operation yet**.

Legacy `cargo install` and source/target builds remain supported: outside a recognized release layout, existing plugin lookup (explicit `PRIVATEER_PLUGINS`, `share/privateer/plugins` beside the binary's own `bin`, then the build checkout whose `Cargo.toml` names the `privateer` package and nothing above it) and executable replacement behavior remain unchanged. They are reported as an unversioned running binary, not assigned a guessed Git commit. A source checkout changing after compilation is not evidence of a binary's commit.

CI builds this layout with `scripts/release-layout.sh` on every push to main as a downloadable artifact: `privateer-macos-arm64-<commit>` (macOS on Apple silicon), `privateer-linux-x86_64-<commit>` and `privateer-linux-aarch64-<commit>` (Linux, built on Ubuntu 24.04 and checked to need glibc 2.39 or older, so they run on Ubuntu 24.04 and newer). Each holds one tarball of one `<commit>/` directory as above, plus `<commit>/SHA256SUMS` covering every other file in it. A release holds plain files and directories only, no links. The bundled plugins need `python3` on the machine.

## Fetching a release

```sh
privateer upgrade fetch <commit> [--build] [--source CHECKOUT] [--repo OWNER/NAME]
```

Operator only. It prepares `releases/<commit>` in the installation the running binary belongs to and switches nothing: `current`, launchers, service units and running processes are untouched. A complete release is staged under `staging/<id>/`, checked, and only then renamed into `releases/`, so `releases/<commit>` is either complete or absent.

- **From CI.** When `uname -sm` names a platform CI builds (and, on Linux, `getconf GNU_LIBC_VERSION` reports glibc 2.39 or newer), `gh api` finds the push run on main of `.github/workflows/ci.yml` in the repository (default `kirkins/privateer`; never a pull request's, a fork's or another workflow's run) whose release job for this platform succeeded, and its unexpired artifact. While that job is still queued or running the fetch is refused ("CI is still building"), never built from source instead; only a commit with no such run, or whose release job failed, was cancelled or was skipped, falls back to a build. The download's sha256 must equal the digest GitHub reports. The zip is unpacked with `python3 -I -m zipfile`, its tarball's names are checked to lie under `<commit>/` before `tar` extracts it, and the result must hold every binary, plugin manifests and a `SHA256SUMS` matching every file exactly.
- **From source.** With `--build`, or when CI has nothing for this platform or commit, and only with `--source CHECKOUT`: the commit must be on the checkout's `origin/main` (fetched first). It is exported with `git archive`, leaving the checkout's working tree, index and branch alone, built with `cargo build --release --locked --features dashboard` into `<data>/build/target`, and laid out by the same script CI runs. Builds share that target directory, so one source build runs at a time: the build and its layout hold `<data>/build/lock`, and a second fetch that needs a build meanwhile is refused, naming the holder. Without `--source` such a machine is refused, with the reason.
- **The checkout is the operator's choice.** A source build trusts the checkout's own `origin`: it is what `origin/main` means, and what is built. `--repo` names where CI artifacts come from and does not change the checkout's remote; when `--repo` is given, the checkout's origin URL must name that repository (`OWNER/NAME` as its last two path segments, `.git` aside) or the build is refused. Without `--repo`, nothing checks the origin, so build only from a checkout you trust. A wrong digest, a bad tarball or a failed check never falls back to a build.
- **Smoke check.** Before publishing, the staged `bin/privateer` must report itself as `<commit>` in `upgrade status`, initialise and read a throwaway home, and the dashboard and broker binaries must run, so a wrong-platform build fails here.
- **Releases never change.** An existing `releases/<commit>` is reused only when it is a real directory (a link to one is refused) whose `SHA256SUMS` still checks out exactly: every listed file matches and nothing else is present, `__pycache__` included. Otherwise the command fails naming the directory and overwrites nothing. The bundled plugins never write bytecode into a release: every plugin script that imports another sets `sys.dont_write_bytecode` before doing so, adapters run as `python3 -B -E -s`, and the smoke check runs with `PYTHONDONTWRITEBYTECODE=1`.

The build runs in the foreground and does not yet take the machine's heavy slot. `PRIVATEER_GH`, `PRIVATEER_CARGO` and `PRIVATEER_GLIBC` replace `gh`, `cargo` and getconf's report in tests.

## Adopting a machine

```sh
privateer upgrade adopt [--dry-run] [--commit COMMIT] [--allow-downgrade]
```

Operator only, once per machine. A machine installed with `cargo install` (or by copying binaries into `~/.local/bin`) runs an unversioned binary, and every recorded path names it: unit `ExecStart` lines, `peer.executable`, `AGENTS.md`, permission rules. Adoption keeps those paths valid by making them resolve through `current/bin/<name>`, without rewriting the records:

- **The release.** The running binary's own release when it runs from `releases/<commit>`; otherwise `--commit COMMIT`, whose `releases/<commit>` must already exist and pass the same check as `upgrade fetch` (`SHA256SUMS` exact). A missing release is refused with "run `privateer upgrade fetch COMMIT` first": adopt builds and downloads nothing. Choose the commit of the build installed now: new wakes and jobs run the adopted release at once (below). Every proven installed `privateer` that is a real file is compared with the release's `bin/privateer`, and the report lists each under `installed` with `same_build`. Adopt cannot order two builds (an unversioned binary's `upgrade status` reports no commit, and the state schema has a single version), so any installed `privateer` that is not byte for byte the release's is refused unless `--allow-downgrade` says the release is not older. A CI artifact and a local `cargo install` of the same commit differ in their bytes, so adopting one over the other needs the flag too.
- **`current`.** Created pointing at `releases/<commit>` (a relative link made as `current.new` and renamed over `current`) when there is none. When it already selects that release it is left alone; when it selects another, adoption is refused: switching is not adoption's.
- **Launchers.** `privateer`, `privateer-dashboard` and `privateer-mcp-broker` in `~/.cargo/bin`, `~/.local/bin`, and the running executable's directory when it is not a release, become links to the absolute `<data>/current/bin/<name>`, each made as `<name>.new` and renamed over the file. A launcher already linked there is left; a link anywhere else is reported and left, and so is a proven launcher whose directory (or the file itself) this user cannot write, such as one in `/usr/local/bin`: link it by hand. A real file is replaced only when it is proven to be that Privateer binary: it is the running image itself (same device and inode), or it is a native executable (ELF or Mach-O, so a script or wrapper is never run or replaced) that, run with a throwaway HOME, XDG directories, fleet registry and ledger and no service manager, answers as itself: `privateer upgrade status` printing release status whose `running_executable` is that very file, the dashboard's `--help` banner, or the broker's `--version`. Anything else is reported with the reason and left; a binary too old to have `upgrade status` is among them (replace it by hand, or `cargo install` a newer one first). `CARGO_HOME` is not followed. The MCP broker's installed copy under `<data>/mcp-broker/` is never a launcher: its path and hash stay as they are ([the MCP broker](mcp-broker.md)).
- **Units.** Every dispatcher and door unit in the systemd user unit directory, and every dispatcher and door agent in `~/Library/LaunchAgents`, that Privateer generated (recognised by its `privateer-` name and its description, or an agent by its exact `--home HOME serve` or `door run NAME` arguments) and that does not run `<data>/current/bin/privateer` has its program rewritten to it, its arguments and environment kept; a dispatcher's also gets the safe stop policy `service stop` applies (`KillMode=process`; an agent abandoning its process group). Only a unit whose program is a `privateer` launcher this adoption moves or already moved (the same path, or the same device and inode) or lies inside this installation is rewritten; one running another `privateer` (a development build, a canary, another installation's) or something else is reported and left with the reason, and so is an `ExecStart` whose program is neither quoted nor an absolute path (a `-`, `@`, `+` or `!` prefix, single quotes, escapes or specifiers). Each is written beside the file and renamed over it. On Linux this is followed by one `systemctl --user daemon-reload`. On macOS launchd has no reload that does not restart, so `launchctl` is not called: a changed agent is read at its next bootstrap, and until then launchd restarts it through the old path, which is now a launcher into `current`. Nothing is started, stopped or restarted, but that does not mean nothing changes at once. A running dispatcher starts each new wake and job through the path it was started from (`executable::installed()`): on Linux that is the replaced launcher's path (the kernel reports the old file as deleted, and the suffix is dropped), on macOS the path it was invoked by, and either now leads to `current`. So new wakes and jobs run the adopted release immediately, while the dispatcher and doors themselves keep the old image until their next restart. On Linux a hosted pane whose image was replaced counts as drifted and relaunches at its next idle point; on macOS panes report no drift from adoption.

The report is one JSON object listing each action (`current`, `launcher`, `unit`, `reload`) with its `state`: `done`, `would` (under `--dry-run`, which changes nothing and lists the same plan; it does run the sandboxed identity checks), `already`, or `left` with a `reason`. A failure stops at that action, marked `failed`, with the rest `not_done`; the command prints the report and exits non-zero, each file is either the old one or the new one, and nothing is recorded in the history. A rerun picks up what is left. When it finds the work done but no adoption in the history (the earlier `daemon-reload` or history write failed), it plans one `daemon-reload` for the systemd units already pointing at `current` and records the adoption. A second run after a recorded adoption is a no-op that says so (`noop: true`). Adoptions of one installation are serialised by `<data>/upgrade.lock`. An adoption that changed anything is recorded in the upgrade history with `kind: "adopt"`.

Once `current` is set, `upgrade status` lists in `launcher_unversioned` each native executable that is a real file at a launcher path again (a later `cargo install`); run `upgrade adopt` again to link it.

## Bundled ship manifests

A first-mate profile stores `"manifest": "bundled:pi"` (or another bundled harness id) rather than a checkout/release path. Profile validation, activation, native-session decoding, plugin command planning, attended harness selection, and child-ship inheritance understand these references. Resolution uses the *running runtime's* plugins. Private configs, model/provider options, and charter provider-plugin pins are not rewritten.

Opening a ship home transactionally and idempotently migrates recognizable old `first_mate.manifest` paths:

- A standard `plugins/<known-harness>/manifest.json` in the build checkout or a recognizable checkout (`.git`, `Cargo.toml`, and `plugins/native.py`), when its JSON matches the shipped bundled manifest.
- A matching bundled manifest in the versioned release layout, including an absent manifest under a deleted release.

Custom paths stay absolute and unchanged, even when their manifest declares a familiar id or has the same JSON as a bundled adapter. Modified manifests are not silently converted. An already-deleted **unidentifiable checkout** cannot safely be distinguished from a missing custom plugin: its path is preserved, not guessed. Such records need an explicit operator profile update to `bundled:<id>`. A new ship using the ordinary bundled manifests records a durable reference before its checkout can disappear. This migration changes no provider route or private config.

## Status and history

```sh
privateer upgrade status
privateer upgrade.status --json '{}'
```

Both forms emit JSON, work outside a ship folder, and may be read by agents. `upgrade.status` is also available over RPC. Status is read-only: it does not create a fleet registry, run Git, change `current`, or start/stop anything.

Fields include:

- `current_commit`: release selected by `current`, or null if no release is selected.
- `running_commit`: this process's release, or null for an unversioned executable. It can differ from `current_commit`.
- `current`, `running_executable`: paths that make that distinction inspectable.
- `launcher_unversioned`: once `current` is set, the native executables that are real files at launcher paths (see adopting, above).
- `history`: newest 100 upgrade outcomes from the fleet registry (`history_limit: 100`). Existing registries without an upgrades table report an empty history.
- `keep_releases`: the future retention default, 3; not a claim that pruning ran.
- `hosted_panes`: each live hosted first mate in this home (listed for the operator when a ship home is at hand). Each reports the release its process runs (`commit`, null for an unversioned build), its `executable`, the `current_commit`, and `drift`: true when the pane runs a different release than `current`, or its binary was replaced on disk. Drift is reported, not repaired, here: hosted panes relaunch at their next idle point or through `privateer host relaunch SHIP`, as the [architecture notes](architecture.md#hosted-first-mates) describe.

A dangling, invalid, or foreign `current` target is reported as an error, not repaired. The fleet registry gains an additive `upgrades` table, independent of per-ship home schemas. `UpgradeRecord` holds an id, exact from/to commits, actor, start/completion timestamps, and each ship's outcome. The persistence API is idempotent for identical records and refuses a conflicting record with the same id. There is deliberately no command to fabricate history: the later guarded orchestrator will record its results here. Status does not imply that the live fleet has restarted or that an upgrade has been approved.

The isolated release tests use copied test binaries, disposable registries/homes, and a real dispatcher and adapter subprocess. They switch only fixture symlinks, never the installed Privateer binary or a live fleet.
