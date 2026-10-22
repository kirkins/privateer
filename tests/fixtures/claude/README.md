# Captured Claude Code streams

Claude Code 2.1.300 (`claude -p --output-format stream-json --verbose`), captured on
2026-10-18 in a scratch git repository with no settings changes. Each stream is reduced
to the fields Privateer reads (init, tool use and results, the final `result`); local
paths read `/tmp/scratch`, and usage keeps only token counts. Nothing here is a credential.

| file | run | what it shows |
|---|---|---|
| `denied-accept-edits.jsonl` | `--permission-mode acceptEdits`, asked to run `curl` | The denial is in `result.permission_denials[]` as `tool_name`, `tool_use_id` and the full `tool_input`. The run still ends `subtype: success`, `is_error: false`. The tool result reads "This command requires approval". |
| `denied-dont-ask.jsonl` | `--permission-mode dontAsk`, the same request | Same denial shape as `acceptEdits`. |
| `hooks-dont-ask-denial.jsonl` | the `dontAsk` run with a session-only `--settings` file that logs every hook event | A denial fires `PreToolUse` and nothing after it: no `PostToolUse`, `PostToolUseFailure` or `PermissionDenied`. An attended hook can only infer a denial from a `PreToolUse` with no `PostToolUse`. |
| `auto-mode-not-refused.jsonl` | `--permission-mode auto`, asked to run `sudo -n rm -rf` on a decoy directory | The auto-mode check allowed it (as it did a force push to a local bare remote). `permission_denials` is empty; the result carries `safety_stops: 0`. No auto-mode refusal was captured, so its shape is still unverified. |
| `settings-reload.md` | 2026-10-21, `--permission-mode auto --settings <file>` against a fake local API (`settings_reload_spike.py`), headless and in a tmux pane | The `--settings` file is read once at launch. Rewriting it in place or by rename never reached the auto-mode classifier of the running session (same pid and session); a restart did. Approvals recorded mid-session need a relaunch. |

`pane-retries.jsonl` is hand-written in the shape of a hosted pane's 2.1.300 transcript
(the captain's fitforge report of 2026-10-22, whose transcript stayed on its machine), with
each tool use repeated first in an `api-request-blob` record as 2.1.300 writes them: a Bash
edit of `scratch/nativest.json` the classifier refused as `[Auto-Mode Bypass]` at 00:06:16,
an `Edit` a rule refused, a `git push` refused, then the same Bash command and the same
file's `Edit` succeeding at 00:45:50 and 00:45:56. The push stays refused.
