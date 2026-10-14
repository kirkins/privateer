<h1 align="center">Privateer</h1>

<h3 align="center">Independent ships. Working under your charter.</h3>

<p align="center">
  <img alt="Privateer fleet: the captain, liaison, peer ships, and their first mates and crews" src="assets/banner.png" width="100%" />
</p>

Privateer is a fresh Rust runtime for independent peer agents working under a charter. One conversational agent is your familiar point of contact; departments can coordinate directly and keep working while that conversation is busy. Every outcome has a recorded owner.

The first milestone is durable machinery and an isolated working pilot. It does not install over an existing fleet, create production credentials, or run model inference automatically.

See [milestone status and validation](docs/milestone-1.md) for what is implemented, demonstrated, and still needed before fleet replacement. The agreed direction for conversation and activation (the captain, ships and their first mates, mailboxes, channels and standing grants) is in [the conversation and activation design](docs/design/conversation-and-activation.md).

## Terminal dashboard

The optional Rust dashboard opens on a Fleet overview with ships in a persistent
sidebar. Each ship has its own Bridge, Assignments, Terminals and Logbook views:

```sh
cargo run --features dashboard --bin privateer-dashboard -- --demo
```

Use `--home /absolute/privateer/home` to observe an existing home. Inside Herdr,
the dashboard can inspect and focus existing agent terminals using the injected
socket. It does not launch agents, send prompts, acknowledge alerts or migrate a
fleet. Navigator chat and automatic agent activation are not connected yet.
Click a ship, then its workspace tabs; `0` returns to Fleet overview. Keyboard
navigation and mouse-wheel scrolling remain available.
See [dashboard setup and boundaries](docs/dashboard.md).

## General runtime and optional integrations

The core owns charters, tasks, custody, targeted messages, coalesced notifications, durable jobs, resource admission, and SSH transport. It contains no personal provider account, model, gateway address, machine name, or user path.

Harnesses and provider routes are optional executable plugins. A private gateway belongs in a plugin's private configuration. Core jobs receive approved argv through the plugin interface; they do not invent a fallback provider. See [architecture](docs/architecture.md) and [plugin examples](plugins/).

The default charter allows implementation, testing, opening PRs, merging passing work, routine recovery, cleanup, delegation, and messaging. Production requires approval of the exact artifact and environment. The current delivery check records workflow evidence; forge merge and production deployment adapters are future integrations.

## Build and try the isolated pilot

Rust 1.85 or newer and Cargo are required. Unix systems are supported for process-group supervision. Python 3 and Git are used only by the isolated pilot and some process fixtures.

```sh
cargo build
cargo test
python3 scripts/pilot.py --binary target/debug/privateer
```

The pilot creates temporary homes for a navigator and two departments, a small Git repository, and a shared resource ledger. It demonstrates direct peer messages, repeated-alert aggregation, detached work surviving its launcher, resource admission, and durable completion. It performs no live inference, remote publishing, or fleet update. Its evidence and temporary files are retained at the reported path for inspection.

For an independent CLI home:

```sh
privateer --home /absolute/private/home init --json '{}'
privateer --home /absolute/private/home status --json '{}'
```

Register an SSH peer after installing the same binary on that host:

```sh
privateer --home /absolute/private/home peer.put --json '{"name":"infrastructure","host":"department-host","executable":"/absolute/bin/privateer","home":"/absolute/privateer/home"}'
privateer --home /absolute/private/home peer.call --json '{"name":"infrastructure","command":"status","args":{}}'
```

Native SSH handles authentication and host verification. The remote executable and home must be absolute paths; an SSH host is an alias or `user@host`. The caller receives the remote JSON envelope, including its correlation ID and success/error result. Remote state remains remote. The current peer address is a transport configuration, not a proof that another host inherited a charter.

## Safety and reliability boundaries

SQLite transactions preserve local state and custody. Job supervisors own process groups, bound logs and deadlines, and use a host-wide capacity ledger. A busy navigator is not a lock on department work. Notification acknowledgments cover only the observed sequence, so newer facts remain pending.

Charters govern this workflow under the operating-system account running Privateer. They are not an adversarial credential sandbox. Agents launched by Privateer run with an agent role that refuses operator commands (charter changes, approvals, peer and resource configuration) and binds actor names to their own identity; this guards against confused agents, not deliberate evasion. Model reasoning still comes from the chosen harness; this milestone has not established a fully autonomous multi-harness fleet. Read [pilot scope](docs/pilot.md) before comparing its evidence with a production deployment.

Privateer is inspired by First Mate's orchestration and Corsair's experiments, but starts with a new codebase rather than inheriting their shell runtime.
