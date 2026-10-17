# Isolated pilot

Run `python3 scripts/pilot.py --binary target/debug/privateer` after building. It uses the actual CLI and supervisor binary, temporary SQLite homes, actual subprocesses, and an actual temporary Git repository. It retains its evidence directory so the result can be inspected.

The scripted pilot pins the repository's Treehouse protocol fixture, which creates real Git worktrees, into its temporary homes. No installed Treehouse binary or user pool is used. The repair runs and is independently tested in the task's leased worktree; the source checkout retains the failing fixture. External Treehouse CLI compatibility is covered by the separate opt-in contract test.

The navigator owns the overall outcome. An application department owns the repair task and an infrastructure department owns a diagnostic task. Each home receives an explicit local charter for the same temporary fixture; this is not a test of distributed charter inheritance. Departments communicate through direct peer RPC and task-linked messages.

The pilot emits many facts for one issue and verifies one pending notification with the facts retained. The application job deliberately remains alive after the `job.start` launcher exits. A separate heavy job is refused while that shared host slot is occupied. The repair changes the fixture, runs a meaningful regression test, commits the result locally, and records durable completion. Fresh CLI invocations inspect the resulting state.

No model is pretending to make decisions in this script. It is a reproducible test of runtime semantics, not proof of autonomous Pi/Codex/Claude orchestration. It does not contact a provider, test a real GLM gateway, push a branch, open a forge PR, merge production work, or touch a live mate's home.

Peer integration tests additionally run fake SSH programs as actual subprocesses. They exercise remote-shell quoting, correlation, invalid endpoints, byte limits, blocked stdin, inherited output pipes, and timeout cleanup. A real two-host SSH canary is separate and must report exactly which remote behavior was exercised.

`scripts/ssh-canary.py` takes an explicit host, preinstalled remote binary, and new temporary remote home/repository paths. It refuses existing state, checks actual SSH RPC, owner acceptance, targeted inboxes, repeated-event aggregation, and enforcement of a revoked remote charter. It can remove the temporary remote state with `--cleanup-remote`. This tests a remote home's local authority; it does not claim cross-host charter inheritance. No machine alias is embedded in the script.

Before any fleet replacement, qualify the selected native harnesses and hooks, real provider routing, cross-host assignment/custody, forge delivery, restart recovery under load, and migration of existing work. Preserve existing profiles, credentials, notes, and jobs through a separately reviewed migration.

## Native harness canary

`scripts/native-canary.py` is a separate opt-in test that makes a real Codex request using the existing native login. It creates a temporary Git regression fixture, starts an agent through the pinned plugin, verifies its code with independent tests, and checks that the agent recorded a task-linked message through the actual runtime. It uses an ephemeral workspace-write session with the operational home explicitly writable. It does not copy credentials, alter global harness settings, or use a live fleet home.

```sh
python3 scripts/native-canary.py --binary target/debug/privateer --manifest plugins/codex/manifest.json
```

Native-agent jobs clean up their recorded ordinary descendants on completion, including descendants that started a process group or session of their own. Raw jobs keep surviving descendants and their capacity until those exit, then settle to the leader's result; `job.cancel` stops them sooner. Neither mode provides hard containment for children that deliberately escape their process group.
