# Setting a machine's resource floor

Every job on a host is admitted against live readings before it starts. The
floor is the machine's own policy: it says how much of the machine must stay
free for the person and the rest of the fleet, whatever the work wants. This
note is about choosing those numbers for a machine's real workload, and about
what the runtime does with them.

Nothing here needs the captain: `resource.configure` is open to agents, so a
ship can tune the machine it looks after to their person's wishes. It changes
host-wide settings for every home on that machine, and the change is audited.

```bash
privateer --home ~/ships/<ship>/.privateer resource.configure --json \
  '{"min_free_disk_bytes": 4294967296, "heavy_estimate": {"disk_bytes": 6442450944}}'
privateer --home ~/ships/<ship>/.privateer resource.status --json '{}'
```

`resource.status` shows the settings beside the live readings: free and total
space, the disk the home's work uses, the reservations held, the capacity
queue, and `disk_trend` and `disk_forecast` (see below).

## What admission counts

A job's `estimate` says what it expects to use: `cpus`, `memory_bytes` and
`disk_bytes`. Fields it leaves out fall back to the heavy-job defaults
(`heavy_estimate`) when the job is heavy. A job asks for heavy work with
`heavy: true`; a job that never says is heavy for CPU and memory but declares no
disk, so an unlabelled `sleep` or `gh` job reserves none. A job with no estimate
that is not heavy is exempt from CPU, memory and disk demand, though it still
obeys the floors.

Before any job starts, the stricter of the host floor (`min_free_disk_bytes`)
and its charter's floor (`min_free_bytes`) must be free on the filesystem the
job will use. On top of that, a job that declares disk must leave room for what
it says it will write, and for what the other reservations on that same
filesystem still have to write:

```
free >= max(host floor, charter floor) + this job's disk_bytes
                                        + the disk_bytes still to be written
                                          by running jobs on the same filesystem
```

A running job has already written some of its declared disk, and the free
reading shows that. So a job's remaining demand is its declared `disk_bytes` less
the drop in free space since it was admitted. Those drops are not counted twice:
the credit a set of concurrent jobs takes is capped at the drop since the oldest
of them was admitted. A job that has written its whole declaration no longer
holds space back from the next job.

Unlike load and memory, disk is not credited back for a job that has just
ended: its bytes are still on the filesystem until something removes them. A
reservation holds its declared disk for as long as the job runs, across homes on
the host. A job that does not fit is refused with the figures it refused on, or
waits in the host capacity queue when it was started with
`wait_for_capacity:true`. A job whose own declared space could not fit on the
filesystem even when it was empty (the floor plus `disk_bytes` is more than the
disk holds) is refused at once with `never fits`, rather than queued for the
whole wait limit.

The heavy default for disk applies only to a job started with `heavy: true`, and
it is a host-wide setting in `heavy_estimate.disk_bytes`. Two heavy jobs that each
declare the default need twice that plus the floor. Raise it when one heavy
build really writes more.

## Choosing the floor for a real workload

The floor is not a cleanup policy. It is the point below which new work stops,
so set it where you want the machine to stop rather than where a build happens
to fail. A workable sequence, on the machine in question:

