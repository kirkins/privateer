#!/usr/bin/env python3
"""Treehouse protocol fixture backed by real Git worktrees, never user pools."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid

args = sys.argv[1:]
assert args.pop(0) == '--root'
pool = Path(args.pop(0))
pool.mkdir(parents=True, exist_ok=True)
state = pool / 'fixture-state.json'
slots = json.loads(state.read_text()) if state.exists() else []
# Real Treehouse discovers slots through markers in existing directories:
# deleting a checkout makes status omit it and return refuse it.
slots = [s for s in slots if Path(s['path']).is_dir()]
repo = Path.cwd()
if (repo.parent / 'flood-helper').exists():
    child = subprocess.Popen(['sleep', '20'])
    (repo.parent / 'flood-child').write_text(str(child.pid))
    while True:
        sys.stdout.write('x' * 8192)
        sys.stdout.flush()


def save():
    temporary = pool / 'fixture-state.tmp'
    temporary.write_text(json.dumps(slots))
    temporary.replace(state)


def git(*argv, cwd=repo):
    return subprocess.check_output(['git', '-C', str(cwd), *argv], stderr=subprocess.PIPE).decode().strip()


def option(flag):
    return args[args.index(flag) + 1]


if args[0] == 'status':
    assert args == ['status', '--json']
    restore = repo.parent / 'restore-on-status.json'
    if restore.exists():
        action = json.loads(restore.read_text())
        action['remaining'] -= 1
        if action['remaining'] == 0:
            Path(action['from']).rename(action['to'])
            restore.unlink()
        else:
            restore.write_text(json.dumps(action))
    override = repo.parent / 'status-override.json'
    if override.exists():
        print(override.read_text())
        sys.exit(0)
    if sys.platform.startswith('linux'):
        for slot in slots:
            slot_path = Path(slot['path']).resolve()
            procs = []
            for pid_entry in Path('/proc').iterdir():
                if pid_entry.name.isdigit() and pid_entry.name != str(os.getpid()):
                    try:
                        cwd = (pid_entry / 'cwd').resolve()
                        if cwd == slot_path or slot_path in cwd.parents:
                            procs.append(int(pid_entry.name))
                    except (OSError, ValueError):
                        pass
            slot['processes'] = procs
    print(json.dumps(slots))
elif args[0] == 'get':
    assert '--lease' in args and '--json' in args
    base, branch = option('--base'), option('--branch')
    if 'origin' in git('remote').splitlines():
        git('fetch', '--quiet', 'origin')
        base = 'refs/remotes/origin/' + base
    idle = next((s for s in slots if not s.get('lease_id')), None)
    if idle:
        slot = idle
        git('checkout', '--quiet', '-b', branch, base, cwd=slot['path'])
    else:
        number = 1
        names = {s['name'] for s in slots}
        while str(number) in names or (pool / str(number)).exists():
            number += 1
        path = pool / str(number) / repo.name
        path.parent.mkdir(parents=True)
        git('worktree', 'add', '--quiet', '-b', branch, str(path), base)
        slot = {'name': str(number), 'path': str(path)}
        slots.append(slot)
    slot.update(status='leased', branch=branch, flavor='git', lease_id=uuid.uuid4().hex,
                lease_holder=option('--lease-holder'), base_branch=base, processes=[])
    save()
    if (repo.parent / 'fail-acquire-response').exists():
        (repo.parent / 'fail-acquire-response').unlink()
        sys.exit(9)
    if (repo.parent / 'pause-acquire-response').exists():
        (repo.parent / 'pause-acquire-response').unlink()
        (repo.parent / 'helper-acquired').write_text(str(os.getpid()))
        time.sleep(1)
    print(json.dumps(slot))
elif args[0] == 'return':
    slot = next((s for s in slots if s['path'] == args[-1]), None)
    if slot is None:
        print('not managed by treehouse', file=sys.stderr)
        sys.exit(1)
    if slot.get('lease_id') != option('--if-lease-id') or slot.get('lease_holder') != option('--if-lease-holder'):
        sys.exit(3)
    if (repo.parent / 'fail-before-return').exists():
        (repo.parent / 'fail-before-return').unlink()
        sys.exit(9)
    assert not git('status', '--porcelain', '--untracked-files=all', cwd=slot['path'])
    git('checkout', '--quiet', '--detach', slot['base_branch'], cwd=slot['path'])
    slot.update(status='available', branch='', lease_id='', lease_holder='')
    save()
    if (repo.parent / 'fail-return-response').exists():
        (repo.parent / 'fail-return-response').unlink()
        sys.exit(9)
    print('returned')
else:
    raise AssertionError(args)
