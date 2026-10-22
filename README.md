<h1 align="center">Privateer</h1>

<h3 align="center">Independent ships. Working under your charter.</h3>

<p align="center">
  <img alt="Privateer fleet: the captain, liaison, peer ships, and their first mates and crews" src="assets/banner.png" width="100%" />
</p>

Privateer is a Rust runtime for a fleet of AI-agent ships. Each ship has a **first mate**, the agent you talk to for that ship's work. First mates coordinate with each other by mail, not by command, and run their own **crew** as supervised jobs. Every task, outcome and open request has one recorded owner. The **charter** says what the fleet may do, and production needs your explicit approval for the exact artifact and environment.

## Install and upgrade

Rust 1.85 or newer and Cargo are required. Unix is supported.

```sh
cargo install --locked --path . --features dashboard  # privateer and privateer-dashboard
```

Versioned release directories (`releases/<git commit>/bin` with a `current` link) are described in [releases](docs/releases.md), and stable launchers should point at `current/bin/privateer`. `cargo install` remains supported. `privateer upgrade status` is read-only, and `privateer upgrade fetch <commit>` prepares a checked release from CI or a source build without switching to it; the runtime does not execute fleet upgrades yet.

## Getting started

```sh
cd ~/ships/liaison
privateer init --ship liaison --liaison   # make this folder a ship and write its AGENTS.md
privateer start                           # talk to this ship's first mate
privateer status
```

`privateer service status|start|stop` manages the background dispatcher, which answers mail while you are away. `privateer ship new NAME` adds a ship, and `privateer ask SHIP "..."` sends one a message. See [getting aboard](docs/architecture.md#getting-aboard) for the first-run flow.

Bare `privateer` opens the dashboard when it is installed. Without a dashboard it falls back to the ship-folder flow above.

## Commands

Captain commands are typed in a terminal, such as `init`, `start`, `status`, `ask`, `tell`, `inbox`, `ship`, `service`, `permissions`, `routes`, `door` and `machine`. Run `privateer --help` for the full list.

Runtime commands take JSON, either inline or from a file:

```sh
privateer --home /absolute/private/home status --json '{}'
privateer --home /absolute/private/home charter.put --file examples/charter.yaml
```

`--file` takes one YAML or JSON object of up to 64 KiB, read as data only. `privateer --home PATH rpc` accepts one JSON request per line. First mates run with `PRIVATEER_ROLE=agent`, which refuses operator commands and binds their actor name to their own ship.

## Concepts

- **Charter**: what the fleet may do. Edit the [example charter](examples/charter.yaml) first. See [architecture](docs/architecture.md#runtime-boundary).
- **Ships and mail**: each ship has a folder, a mailbox and a first mate. Messages are durable and targeted. See [ships and mailboxes](docs/architecture.md#ships-and-mailboxes).
- **Tasks and custody**: a task has one owner until a handoff is accepted. See [architecture](docs/architecture.md#runtime-boundary).
- **Waking**: the dispatcher wakes first mates for due mail and work. See [waking first mates](docs/architecture.md#waking-first-mates).
- **Crew and outcomes**: crew are supervised jobs that report results back to their first mate. See [crew outcomes](docs/architecture.md#crew-outcomes).
- **Errands**: short headless reviews or investigations that mail back only a verdict. See [errands](docs/architecture.md#errands).
- **Scheduling**: prepared tasks can run without a captain nudge. See [work scheduling](docs/scheduling.md).
- **Workspaces**: Git jobs use separate checkouts through [Treehouse](docs/workspaces.md). Non-Git jobs do not need it.
- **Resources**: jobs share one host-wide ledger of CPU, memory and disk. See [resources](docs/resources.md).
- **Permissions and routes**: launch permissions and the order in which crew routes are tried. See [tool permissions](docs/tool-permissions.md).
- **Harnesses and plugins**: agent harnesses and provider routes are executable plugins. Private gateways and credentials stay in private configuration. See [plugins](plugins/README.md).
- **Machines and front doors**: ships on linked machines exchange directories over SSH, and front doors such as [Telegram](plugins/telegram/README.md) carry the captain's mail. See [peer transport](docs/architecture.md#peer-transport) and [front doors](docs/architecture.md#front-doors).

## Dashboard

`privateer-dashboard` is a terminal command deck for the whole fleet: stat tiles, a machine → ship tree with what each ship is doing and an hourly activity strip, token use per model, **Needs you** and a live event feed. It reads the real homes of your registered fleet and the runtime's fleet commands, and never creates or repairs them. It has no sample or demo data.

```sh
privateer-dashboard                      # interactive; needs a terminal
privateer-dashboard --once               # one bounded read-only JSON observation
privateer-dashboard --home /absolute/home --no-herdr
```

Enter focuses a ship's existing, validated Herdr pane. It never launches or creates panes. Needs you lists typed runtime needs, and an answer is sent as a reply to the original request. `i` opens mail history, `v` opens the voyage view, and `q` quits. Unknown or unavailable evidence is never shown as an all-clear. Full details are in [dashboard setup and boundaries](docs/dashboard.md).

## Design notes

Current architecture is in [docs/architecture.md](docs/architecture.md). Agreed directions and proposals live in [docs/design/](docs/design/), starting with [conversation and activation](docs/design/conversation-and-activation.md). The [milestone 1 notes](docs/milestone-1.md) record the first milestone's status and validation.

## Build, test and pilot

```sh
cargo build
cargo test
python3 scripts/pilot.py --binary target/debug/privateer
```

Python 3 is used by optional native adapters and test fixtures, and Git by the pilot. Node 24 is needed for the Pi/OMP extension tests. The pilot runs in temporary homes, with no live inference, remote publishing or fleet update. See [pilot scope](docs/pilot.md) before comparing its evidence with a production deployment.

## Safety boundaries

SQLite transactions preserve local state and custody. Job supervisors own their process groups, bound their logs and deadlines, and draw on the host-wide capacity ledger. Charters govern the workflow under the operating-system account that runs Privateer. They are not an adversarial credential sandbox. See [operators and agents](docs/architecture.md#operators-and-agents) for the identity checks and their limits.

Privateer is inspired by First Mate's orchestration and Corsair's experiments, but it is a new codebase rather than an inherited shell runtime.
