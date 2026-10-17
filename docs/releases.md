# Versioned releases and upgrade status

This is the release-identity foundation for [issue #76](https://github.com/kirkins/privateer/issues/76), not the fleet upgrader. `privateer upgrade status` is available; building, installing, switching, draining/restarting, pruning, rollback, and approval-bound upgrade requests are not implemented by this change. Do not use an ad hoc symlink switch as a safe fleet upgrade.

## Layout

The installation data directory is `$XDG_DATA_HOME/privateer`, defaulting to `~/.local/share/privateer`:

```text
privateer/
  releases/
    <full-lowercase-git-object-id>/
      bin/privateer
      bin/privateer-dashboard
      share/privateer/plugins/
  current -> releases/<full-lowercase-git-object-id>
```

Both SHA-1 and SHA-256 full Git object IDs are accepted; branch names, abbreviations, and paths are not release identities. `releases::Release` defines the layout and the stable `current/bin/{privateer,privateer-dashboard}` entry points. A future installer must publish complete immutable releases (both binaries, built with the dashboard feature, and their bundled plugins), then switch `current` atomically. Stable launchers such as `~/.local/bin/privateer` and service ExecStart paths should address `current/bin/privateer`, not a commit directory. This PR does not install launchers or rewrite existing service units.

The OS-reported **real executable path**, not the launcher symlink or source build path, identifies the running release. A release binary loads plugins **only** from its own `share/privateer/plugins`. It neither follows `current` for adapters nor falls back to the build checkout, ancestor directories, or `PRIVATEER_PLUGINS`. Missing release plugins are an error. Thus moving `current` cannot change the Python code under an old dispatcher.

Agent control argv, generated instructions, and newly persisted peer/service commands go through `current/bin/privateer`. New dashboard launches from a release similarly use `current/bin/privateer-dashboard`, without falling back to an unrelated PATH binary. On Linux, internal job supervisors still reexec the running image through `/proc/self/exe`; their protocol must match their dispatcher. Keep the old release's files while its processes run. The retention default for the future orchestrator is three releases; there is **no pruning operation yet**.

Legacy `cargo install` and source/target builds remain supported: outside a recognized release layout, existing plugin lookup (explicit `PRIVATEER_PLUGINS`, nearby plugins, build checkout) and executable replacement behavior remain unchanged. They are reported as an unversioned running binary, not assigned a guessed Git commit. A source checkout changing after compilation is not evidence of a binary's commit.

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
- `history`: newest 100 upgrade outcomes from the fleet registry (`history_limit: 100`). Existing registries without an upgrades table report an empty history.
- `keep_releases`: the future retention default, 3; not a claim that pruning ran.
- `hosted_panes`: each live hosted first mate in this home (listed for the operator when a ship home is at hand). Each reports the release its process runs (`commit`, null for an unversioned build), its `executable`, the `current_commit`, and `drift`: true when the pane runs a different release than `current`, or its binary was replaced on disk. Drift is reported, not repaired, here: hosted panes relaunch at their next idle point or through `privateer host relaunch SHIP`, as the [architecture notes](architecture.md#hosted-first-mates) describe.

A dangling, invalid, or foreign `current` target is reported as an error, not repaired. The fleet registry gains an additive `upgrades` table, independent of per-ship home schemas. `UpgradeRecord` holds an id, exact from/to commits, actor, start/completion timestamps, and each ship's outcome. The persistence API is idempotent for identical records and refuses a conflicting record with the same id. There is deliberately no command to fabricate history: the later guarded orchestrator will record its results here. Status does not imply that the live fleet has restarted or that an upgrade has been approved.

The isolated release tests use copied test binaries, disposable registries/homes, and a real dispatcher and adapter subprocess. They switch only fixture symlinks, never the installed Privateer binary or a live fleet.