1. **Find what one build really writes.** Run the heaviest real job the machine
   will run and read the drop in free space (`df` before and after, or
   `resource.status`'s `disk_available_bytes` before and after). That is the
   job's `disk_bytes`, and it belongs in the job's own `estimate` when it is
   not the typical heavy job.
2. **Add the headroom the machine needs to keep free.** Whatever the person
   wants available for their own work, plus whatever you want in reserve.
3. **Check the total.** A floor above the disk's real size refuses everything;
   `resource.status` reports the total, so check the arithmetic before setting
   it.

Take a machine where one build writes about 6 GiB and the person wants 4 GiB
left for their own work. The floor is the 4 GiB, and the heavy default is the
6 GiB a build writes:

```json
{"min_free_disk_bytes": 4294967296, "heavy_estimate": {"disk_bytes": 6442450944}}
```

A heavy build then starts only while about 10 GiB is free (4 GiB above the floor
plus its 6 GiB). Below that the machine refuses heavy work by name, with the
figures it refused on, instead of starting a build that fails a few minutes
later. Admission already takes the larger of the host floor and the charter's
`min_free_bytes` (2 GiB by default), so set the host floor to the number you want
for the machine.

Disk floors are per filesystem. A job on a second volume is measured there, and
a workspace checks the filesystem its worktree pool lives on, so one full disk
does not block work on another. Declared disk is counted per filesystem too: a
reservation on one volume does not hold back a build on another.
`heavy_estimate.disk_bytes` is a host-wide number, not a per-charter one.

## Hearing about it before it happens

A machine's owner (see `privateer machine owner SHIP`) is told when free space
falls below the floor. It is also told *before* that, while there is still room
to act. Every job admission and every owner check keeps a free-space reading for
the filesystem it measured, at most one every five minutes, for seven days, and
the readings are keyed by filesystem so work anywhere on that disk feeds the same
series. A falling rate is read from the last twelve hours of readings. The window
starts at the last cleanup (a rise of a gibibyte or more between two readings)
or at the last trim of build output (see below), whichever is later. The rate is
a least-squares fit, and it needs at least an hour of readings after that point,
so one build's writing cannot read as a machine filling up. When the rate
would reach the floor within `disk_warn_horizon_secs` (default 24 hours), the
owner is mailed a notice with the rate and the hours left.

The warning has hysteresis: once raised it clears only when the time to the floor
is more than twice the horizon, so a forecast hovering at the horizon does not
flap. While the floor alarm is up, a raised warning stays raised rather than being
cleared in the same pass, so the owner is not told "Cleared" about a disk that is
still under its floor. The floor's own notice clears on its own when space
recovers.

`resource.status` reports the readings the rate was read from (`disk_trend`:
the first and last reading, and the fitted rate) and `disk_forecast` (the rate
and when it reaches the floor), so an owner can check the arithmetic itself. A
horizon of 0 warns only once the floor is crossed.

A falling rate is not an order to clean up: only the machine's owner and the
captain decide what to remove. The runtime's part is to say so while there is
still time.

## Build output in released worktrees

A task's worktree is returned to its pool when its work has landed, but the
build output in it (a Rust `target` directory, for one) stays with the slot and
can run to gigabytes. A pool that is reused keeps those bytes; one that grows
keeps all of them. `workspace_cache_budget_bytes` (8 GiB by default) is how much
build output released slots of a pool may keep:

```json
{"workspace_cache_budget_bytes": 8589934592}
```

The budget is enforced in two places. When a workspace is released, and when a
workspace or job would not fit above the floor plus the disk held jobs still
have to write, the runtime trims the least recently released slots until the
pool is within budget and the space asked for is free. A held or leased slot is
never trimmed, however large its output. Trimming removes only the slot's
`target` directory, and only from a slot whose worktree is clean and tracks
nothing under `target`: source, uncommitted work and untracked files stay where
they are. A slot that does not qualify is skipped, and the next least recently
released one is tried. A trim that fails is recorded in the audit log and does
not undo the release.

A trim frees space that the job holding the disk did not write, so the job's
credit for its declared space is computed as if the trim had not happened. The
trend the owner sees restarts from the trim, so a trim is not read as a cleanup
rise or as a fall caused by a job. Setting the budget to 0 keeps no build output
in released slots; the next build there starts cold.

## Limitations

- A declared demand is what the job says, or the heavy default. A job that
  writes far more than it declared can still fill the disk; the readings and
  the owner's warning are what notice that, not admission.
- A running job's declared disk is released when its reservation ends, whether
  or not its files have been removed.
- A fall that starts right after a cleanup is seen only once the readings after
  the cleanup span an hour; until then the warning waits.
- The rate is read from free space alone. It cannot say what grew, or whether
  the space will come back.
- The floor applies to new work. Running jobs are not stopped when free space
  crosses it.