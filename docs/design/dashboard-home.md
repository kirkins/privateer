# Handoff: the dashboard as home, and onboarding

Status: **steps 1–6 implemented; step 7 pending**. See [dashboard setup and boundaries](../dashboard.md) for current behavior. Read [conversation and activation](conversation-and-activation.md) for the overall design and [architecture](../architecture.md) for what exists.

## Goal

A person should be able to install Privateer, type `privateer`, and land somewhere they can see every ship, read and answer their mail, and jump into any ship's first mate, without visiting folders one by one or writing commands. Inside Herdr that place is the dashboard (`privateer-dashboard`); a tmux variant can follow for people without Herdr. A first-time user is walked through creating their first ship, suggested as a liaison, which then helps them create the rest.

## What exists today

- **Ships are folders.** State lives in `<folder>/.privateer`, the folder is the charter's repository root, and projects go in `projects/`. Setup generates harness instructions (`AGENTS.md`, `CLAUDE.md`) and Claude permissions and hooks. See [Getting aboard](../architecture.md#getting-aboard) for initialization, dashboard entry, refresh policy and dispatcher behavior.
- **The fleet.** A per-user registry lists every ship on the machine: `fleet.list` returns `{registry, ships:[{name, home, created_by, created_ms, retired_ms}]}`. Ships are linked both ways automatically, so they can mail each other. `ship.create {name, scope?, objective?, folder?, harness?}` makes a sibling ship from inside a ship (`privateer ship new NAME`); `ship.retire {name}` retires one, keeping its folder. An agent's new ship gets at most its own charter's actions.
- **Talking to a ship.** `privateer start [HARNESS]` launches the ship's harness in its folder as its first mate, holding the ship's presence lease (`ship.presence {ship}` shows it) so the dispatcher leaves mail to the live session. Claude Code sessions also get mail at each prompt and turn end through hooks.
- **The captain's mail.** `captain` is a mailbox in each home. Ships send it requests, notices and decisions; `privateer inbox` reads and acknowledges it, `privateer reply ID TEXT` answers, `privateer ask SHIP TEXT` / `tell` send. JSON equivalents: `mail.inbox {"address":"captain"}`, `mail.ack`, `mail.send {"from":"captain",...}`, `mail.show`.
- **Per-ship state.** `ship.bearings {ship}` (open tasks, requests owed and owing, unacknowledged mail, ship knowledge note, plus a ready `text` rendering), `wake.list {ship}` (recent wakes: mode, status, usage), `note.list/get`, `focus.digest`, `status`.
- **The dashboard** (`src/dashboard/`): see [dashboard setup and boundaries](../dashboard.md) for its current capabilities.

## What to build

### 1. Fleet home

Read all ships from `fleet.list` (run `privateer fleet.list --json '{}'`, or read the registry file read-only) and show each active ship with what its own home reports: scope, harness, presence (aboard or not), dispatcher running, open requests, unacknowledged mail and the latest wakes. Keep reading homes read-only, as the dashboard does now; open each home's database only after checking its folder still exists (opening a Privateer home creates it).

### 2. Live captain inbox

Replace the placeholder with the real mailbox, aggregated across fleet homes: for each home, `mail.inbox {"address":"captain"}`. Show decisions first, then requests, then notices. Answering uses `mail.send {"from":"captain","to":<sender>,"kind":"reply","correlation":<id>,"body":...}` in the home that holds the message (or `privateer reply` run in that ship's folder); reading acknowledges with `mail.ack` in that home. The composer's Ask/Message modes send `request`/`notice` mail from `captain` to the chosen ship.

### 3. Boarding a ship

Implemented; see [Boarding and management](../dashboard.md#boarding-and-management)
for the launch protocol, automatic bindings and navigation safety boundaries.

### 4. Ship management

Implemented; the forms and confirmation rules are documented under
[Boarding and management](../dashboard.md#boarding-and-management).

### 5. Onboarding

Implemented; see [First run and everyday entry](../dashboard.md#first-run-and-everyday-entry)
for the form and creation flow, and [Getting aboard](../architecture.md#getting-aboard)
for the liaison preset and generated instructions.

### 6. Opening into the dashboard

Implemented; see [Getting aboard](../architecture.md#getting-aboard) for entry
behavior and [Build and preview](../dashboard.md#build-and-preview) for installation.

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
- First run with an empty fleet walks through creating a liaison, following the documented [creation and Board conditions](../dashboard.md#first-run-and-everyday-entry).
- Inside Herdr with the dashboard installed, bare `privateer` opens the dashboard; elsewhere it starts the harness as today.
- Tests drive real binaries against temporary homes with a private `PRIVATEER_FLEET` and `PRIVATEER_SERVICE=none`; Herdr interactions are covered by the existing smoke-test approach.
