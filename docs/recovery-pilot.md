# Isolated two-ship recovery pilot

This deterministic pilot exercises current mailbox, activation and crew machinery through real CLI, dispatcher, adapter and supervised-process boundaries. Its first mates and crew follow a fixed Python fixture. It makes no model calls and does not establish native agent judgement or full fleet autonomy.

```sh
cargo build --locked --bin privateer
python3 scripts/recovery-pilot.py --binary target/debug/privateer
```

The script prints a small JSON summary with `passed`, `native_inference`, `directory` and `evidence`. The evidence directory is retained on success and failure. `--output /absolute/new-directory` chooses a new directory; existing paths are refused. `--timeout 90` sets the scenario deadline, from 30 to 300 seconds. Cleanup switches to its own bounded command log and 20-second deadline before stopping dispatchers or querying jobs, so an exhausted scenario log or deadline cannot block payload cancellation. The normal Cargo integration suite runs the pilot and an exhausted-log/live-payload cleanup regression through `tests/recovery_pilot.rs` with its own compiled binary.

## Scenario and evidence

1. Create a failing addition regression in a temporary Git repository, two homes (`review` and `apps`), fixture-only charters and a shared private resource ledger. The script replaces inherited `PRIVATEER_*`, HOME and XDG settings and disables service-manager integration. No real fleet, Herdr socket, provider or credential configuration is used.
2. Offer and accept the `repair` task in the apps home. The original owner remains recorded until acceptance; apps then becomes its sole owner. The separate `outcome` task stays owned by review in the review home. Task IDs and custody are home-local: this does not transfer a task between homes or qualify distributed task custody.
3. A captain request wakes review's fixture first mate. It sends a durable request to apps over local peer RPC. Apps exchanges a correlated scope check with review and starts one supervised crew job under its accepted task.
4. Crew sends an idempotent decision request with `crew.report`; apps' fixture first mate replies, and crew reads it using `crew.answer`. A gate holds the payload and an owned helper alive while the driver records their process identities.
5. Kill the apps dispatcher and wait for that exact child to exit. Separately verify that the recorded worker and helper remain alive and the job remains running. Release the gate: crew repairs the fixture, runs real regressions and commits locally while its dispatcher is absent.
6. Independently rerun the regressions and compare the Git head with the worker's recorded tested head. Check the durable crew outcome before restarting the dispatcher. Replay `job.start` and the original mail ID, requiring the same incarnation, one actual worker invocation, one decision and one crew outcome.
7. Restart apps' dispatcher. Its first mate consumes the durable outcome, completes its task and mails the result to review, which answers the captain and completes its own outcome. Inspect both homes' copies of the correlated request/reply and the unchanged task owners.
8. Stop both dispatchers, reconcile and cancel any remaining fixture jobs, and inspect recorded payload identities and process-group membership. A wrapper exit or a successful job status alone does not establish cleanup. Passing evidence requires no live payload group members, no live recorded worker/helper, no remaining reservations and no queued capacity requests.

The crew reservation requests one CPU and 64 MiB, with one heavy slot and a bounded capacity wait. First mates have a wake budget and jobs have deadlines. The workload is a small real Python repair rather than a Rust build or live model request.

`evidence.json` records binary, pilot and fixture SHA-256 hashes; named assertions with observed records; the tested fixture commit; cleanup results; and paths to logs. `commands/*.jsonl` records explicit argv, exit results, bounded stdout/stderr and elapsed time for the driver, first mates and crew. Each command has a deadline and a streaming 64 KiB combined output cap; each command log has a 1 MiB cap. Dispatcher logs, runtime job logs, homes, generated fixture adapter and Git repository remain available for inspection. Evidence includes fixture prompts and paths, so treat the retained directory as private operational state.

The pilot uses the operating system account running it; its charters and role markers are workflow controls. Its assertions cover one bounded local scenario. Deliberately escaped process groups, arbitrary crash points, sustained load, cross-host reconnects, upgrades of existing homes and credential isolation require separate qualification.

## Native trial preparation — pending profile selection

The fixture runner accepts no native manifest or provider options. Native first-mate and crew profiles have not been selected for the next trial, and this change runs no inference. A later native trial needs its own explicit, reviewed invocation with these recorded inputs and checks:

1. Select the exact Privateer commit/binary, two first-mate profiles and a crew profile. Record resolved harness executables and versions, manifest hashes, supported headless/resume/attended modes and selected provider routes. Keep models, endpoints, account names and credentials in private profile configuration; exported evidence records route checks and redacted identifiers, never secret values. Qualify each selected profile's native event/session semantics before treating resume or usage as established.
2. Use newly created homes, a separate registry and ledger, and a disposable regression repository. Grant only fixture inspection, implementation, tests, messages, spawning and cleanup. Give the native run explicit model-call/cost, wake, elapsed-time and heavy-work limits before launch. No merge, deployment, live fleet replacement or existing-home migration belongs to this trial.
3. Run the existing [single-profile native canary](pilot.md#native-harness-canary) for any selected profile it actually supports. Other profiles need equivalent opt-in canaries; a fixture adapter passing does not qualify them. Build a separate opt-in two-ship runner that reuses this scenario's observable gates without replacing those gates with model prose.
4. Ask the selected first mate to diagnose the failing fixture and delegate a repair, exchange an addressed request/reply with its peer, and handle one crew decision. Independently validate changed code and tests at the recorded commit. Interrupt and restart the dispatcher only after recording the actual supervised payload identity; require durable result recovery, one accepted owner per local task and no duplicated worker/outcome. Cross-home assignment, if required, needs a real custody protocol rather than copied task records.
5. Stop on route mismatch, unbounded retries, budget exhaustion, missing process identity, stale/replayed outcomes, lost custody or independent regression failure. Cancel through the supervisor, verify surviving payload groups and ledger cleanup, and retain versioned evidence. A passing native trial establishes only those chosen profiles and that isolated workload; broader autonomy and production readiness remain separate claims.

The existing [runtime pilot](pilot.md) and this recovery pilot can run before profile selection. Actual native calls and any real SSH canary remain separate, opt-in work.
