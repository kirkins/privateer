# Desk schema shape audit

Audit baseline: `6965d6da`. Scope: production `CREATE TABLE IF NOT EXISTS`
and `CREATE INDEX` statements recursively under `src/machines/desk*`,
`src/door/subscription*`, and `src/mail/source*`. Test-only fixture DDL is
excluded. No installed registry was opened.

## Method and release boundary

Inspected `git log -p -S <table>` for every table below, the DDL/ALTER patches,
and normalized CREATE definitions at **every first-parent main snapshot** that
touched these paths, from their introduction through the baseline. This
separates development commits from schemas actually merged to main. Commit
IDs below identify the first main snapshot containing the table, not necessarily
the implementation commit. “Stable” means its CREATE column/key definition
never changed after that first main snapshot. Additive ALTER migrations are
called out separately; an unchanged CREATE statement alone does not prove
shape compatibility.

There are **69 current tables and 18 explicit indexes**. Only
`desk_replay_cut_return` has multiple CREATE shapes on main; its existing
`failures` migration is guarded. Several older development shapes nevertheless
exist in installed registries. This patch repairs the three retry/cleanup
shapes identified by the initial review and R1, even though they were never
main's initial CREATE shape.

## Tables

| Table | First main | Verdict / migration |
|---|---|---|
| `desk_baseline_cursor` | `23dde035` | Stable. |
| `desk_baseline_home` | `23dde035` | Stable. |
| `desk_exposure_cut` | `3b15a51a` | Stable on main, but known development shape lacks `journal_upper`; repair belongs to separate #626, not this patch. |
| `desk_exposure_upgrade` | `3b15a51a` | Stable. |
| `desk_home_copy` | `0753e7ca` | Stable. |
| `desk_home_inventory` | `0753e7ca` | Stable. |
| `desk_home_inventory_gap` | `0753e7ca` | Stable. |
| `desk_home_receipt` | `0753e7ca` | Stable. |
| `desk_intake_audit` | `926ac334` | Stable. |
| `desk_intake_checkpoint` | `8cd7e290` | Stable. |
| `desk_intake_contact` | `8cd7e290` | Stable. |
| `desk_intake_fact` | `8cd7e290` | Stable. |
| `desk_intake_gap` | `8cd7e290` | Stable; migration to separate loss evidence preserves nonrecoverable diagnostics. |
| `desk_intake_loss` | `1ff5496c` | Stable. |
| `desk_intake_order` | `926ac334` | Stable. |
| `desk_intake_original` | `84f3ed95` | CREATE stable; `ae499b6e` adds nullable `source`, `40471cc9` adds nullable `first_kind`. Both guarded by column checks, including readiness before/after writer acquisition. No inferred historical kind. |
| `desk_intake_proof` | `1ff5496c` | Stable. |
| `desk_intake_transport` | `926ac334` | Stable; `last_receipt` was present at introduction (`87d3b91b`). |
| `desk_placement_audit` | `06e0acf3` | Stable. |
| `desk_placement_name` | `06e0acf3` | Stable. |
| `desk_placement_peer` | `06e0acf3` | Stable. |
| `desk_placement_record` | `06e0acf3` | Stable. |
| `desk_placement_topology` | `06e0acf3` | Stable. |
| `desk_replay_adopted` | `0be50216` | Stable. |
| `desk_replay_batch` | `0be50216` | Stable on main. Development `d9e16689` lacked `binding`; `a486df17` adds it with guarded ALTER and invalidates whole-batch receipts. Member/cache tables also force old installs into initialization. |
| `desk_replay_budget` | `2a182097` | Stable; `replay_v5` marker forces old counter/trigger repair once. |
| `desk_replay_cut_return` | `06733f91` | Changed on main: `45a3a36f` adds `failures`, first main `88796021`. Existing readiness checks the column, guarded ALTER defaults to 0 before creating failures index. |
| `desk_replay_cut_send` | `06733f91` | Stable. |
| `desk_replay_home` | `2a182097` | Stable on main. Development `44e42b20` lacked `baseline`; `40471cc9` supplies guarded nullable ALTER, with replay version readiness. |
| `desk_replay_home_cache` | `0be50216` | Stable. |
| `desk_replay_import` | `2a182097` | Stable; `rebuild_cut` existed at introduction (`ae499b6e`). |
| `desk_replay_link_retry` | `3b15a51a` | Stable on main, known development `(binding,source,cursor)` shape. **New guard** checks the entire column set and primary key in readiness; drops/recreates only retry cursors inside the existing IMMEDIATE transaction, then rechecks readiness. |
| `desk_replay_member` | `0be50216` | Stable. |
| `desk_replay_omission` | `2a182097` | Stable on main. Development `ae499b6e` used source-only PK and `enclosing=1`; `f8aad868` already detects old PK, rebuilds preserving lasting gaps, and repairs counters. |
| `desk_replay_receipt` | `0be50216` | Stable on main. Development `d9e16689` lacked `body`; `a486df17` supplies guarded nullable ALTER and clears non-subset receipts. |
| `desk_replay_release` | `0be50216` | Stable. |
| `desk_replay_revision` | `17e0d7cf` | Stable. |
| `desk_replay_send` | `0be50216` | Stable. |
| `desk_replay_vector` | `0be50216` | Stable. |
| `desk_replay_witness` | `2a182097` | Stable. |
| `desk_source_barrier` | `8cd7e290` | Stable. |
| `desk_source_cleanup` | `8efb4d14` | Stable on main, but development `a9f0d6da` lacks `upper_seq` (`4597921c` added it in place). **New guard** includes column in readiness and adds `INTEGER NOT NULL DEFAULT 0` under writer lock, preserving scan/prune cursors. |
| `desk_source_event` | `8cd7e290` | Stable. |
| `desk_source_head` | `8efb4d14` | Stable. |
| `desk_source_issuance` | `926ac334` | Stable. |
| `desk_source_proof` | `1ff5496c` | Stable. |
| `desk_source_proof_work` | `1ff5496c` | Stable. |
| `desk_source_reconciliation` | `32551ea0` | Stable. |
| `desk_source_revision` | `1ff5496c` | Stable. |
| `desk_subscription` | `07b0290b` | Stable. |
| `desk_subscription_exception` | `17e0d7cf` | Stable on main; incarnation added in `ddac869c` before first merge. |
| `desk_subscription_exclusion` | `17e0d7cf` | Stable on main; incarnation/key change in `ddac869c` preceded first merge. |
| `desk_subscription_history_receipt` | `17e0d7cf` | Stable. |
| `desk_subscription_history_ref` | `17e0d7cf` | Stable. |
| `desk_subscription_pin` | `17e0d7cf` | Stable on main; incarnation added in `ddac869c` before first merge. |
| `desk_subscription_progress` | `07b0290b` | Stable. |
| `desk_subscription_receipt` | `07b0290b` | Stable on main. Development v5 `kind` ALTER in `4d4e9345` was removed in `ddac869c` before first v5 main merge; automatic tables remained v4-compatible. |
| `desk_subscription_ref` | `07b0290b` | Stable on main; development v5 `kind` rebuild reverted before first v5 main merge (`ddac869c`). |
| `desk_subscription_replay` | `84f3ed95` | Stable. |
| `desk_subscription_selection` | `17e0d7cf` | Stable on main; incarnation/key change in `ddac869c` preceded first merge. |
| `desk_subscription_snapshot` | `07b0290b` | Stable. |
| `mail_source_cleanup` | `8efb4d14` | Stable on main. Development `a9f0d6da` lacked `upper_seq`; `4597921c` added it before first merge. **R1 guard** includes the column in intake readiness and adds `INTEGER NOT NULL DEFAULT 0` inside the optional intake IMMEDIATE transaction, preserving `scan_seq`, then rechecks readiness. |
| `mail_source_enrollment` | `23dde035` | CREATE stable. Existing guarded ALTERs add `nonce`, `prior_lost`, `prior_loss` (`1e4ba47f`, `c4d66732`); all participate in enrollment readiness. |
| `mail_source_health` | `0753e7ca` | Stable on main; `core` added in `1b07989a` before first merge of `2edd31a5`. |
| `mail_source_import` | `0753e7ca` | Stable. |
| `mail_source_origin` | `0753e7ca` | Stable. |
| `mail_source_outbox` | `0753e7ca` | Stable. |
| `mail_source_snapshot` | `0753e7ca` | CREATE stable. Existing guarded nullable `nonce` ALTER (`1e4ba47f`) participates in enrollment readiness. |
| `mail_source_upgrade_failure` | `23dde035` | Stable. |

