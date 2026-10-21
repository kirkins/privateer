"""Stand-ins for `open -a Xcode PROJECT` and the AppleScript close, acting on
the windows file the fake mcpbridge lists (--windows there).

  fake_xcode_window.py open|close --windows FILE --log FILE [options] PROJECT

Both append their argv to --log, one JSON line each.

open:   add {"tabIdentifier": "tab-N", "workspacePath": PROJECT}
  --delay SECS   add it this much later, from a detached child, and exit now
  --never        exit 0 and never add it
  --fail         print one line on stderr and exit 3
close:  remove every window whose workspacePath is PROJECT
  --fail         print one line on stderr and exit 4
  --remove PATH  also delete PATH (a file "Xcode" left in the worktree)
"""
import argparse
import fcntl
import json
import os
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument('action', choices=['open', 'close'])
parser.add_argument('--windows', required=True)
parser.add_argument('--log', required=True)
parser.add_argument('--delay', type=float, default=0)
parser.add_argument('--never', action='store_true')
parser.add_argument('--fail', action='store_true')
parser.add_argument('--remove')
parser.add_argument('project')
options = parser.parse_args()


def change(update):
    """Apply `update` to the windows list under a lock, replacing the file."""
    with open(options.windows + '.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        windows = []
        if os.path.exists(options.windows):
            with open(options.windows) as stream:
                windows = json.load(stream)
        windows = update(windows)
        temporary = options.windows + '.tmp'
        with open(temporary, 'w') as stream:
            json.dump(windows, stream)
        os.replace(temporary, options.windows)


def add(windows):
    used = {w['tabIdentifier'] for w in windows}
    number = 1
    while f'tab-{number}' in used:
        number += 1
    return windows + [{'tabIdentifier': f'tab-{number}', 'workspacePath': options.project}]


with open(options.log, 'a') as stream:
    stream.write(json.dumps({'argv': sys.argv[1:], 'cwd': os.getcwd(), 'pid': os.getpid()}) + '\n')

if options.action == 'open':
    if options.fail:
        sys.stderr.write('LSOpenURLsWithRole() failed: "no such app"\nsecond line\n')
        sys.exit(3)
    if options.never:
        sys.exit(0)
    if options.delay:
        if os.fork() == 0:
            os.setsid()
            devnull = os.open(os.devnull, os.O_RDWR)
            for fd in (0, 1, 2):
                os.dup2(devnull, fd)
            time.sleep(options.delay)
            change(add)
            os._exit(0)
        sys.exit(0)
    change(add)
else:
    if options.fail:
        sys.stderr.write('execution error: Xcode got an error (-1708)\n')
        sys.exit(4)
    change(lambda windows: [w for w in windows if w['workspacePath'] != options.project])
    if options.remove and os.path.exists(options.remove):
        os.remove(options.remove)
