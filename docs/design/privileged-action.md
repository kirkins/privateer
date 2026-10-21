# Captain-approved privileged actions

Status: implemented (change 7b). The captain approved the design on
2026-10-20 with both questions answered yes. On 2026-10-21 they settled
expiry: an approved command stays runnable until the request's own 24-hour
expiry.

## The problem

Sometimes a ship needs one command run as root: installing a package,
restarting a system service, or fixing a permission. Today the ship asks in
prose, and the captain copies a command into a terminal. Nothing ties what
was approved to what runs, and the ship never gets a record of the result.

## Shape

1. **Stage.** A first mate runs `privileged.stage {from, task_id, program,
   args[], cwd, reason}`. The runtime checks the command, binds it with a
   hash and writes one captain request,
   `request_kind:"privileged_action"`. Only the runtime writes this kind:
   `mail.send`, peer delivery and collection refuse it, along with any
   `payload.privileged` or `payload.privileged_result`, whoever the caller.
   Crew cannot stage. They ask their first mate.
2. **Approve (Q2).** The captain grants or denies it from the dashboard
   bridge or from a front door (the Telegram buttons). Both go through the
   existing correlated `approval.grant` / `approval.deny` path, with
   `request_id` and `expected_version`. The grant writes one row in
   `approvals` with `artifact = privileged:sha256:<hash>` and
   `environment = host:<machine>`, so the table needs no new column. The ship
   is told "approved, waiting for the captain at the machine".
3. **Confirm and run (Q1).** At the machine, the captain runs
   `privateer privileged run ID`. The dashboard opens this in a Herdr pane
   after a grant, or shows the command line to run. The runtime prints the
   full hash and the command again, and the captain types the first 8 hex
   characters of the hash on `/dev/tty`. It asks every time, whatever the
   sudoers rule, because the captain's laptop runs sudo with NOPASSWD. Under
   the write lock the runtime uses up the approval and records a
   `privileged_runs` row, then runs `sudo -k -- PROGRAM ARGS...` with no
   shell. sudo asks for the password when its rules require one.
4. **Report.** The exit code and the last 64 KiB of output are saved. The
   request ends, and the ship gets a notice in the request's thread.

## Binding

The hash is SHA-256 over canonical JSON (keys sorted, no whitespace):

```
{"argv":[program,...args],"cwd":..,"machine":..,"program":<canonical path>,
 "program_sha256":<sha256 of the file's bytes>,"request_id":..,"ship":..,
 "task_id":..,"v":1}
```

The request payload stores `privileged{v, program, program_sha256, args,
argv, cwd, reason, machine, ship, task_id, hash}`, plus `artifact`,
`environment` and `charter_hash`, as `production_approval` does. The hash is
recomputed three times:

- in `needs` before the item is listed or acted on;
- in `approval.grant` / `approval.deny`, which also check the artifact and
  environment strings;
- in `privileged.run`, from the stored record and the program file on disk.
  `run` also checks that the machine is still this one and that the task's
  charter hash has not changed.

Any mismatch refuses.

## Refusals at staging, rechecked at run

- **Shape.** The program must be an absolute path. At most 64 arguments,
  each at most 4 KiB, 16 KiB in total. No control characters or NUL
  anywhere. The working folder must be absolute and exist. The reason is 1
  to 1024 bytes. Staged environment variables are refused: sudo's
  `env_reset` strips them anyway unless sudoers grants SETENV.
- **Programs**, matched on the basename of the canonical path:
  - shells: `sh`, `bash`, `zsh`, `dash`, `ksh`, `fish`, `csh`, `tcsh`,
    `busybox`;
  - launchers: `env`, `nice`, `nohup`, `timeout`, `stdbuf`, `time`, `xargs`,
    `find`, `watch`, `script`, `chroot`, `nsenter`, `unshare`, `setpriv`,
    `runuser`, `systemd-run`, `ssh`, `tmux`, `screen`;
  - privilege tools: `sudo`, `sudoedit`, `su`, `doas`, `pkexec`, `visudo`;
  - interpreters: `python*`, `perl*`, `ruby*`, `node`, `deno`, `bun`,
    `php*`, `lua*`, `tclsh`, `osascript`, `awk`, `gawk`, `mawk`, `sed`;
  - editors and pagers: `vi*`, `nvim`, `emacs`, `nano`, `less`, `more`,
    `man`.
