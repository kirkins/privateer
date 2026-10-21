# Does a running session re-read its `--settings` file?

**No.** Claude Code 2.1.300 reads the `--settings` FILE once, at launch. A rewrite while the
session runs never reaches the auto-mode classifier, whether the write is in place or a
rename, headless or interactive. Restarting on the rewritten file does pick it up.
A captain approval recorded mid-session therefore needs a relaunch (PR 3) to widen
`autoMode.allow`; Privateer keeps passing first-mate settings inline.

Captured on 2026-10-21 on this laptop (Linux, aarch64) with
`tests/fixtures/claude/settings_reload_spike.py`, against a fake Messages API on
127.0.0.1, a fake key (`sk-ant-spike-fake`), an isolated HOME/XDG tree, and
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`. No real account was used.

## Setup

- `claude --permission-mode auto --settings <case>/s.json` in a scratch git repo. `s.json`
  starts as `{"autoMode":{"allow":["$defaults","SPIKE-BASELINE-<nonce>"]}}`. The rewrite
  adds `"SPIKE-LIVE-<nonce>"`.
- The fake main model answers each prompt with one Bash call,
  `curl -s http://127.0.0.1:<port>/probe-<n>`, and then text. The classifier is the
  tool-less request (`model: claude-sonnet-5`, `max_tokens: 64`,
  `stop_sequences: ["</severity>"]`) whose system prompt lists the allow rules inside
  `<cc_automode_permissions>`. The fake answers it `<severity>5`, which allows.
- A case passes as a reload only if a classifier request made after the rewrite lists
  SPIKE-LIVE in its system prompt, from the same pid and session.
- Each case: prompt 1, wait for its result, rewrite `s.json`, wait 10 s, prompt 2.

## Results

| case | mode | write style | same pid / session | classifier after rewrite lists | reload |
|---|---|---|---|---|---|
| headless | `-p --input-format stream-json` | in place (truncate, write) | yes / yes | baseline only (t+10.1 s) | NO |
| headless | `-p --input-format stream-json` | temp file, rename over | yes / yes | baseline only (t+10.1 s) | NO |
| interactive | tmux pane | in place (truncate, write) | yes / yes | baseline only (t+10.6 s) | NO |
| interactive | tmux pane | temp file, rename over | yes / yes | baseline only (t+10.6 s) | NO |
| control | tmux pane, launched on the rewritten file | none | n/a | baseline and SPIKE-LIVE | (sees it) |
| self-edit | headless; the model Reads then Writes `s.json` | the session's own Write tool | yes / yes | baseline only | NO |

Reduced excerpt, the tail of `<cc_automode_permissions>` in the classifier's system
prompt (interactive, in place; the rename case is identical apart from the nonce):

```
before the rewrite (t-1.0 s):  ...localhost, or a URL the user explicitly named in their request.\n- SPIKE-BASELINE-25548b75
after the rewrite (t+10.6 s):  ...localhost, or a URL the user explicitly named in their request.\n- SPIKE-BASELINE-25548b75
control restart:                - SPIKE-BASELINE-d5c15784\n- SPIKE-LIVE-6ade6275
```

Other things the spike showed:

- The first main-loop request of a session also carries the auto-mode config
  (`"auto_mode": {"allow": ["$defaults", "SPIKE-BASELINE-..."], ...}`) next to the
  permission rules; later main-loop requests do not repeat it.
- Self-edit: the session's Write to its own `--settings` file was sent to the classifier
  like any other write, with no built-in settings-file guard before it. The fake classifier
  allowed it, the file changed on disk, and the classifier still saw only the baseline.
  A settings file the session can write is guarded only by the classifier, so a file it
  must not widen belongs outside every directory it can write.
- Interactive sessions with a non-Anthropic `ANTHROPIC_BASE_URL` show a one-time
  classifier billing notice ("Enter to continue") that holds the first tool call until it
  is dismissed. The harness presses Enter for it.
- The scratch repo sits under this repository's `target/`, so the classifier's context
  also quotes this repository's `AGENTS.md`. It does not affect the result.
- A pane held at an approval or lockout prompt was not tested; no reload exists for it to
  pick up.

## Rerun

```
CLAUDE_BIN=<real claude binary, not a mise/asdf shim> \
  python3 tests/fixtures/claude/settings_reload_spike.py headless --write inplace
# likewise: headless --write rename, tmux --write inplace, tmux --write rename,
#           control, headless --self-edit
python3 tests/fixtures/claude/settings_reload_spike.py summary
```

Each case writes `target/reload-spike/<case>-<time>/` with `requests.jsonl` (every request
body), `meta.json` (pids, rewrite time, per-request flags), and `stdout.jsonl` or
`screens.txt`. The tmux cases use a detached session named `reload-spike` and kill only
it. Run one case at a time.
