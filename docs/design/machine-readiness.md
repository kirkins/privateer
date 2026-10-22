# Design: machine readiness

Status: PR 1 (readiness core, dispatcher pass, runtime.needs, owner alert, bearings, and the `service::ensure` summary) is implemented in `src/readiness.rs`; PR 2 (route choice and the flags at routes.set and ship.put) in `src/crew_routes.rs` and `src/mail.rs`; PRs 3-4 are not. Read with [isolated workspaces](../workspaces.md#machine-readiness) and [runtime needs](../runtime-needs.md). Written by the design errand 8f74b4af on main f12c025; line references are to that commit.

Where PR 1 settled what the note left open: `needed_by` is a JSON list of ship names, charter ids for Treehouse, `route:<name>` for routes, and `all` for git; a tool is one row, blocking when anything it serves blocks. The ensure summary is appended only when something fails. A Git charter is one whose repo has a `.git` entry in it or a parent directory.

Task privateer-machine-readiness, from captain notice 80d3c1d5. On the server, storefront's first agent.start failed because Treehouse was missing; nothing said so until launch (docs/workspaces.md:25-29 makes a missing Treehouse a launch failure, by design).

## What exists (origin/main f12c025)
- `capabilities.rs` (PR 329): captain-declared vocabulary, probes (path/file/argv, 5 s, process group, cleared env), 10-min cache, parallel runner. It probes only names the captain declares; it knows nothing of what Privateer itself needs.
- Tool lookups are scattered and use different PATHs: `workspace::binary` (workspace.rs:242, process PATH, `treehouse_binary` in config.json); `cli::on_path`/`detect_harness` (cli.rs:85,196, process PATH, at init only); `moves.rs:851` (herdr, silently drops hosting); capabilities (login PATH, service.rs:325).
- Every bundled manifest runs `python3` (plugins/*/manifest.json), and the harness binary is known only inside native.py PROFILES. Manifest parsing (plugins.rs:18) ignores unknown fields.
- `machines::alert_owner` (machines.rs:763) raises/clears keyed problems from `host_problems` (machines.rs:568) as notices on the owner's duty task, once a minute, but only when the machine was ever linked and has an owner (`owner_here`, machines.rs:742). A single-machine fleet gets nothing.
- `runtime.needs` (needs.rs:116) is operator-only and reads typed rows; agents cannot read it, so a first mate learns of a machine problem only through bearings or mail.
- Every dispatcher start goes through `service::ensure` / `ensure_with_path` (service.rs:308,315), called from init (cli.rs:1753), service start (cli.rs:1931), ship.create (fleet.rs:560) and moves (moves.rs:453,506).

## What to check, and how
One list, built from configuration, never from machine paths:
1. git: `git --version` >= 2.31 (workspace.rs `common()` uses `rev-parse --path-format=absolute`). Blocks everything.
2. Treehouse (`treehouse_binary` or `treehouse`), when any active charter's jobs can land in Git (its repo is in a Git work tree, a project under `<repo>/projects/` is one, or one of its tasks holds a workspace; `workspace::git_backed`): `treehouse --version` >= 3.1.2 (qualified version) and `treehouse get --help` naming `--lease` and `--json`. A missing tool or a version below the floor blocks crews on Git charters. Help flags present but version unparseable: warn "unqualified".
3. Harness binaries: from a new optional manifest field `"requires": ["python3", "claude"]`, binary names only, which we add to the 16 bundled manifests. Read for each manifest named by a first-mate profile or a route in the routes file. A custom manifest without `requires` reports "unchecked", never missing.
4. herdr when any local ship's first mate has `host: herdr`; ssh when the registry has linked machines. Both are presence plus `--version`; both warn only.

The probe runs on the PATH the launch will use: the process PATH in the dispatcher (the unit records it), or, at `service::ensure`, the PATH the unit is about to record (`recorded(service,"PATH")`). When a tool is missing there but `login_path()` finds it, the reason says so: "found at X in your login shell but not on the dispatcher's PATH; run `privateer service start` from that shell". That is the likeliest server failure after a plain missing install.

## When, and what it costs
- Each dispatcher pass, throttled to once a minute like alert_owner. (As built: the resident dispatcher runs the check on a background thread, at most one in flight, and the pass reports the stored rows; `serve --once` only reports. The login-shell PATH lookup is bounded like a probe.) A pass computes a fingerprint: PATH, each needed binary's resolved path plus mtime and size, the set of needed tools (manifests, hosts, links). The cost is a few dozen stat calls.
- Version and protocol probes spawn processes only when the fingerprint changed, or a result is older than its TTL: 5 minutes for a failure (an install clears soon) and 6 hours for a pass. They reuse the capabilities runner (`run_argv`, 5 s each, parallel, 30 s budget).
- Live, uncached, at `service::ensure` (so init, ship.create and service start), at `routes.set` and `ship.put` of a profile (flag a route or harness that is not installed when it is set), and at machine add through a new gateway command `machine.readiness` that returns the remote report.

## Where it shows
- Stored per home in `machine_readiness(tool PRIMARY KEY, level, ok, version, path, why, install, needed_by, fingerprint, checked_ms)`. It is per home because each dispatcher has its own recorded PATH.
- runtime.needs: a new source `readiness`, one item per failing tool, `kind:"blocker"`, `evidence:"machine_tool_missing"`, with summary, `install` and `needed_by`. The dashboard's Needs list reads runtime.needs, so it shows there without new code.
- Owner alert: `host_problems` adds keys `tool:<name>`, so raise, clear and "lasting" come free on linked machines.
- Bearings: one line for a ship whose own work is affected ("Machine: treehouse is missing, so crews on Git projects cannot start. Install: …"). This is the surface first mates can read, and the only alert on an unlinked machine besides runtime.needs.
- The return of `service::ensure` and the init and ship.create results: a `readiness` summary, printed by the CLI.
- agent.start's existing refusal (workspace.rs:272) gains "see runtime.needs readiness", unchanged otherwise.

## Blocks versus warns
- `block`: git, Treehouse (Git charters), python3 when any bundled manifest is in use, the harness of a ship's first-mate profile (that ship cannot wake).
- `warn`: a crew, errand or review route whose harness is missing. In PR 2, `crew_routes::choose` (crew_routes.rs:1125) treats it like backing off ("unavailable: claude not installed"), and it escalates to block only when no route remains for a ship and kind.
  As built: `choose`, `choose_reviewer` and a first mate's fallback read the stored rows (`readiness::missing_for`, a failing row for a tool the manifest `requires`; no row counts as installed) and pass the route over; `choose` adds "passed over <route>: unavailable: <tool> not installed" to the chosen route's `why`. A named route, or an agent's explicit harness, is refused with that reason. The escalation is the refusal, not a row: when no route remains, it lists each route's reason and the install step for every missing tool. A per-ship block row would be derived state (routes file × rows × ship lists) that the pass would have to recompute on every routes.set and check and that could go stale between them; the refusal is computed from the same stored rows at the launch that needs it, and the route rows already show in runtime.needs. `routes.set` and a `ship.put` with a first mate check the named manifests live on the command's PATH (presence, the login-shell lookup bounded by `run_bounded`) and reply with `readiness: {missing, unchecked}`; they store no rows, which stay the dispatcher's, on its own PATH, and never refuse the setting. herdr missing for a hosted ship (it already falls back to headless; now it says so). ssh missing with links.
- Never blocks init or ship.create: it reports. Only launches fail, as today, but with the reason known in advance.

## Generic for a public project
No paths in code. Floors are constants beside the code that needs them, and binaries come from manifests and `treehouse_binary`. Install hints are text: core tools carry a project URL (Treehouse: https://github.com/kunchenguid/treehouse#install; git: "install git 2.31 or later with your package manager"), and manifests may carry `"install": "<hint>"`. No package-manager guessing and no installs: an optional `privateer machine setup` with a pinned per-platform manifest is a later, explicit step, and logins stay manual. The captain's capability vocabulary stays separate: readiness is what Privateer needs, capabilities are what ships need.

## Slices
PR 1 (below): readiness core, pass, runtime.needs, owner alert, bearings, ensure. PR 2: route choice skips uninstalled harnesses; flag at routes.set and profile set. PR 3: machine add over the gateway; herdr and ssh. PR 4: `machine setup` with a pinned toolchain manifest.
