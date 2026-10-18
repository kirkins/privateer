# Harness logs for `wake.transcript`

Job logs (`jobs/<id>/<incarnation>/stdout.log`) as headless wakes leave them, reduced to the
fields the transcript reads.

| file | shape |
|---|---|
| `claude-wake.jsonl` | Claude Code `-p --output-format stream-json --verbose`, built on the captured streams in `../claude/`, then the runtime's `turn.completed` line |
| `codex-wake.jsonl` | `codex exec --json` events (`thread.started`, `item.started`/`item.completed`, `turn.completed`), written to that shape, not captured |
| `pi-wake.jsonl` | Pi `--mode json` events with a tool call and its result, written to the shape `plugins/native_turn.py` reads, not captured |
| `../errand-rotated-pi.log` | a real Pi errand log whose middle the job log dropped |

Values containing `PLANTED` are planted secrets the transcript must mask; nothing here is a
credential.