- **Arguments.** Any argument containing `sudoers` is refused.
- **Program file.** After canonicalizing, the file and every parent folder
  must be owned by root and not writable by group or others. Homebrew
  programs under `/usr/local` or `/opt/homebrew` are therefore refused, which
  is intended.

## Kind checks

`mail::is_approval_kind()` stands for "production or privileged" at every
place that used to compare against `production_approval`:

- block-ask attachment (send and deliver);
- the charter recheck in send;
- `needs::approval_request` and the needs listing;
- `captain.relay`, `captain.reply` and `captain.dismiss`, which all refuse;
- the bridge.

`away.rs` still marks only production approvals urgent.

The request kinds are kept apart:

- A captain `mail.send` reply to a privileged request is refused, so a
  reply never closes or grants it.
- `approval.grant` without `request_id`, `approval.consume`,
  `delivery.check`, and a `production_approval` request all refuse an
  environment starting `host:` or an artifact starting `privileged:`. Only
  `privileged.run` uses up a privileged approval.
- `payload.approves` (captain approvals, #338) is refused on a privileged
  request, as on a production approval.

## Where it can be approved

The needs item lists the request as `kind:"approval"` with an
`item.privileged` block. That block holds the argv one per line (control and
non-ASCII characters escaped), the working folder, the reason and the hash.
Grant and deny actions are offered only when the request was written in a
home on this machine. A copy carried to a desk on another machine is shown
with no actions, because the program file, the sudo and the terminal are on
the machine that staged it.

The door's new request, `{"command":"approval","args":{"id","action"}}`,
finds the home on this machine that holds the request and re-reads its needs
item. It then dispatches the item's own grant or deny with `request_id` and
`expected_version`, as the captain. It refuses every kind but
`privileged_action`, so production approvals do not quietly become
approvable from the phone.

## Run

`privileged.run` runs only as `privateer privileged run ID
[--timeout-secs N]` from a terminal. Calling it through `dispatch`, RPC, a
gateway or a door is refused.

**Refusals.** It refuses when:

- `PRIVATEER_ROLE` or `PRIVATEER_JOB` is set;
- `/dev/tty` cannot be opened;
- the request is not approved;
- the approval row is used;
- a run row exists;
- the hash, machine or charter changed;
- the request is past expiry.

**Expiry.** There is one constant, `PRIVILEGED_REQUEST_TTL_MS` = 24 hours
from the request's creation. It covers both an unapproved request and an
approved one not yet run. There is no shorter limit after approval. The
dispatcher's serve pass ends lapsed requests as `expired` and tells the
ship.

**Confirmation.** The hash prefix is read from `/dev/tty`, never from stdin.

**Use it up first.** The approval is marked used and the `privileged_runs`
row committed before sudo starts. A crash afterwards leaves a used approval
and an unfinished run row, never a second run.

**Process.** sudo is found only at `/usr/bin/sudo` or `/bin/sudo`. It is
started with `env_clear()` and then only `PATH=/usr/sbin:/usr/bin:/sbin:/bin`,
`TERM` and `LANG`, in the staged working folder. stdin is the terminal, and
stdout and stderr are piped. Output is copied to the screen live and the
last 64 KiB is kept. The process runs in its own process group. The timeout
defaults to 600 s (at most 3600 s). On timeout the group gets SIGTERM, then
SIGKILL after 5 s.

**Tests.** They inject a fake sudo through `PRIVATEER_SUDO`, which is read
only when built with the `test-sudo` feature. Release builds always use the
system sudo.

## Limits

- This is a workflow guard under one OS account, like the rest of
  Privateer's role checks. It is not a security boundary against a hostile
  same-user process. Such a process could read the hash from a Herdr pane
  and type the prefix, or run sudo itself where sudoers is NOPASSWD. The
  captain's typed prefix and sudo's own password are what this adds.
- With `use_pty` (the default in sudo 1.9.14 and later), sudo runs the
  command in its own pty and reads the password from `/dev/tty`. Whether the
  prompt reaches the captain while stdout is piped has not been checked by
  hand: the change was built without running real sudo. The captain should
  confirm it once. macOS is unchecked.
- A request staged on a machine whose captain desk is elsewhere is approved
  and run on the staging machine.