Historical `desk_replay_anchor` was introduced in development `ae499b6e`,
replaced by `desk_replay_home` in `44e42b20`, before first main merge
`2a182097`. Existing storage migration folds its monotonic mail orders into
home marks before dropping it. Temporary migration tables are not independent
CREATE IF NOT EXISTS schemas.

## Explicit indexes

All 18 index definitions are unchanged since their first main snapshot.
They refer only to columns present at that snapshot (with the noted known
old-shape exception). No unique index is installed separately in this scope;
PRIMARY KEY / UNIQUE table constraints are included in the table audit.

| Index | Table | First main / verdict |
|---|---|---|
| `desk_source_origin_seq` | `desk_source_event` | `8efb4d14`, stable expression index. |
| `desk_replay_binding` | `desk_replay_import` | `2a182097`, stable. |
| `desk_replay_member_intent` | `desk_replay_member` | `0be50216`, stable. |
| `desk_replay_adopted_retry` | `desk_replay_adopted` | `3b15a51a`, stable. |
| `desk_replay_cut_work` | `desk_replay_cut_return` | `06733f91`, stable. |
| `desk_replay_rebuild` | `desk_replay_import` | `06733f91`, stable. |
| `desk_replay_cut_failures` | `desk_replay_cut_return` | `88796021`, stable; guarded ALTER runs first. |
| `desk_exposure_cut_order` | `desk_exposure_cut` | `3b15a51a`, stable; old-shape repair tracked in #626. |
| `desk_exposure_cut_mailbox` | `desk_exposure_cut` | `3b15a51a`, stable; old-shape repair tracked in #626. |
| `desk_subscription_snapshot_reader` | `desk_subscription_snapshot` | `07b0290b`, stable. |
| `desk_subscription_ref_reader` | `desk_subscription_ref` | `07b0290b`, stable. |
| `desk_subscription_history_ref_reader` | `desk_subscription_history_ref` | `17e0d7cf`, stable. |
| `desk_subscription_exclusion_work` | `desk_subscription_exclusion` | `17e0d7cf`, stable (development incarnation change preceded main). |
| `mail_source_import_origin_seq` | `mail_source_import` | `8efb4d14`, stable partial expression index. |
| `mail_source_import_local_seq` | `mail_source_import` | `8efb4d14`, stable partial expression index. |
| `mail_source_origin_unresolved` | `mail_source_origin` | `1ff5496c`, stable partial index. |
| `mail_source_import_pending` | `mail_source_import` | `8efb4d14`, stable partial index. |
| `mail_source_import_receipted` | `mail_source_import` | `8efb4d14`, stable partial index. |

