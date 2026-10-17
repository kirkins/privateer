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
privateer --home ~/.ship .privateer resource.configure --json \
  '{"min_free_disk_bytes": 16106127360, "heavy_estimate": {"disk_bytes": 10737418240}}'
privateer --home ~/.ship .privateer resource.status --json '{}'
```

`resource.status` shows the settings beside the live readings: free and total
space, the disk the home's work uses, the reservations held, the capacity
queue, and `disk_trend` and `disk_forecast` (see below).

## What admission counts

A job's `estimate` says what it expects to use: `cpus`, `memory_bytes` and
`disk_bytes`. Fields it leaves out fall back to the heavy-job defaults
(`heavy_estimate`), and a job with no estimate at all that is not heavy is
exempt from CPU, memory and disk demand, though it still obeys the floors.

Before any job starts, the stricter of the host floor (`min_free_disk_bytes`)
and its charter's floor (`min_free_bytes`) must be free on the filesystem the
job will use. On top of that, a job that declares disk must leave room for what
it says it will write, and for what every other reservation on the host has
declared:

```
free >= max(host floor, charter floor) + this job's disk_bytes
                                        + the disk_bytes of running jobs
```

Unlike load and memory, disk is not credited back for a job that has just
ended: its bytes are still on the filesystem until something removes them.
Every live reservation therefore holds its declared disk for as long as the job
runs, across homes on the host. A job that does not fit is refused with the
figures it refused on, or waits in the host capacity queue when it was started
with `wait_for_capacity:true`.

The heavy default is 4 GiB of disk (every CPU, 2 GiB of memory). Two heavy jobs
that each declare 4 GiB need 8 GiB plus the floor. Raise it when one heavy build
really writes more.

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

For a Mac that builds iOS apps, one build or test can use 10 GB or more, so:

```json
{"min_free_disk_bytes": 16106127360, "heavy_estimate": {"disk_bytes": 10737418240}}
```

15 GiB must stay free and each heavy job asks for 10 GiB: with 8.6 GiB free the
machine refuses heavy work by name, with the figures it refused on, instead of
starting a build that fails a few minutes later. Set the host floor to the
stricter of that and the per-charter `min_free_bytes`, which is 2 GiB by
default.

Disk floors are per filesystem. A job on a second volume is measured there, and
a workspace checks the filesystem its worktree pool lives on, so one full disk
does not block work on another. `heavy_estimate.disk_bytes` is a host-wide
number, not a per-charter one.

## Hearing about it before it happens

A machine's owner (see `privateer machine owner SHIP`) is told when free space
falls below the floor. It is also told *before* that, while there is still room
to act: every job admission and every owner check keeps a free-space reading
for the filesystem it measured, at most one reading every five minutes, for
seven days: an admitted job records the reading it decided against, and
every owner check records one. When the oldest and newest of those readings are at least half an
hour apart and the rate they describe would reach the floor within
`disk_warn_horizon_secs` (default 24 hours), the owner is mailed a notice with
the rate and the hours left. The notice clears when the rate stops.

`resource.status` reports the same numbers as `disk_trend` (the oldest and
newest readings) and `disk_forecast` (the rate and when it reaches the floor),
so an owner can check the arithmetic itself. A horizon of 0 warns only once the
floor is crossed. The readings are keyed by filesystem, so work anywhere on that
disk feeds the same series.

A falling rate is not an order to clean up: only the machine's owner and the
captain decide what to remove. The runtime's part is to say so while there is
still time.

## Limitations

- A declared demand is what the job says, or the heavy default. A job that
  writes far more than it declared can still fill the disk; the readings and
  the owner's warning are what notice that, not admission.
- A running job's declared disk is released when its reservation ends, whether
  or not its files have been removed.
- The rate is read from free space alone. It cannot say what grew, or whether
  the space will come back.
- The floor applies to new work. Running jobs are not stopped when free space
  crosses it.