# Handoff: the dashboard as home, and onboarding

Status: **handoff for implementation**, written 14 October 2026. Read [conversation and activation](conversation-and-activation.md) for the overall design and [architecture](../architecture.md) for what exists. This document is self-contained enough to start from; where it names a command, that command exists today unless marked *new*.

## Goal

A person should be able to install Privateer, type `privateer`, and land somewhere they can see every ship, read and answer their mail, and jump into any ship's first mate, without visiting folders one by one or writing commands. Inside Herdr that place is the dashboard (`privateer-dashboard`); a tmux variant can follow for people without Herdr. A first-time user is walked through creating their first ship, suggested as a liaison, which then helps them create the rest.

## What exists today

- **Ships are folders.** `privateer init` (or bare `privateer`, which asks first) makes the current folder a ship: state in `<folder>/.privateer`, the folder is the charter's repository root, projects go in `projects/`. It writes `AGENTS.md`, `CLAUDE.md`, Claude permissions and hooks, starts the ship's dispatcher (`privateer service status|start|stop`; a systemd user service where available, otherwise detached), and registers the ship in the fleet.
- **The fleet.** A per-user registry lists every ship on the machine: `fleet.list` returns `{registry, ships:[{name, home, created_by, created_ms, retired_ms}]}`. Ships are linked both ways automatically, so they can mail each other. `ship.create {name, scope?, objective?, folder?, harness?}` makes a sibling ship from inside a ship (`privateer ship new NAME`); `ship.retire {name}` retires one, keeping its folder. An agent's new ship gets at most its own charter's actions.
- **Talking to a ship.** `privateer start [HARNESS]` launches the ship's harness in its folder as its first mate, holding the ship's presence lease (`ship.presence {ship}` shows it) so the dispatcher leaves mail to the live session. Claude Code sessions also get mail at each prompt and turn end through hooks.
- **The captain's mail.** `captain` is a mailbox in each home. Ships send it requests, notices and decisions; `privateer inbox` reads and acknowledges it, `privateer reply ID TEXT` answers, `privateer ask SHIP TEXT` / `tell` send. JSON equivalents: `mail.inbox {"address":"captain"}`, `mail.ack`, `mail.send {"from":"captain",...}`, `mail.show`. Captain mail does not cross homes yet: each home holds the captain mail its ships sent there.
- **Per-ship state.** `ship.bearings {ship}` (open tasks, requests owed and owing, unacknowledged mail, ship knowledge note, plus a ready `text` rendering), `wake.list {ship}` (recent wakes: mode, status, usage), `note.list/get`, `focus.digest`, `status`.
- **The dashboard** (`src/dashboard/`, `docs/dashboard.md`) reads **one** home read-only, shows ships as owner identities in that home, and can *focus* Herdr panes bound by hand in `<home>/dashboard.json`. Its live captain inbox is still a placeholder ("unavailable until the runtime's front door is connected"), and it cannot launch agents.

## What to build

### 1. Fleet home

Read all ships from `fleet.list` (run `privateer fleet.list --json '{}'`, or read the registry file read-only) and show each active ship with what its own home reports: scope, harness, presence (aboard or not), dispatcher running, open requests, unacknowledged mail and the latest wakes. Keep reading homes read-only, as the dashboard does now; open each home's database only after checking its folder still exists (opening a Privateer home creates it).

### 2. Live captain inbox

Replace the placeholder with the real mailbox, aggregated across fleet homes: for each home, `mail.inbox {"address":"captain"}`. Show decisions first, then requests, then notices. Answering uses `mail.send {"from":"captain","to":<sender>,"kind":"reply","correlation":<id>,"body":...}` in the home that holds the message (or `privateer reply` run in that ship's folder); reading acknowledges with `mail.ack` in that home. The composer's Ask/Message modes send `request`/`notice` mail from `captain` to the chosen ship.

### 3. Boarding a ship

Give each ship a **Board** action:

- If a live pane is already bound to the ship, focus it (existing `agent.focus` path).
- Otherwise open one: create a Herdr tab in the ship's folder (`herdr tab create --cwd <folder> --label <ship>`, or the socket equivalent) and run `privateer start` in it. Check the installed Herdr API (`herdr api`, `herdr --skill`) for the exact way to launch a command or agent in the new pane; `herdr agent start` starts a supported agent in an existing pane.
- Bind the new pane to the ship automatically instead of requiring hand-edited pane ids, and show presence from `ship.presence`.

### 4. Ship management

From the fleet home: **New ship** (a small form calling `ship.create`), **Retire** (with confirmation, calling `ship.retire`; say that the folder is kept), and **Dispatcher** status/start/stop via `privateer service`. Mutations go through Privateer commands, never direct database writes.

### 5. Onboarding

On first run with an empty fleet:

1. Welcome, then suggest creating a first ship, named `liaison` by default, in `~/ships/liaison`; let the person rename it, pick the folder, and choose a harness from those installed.
2. Run `privateer init --ship <name> --harness <id>` in that folder (create it first), then Board it.
3. The liaison's first session onboards the captain: it asks what they work on, proposes ships with scopes and charters, creates them with `ship.create`, adds projects (`privateer project add`), and explains how to reach any ship. Nothing forces a liaison: a person can make any ship first, or skip onboarding.

**Small runtime piece (new):** a liaison preset, for example `privateer init --liaison`, that writes a liaison charter (read, message and delegate by default, without implementation, per the design) and an `AGENTS.md` variant with the onboarding checklist above. Coordinate it with whoever owns `src/cli.rs` (the `setup` function and `instructions`), or add it in the same work.

### 6. Opening into the dashboard

Last step, once Board works: bare `privateer` should open the dashboard when running inside Herdr (`HERDR_ENV=1`) and `privateer-dashboard` is installed (on `PATH` or beside the `privateer` binary), and otherwise keep today's behaviour of starting the ship's harness. The change belongs at the end of `board` in `src/cli.rs`, which currently calls `start(&home, &[])` after making sure the dispatcher runs. Keep `privateer start` as the direct path. Installation should cover both binaries: `cargo install --locked --path . --features dashboard`.

### 7. tmux fallback (later)

The same actions without Herdr: a tmux window per ship running `privateer start`, and the fleet home and inbox in the first window.

## Constraints

- Read homes read-only; change state only through Privateer commands, as the operator. Never write `privateer.sqlite` directly.
- Never create a home by opening a missing one; check the folder exists.
- Demo mode stays sample-only.
- Keep the existing Herdr safety rules: no guessed sockets, protocol checks, no focusing or binding by label similarity.

## Acceptance

- With several ships made by `init` and `ship.create`, the fleet home lists all of them with live presence, dispatcher and mail counts.
- A request from a ship to the captain appears in the dashboard inbox; replying closes it (`mail.show` reports `answered`).
- Board on a ship with no pane opens a Herdr tab in its folder running `privateer start`, and the ship shows as aboard; Board again focuses that pane.
- First run with an empty fleet walks through creating a liaison and boards it.
- Inside Herdr with the dashboard installed, bare `privateer` opens the dashboard; elsewhere it starts the harness as today.
- Tests drive real binaries against temporary homes with a private `PRIVATEER_FLEET` and `PRIVATEER_SERVICE=none`; Herdr interactions are covered by the existing smoke-test approach.