## Safety and regression evidence

`desk_source_cleanup.upper_seq` freezes a cleanup sweep's upper sequence so
advancing traffic cannot starve retries. Zero already means “no frozen sweep”:
`retention::acknowledged_batch` chooses the exact acknowledged page's `through`
as the next sweep's upper. It does not acknowledge anything, advance pruning,
or make a retained row disposable. Per-row receipt pins, live-state checks and
supersession checks remain authoritative. Defaulting legacy rows to zero is
therefore conservative; existing `scan_seq` and `pruned_through` are preserved.

`mail_source_cleanup.upper_seq` uses the same zero sentinel. In
`home::reclaim_import_batch`, zero chooses the caller's `cleanup_upper`; a
nonzero saved bound is capped at that value. The migration does not change
`scan_seq`, receipts, source identity filtering, live-state/supersession checks,
or registry-proof-first discharge. It therefore starts a bounded sweep, not
unbounded pruning or a fabricated acknowledgment. The seeded home test compares
all cleanup-column metadata with a fresh install, checks cursor preservation,
exercises the current writer, and reopens with a competing writer to prove the
second intake initialization performs no DDL/DML or writer acquisition. It runs
with both absent intake indexes and otherwise-current indexes. A separate home
cleanup regression proves migrated zero retains live facts and disposable rows
beyond the caller's bound.

`schema_tests.rs` seeds each requested old registry shape alone and together, both in a
partial install and in an otherwise current registry. It compares columns,
types, nullability, defaults and keys for **all 61 desk tables** against a fresh
install (column order deliberately ignored for ALTER compatibility). It reopens
the database and proves the second desk initialization performs no DDL/DML and
needs no writer lock. Additional tests exercise full-set/key rejection,
transaction rollback after a downstream injected failure, simultaneous openers
synchronized before BEGIN, and cleanup respecting the acknowledged bound and
live rows after migration.

## R1 positional INSERT audit

Searched the scoped source trees for `INSERT [OR ...] INTO desk_* / mail_source_* VALUES(...)`
and compared the results with the column/key guards above. Named the columns in:

- `desk_replay_link_retry`: production retry write in `wire/cuts.rs`, and the
  historical fixture in `schema_tests.rs`.
- `desk_source_cleanup`: both historical fixtures in `schema_tests.rs`.
- `desk_replay_omission`: production write in `storage.rs` and historical fixture
  in replay `review_tests.rs`. Also named both sides of its migration's
  `INSERT ... SELECT` so rebuilding does not depend on historical column order.
- `desk_exposure_cut`: production write in `wire/cuts.rs`, proactively compatible
  with the separate #626 guard. Its positional historical fixture in
  `src/door/subscription/replay_tests.rs` is intentionally untouched because that
  file belongs to #626.

Other positional matches target tables without column/key shape guards
(existence checks alone are not shape guards); no changes to those in R1.
The retry shape check deliberately ignores declared types and NOT NULL, and
accepts column reordering; named writes match that contract.

Limits: this is not a universal repair for arbitrary/corrupted SQLite schemas.
It does not migrate every never-released development variant. In particular,
known exposure-cut repair is intentionally left to #626; no live-installation
verification or production deployment is performed here.
