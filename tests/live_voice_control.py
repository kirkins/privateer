"""The fake app-server's append-only control logs, written and read safely.

`tests/fake_live_voice_app_server.py` records every message it sees as one JSON
line under `FAKE_VOICE_DIR`. Tests run in another process and poll those logs
while the fake is still appending, so two rules keep a poll from parsing a
half-written record:

- `append_record` writes the whole line with one `write(2)` on an `O_APPEND`
  descriptor. Nothing is left in a stdio buffer that a later record could
  splice into, and no other writer's append can land in the middle of it.
- `read_records` counts only newline-terminated lines. A large write can still
  become visible to a reader a piece at a time (the kernel copies it in
  steps), so a trailing fragment that has not reached its newline is not a
  record yet: the poller asks again. A finished line that is not JSON stays an
  error, so tolerance here never hides a corrupt log (#713).

Callers serialize concurrent `append_record` calls themselves; the fake holds
one lock around every record.
"""

import json
import os


def append_record(path, message):
    """Append one newline-terminated JSON record as a single whole line."""
    payload = (json.dumps(message) + "\n").encode("utf-8")
    descriptor = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    try:
        while payload:
            payload = payload[os.write(descriptor, payload):]
    finally:
        os.close(descriptor)


def read_records(path):
    """The finished records in `path`; a trailing partial line is not one yet."""
    try:
        with open(path, "rb") as f:
            raw = f.read()
    except OSError:
        return []
    return [json.loads(line) for line in raw.split(b"\n")[:-1]]
