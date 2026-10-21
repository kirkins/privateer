"""Executables the stdio tests may pin, chosen by bundles' own rule.

A stdio server's executable must pass bundles.executable(): every component
owned by root or this user, none a symlink or group- or world-writable. The
Python running the tests need not pass it (a framework Python under
/Library/Frameworks, root:admin 775, does not), and it is not the one that
matters for the launcher: bundles.interpreter() runs the launcher unpinned,
while the server's interpreter is pinned. So a test that runs a real server
pins the first pinnable Python at least as new as this one, or skips naming
why each candidate was refused. Tests that only need a pinnable file use a
copy in private_directory()."""
import functools
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile

import bundles

ROOT = Path(__file__).resolve().parents[1]
CANDIDATES = ('/usr/bin/python3', '/usr/local/bin/python3', '/opt/homebrew/bin/python3')


def refused_part(path):
    """The first component of `path` the rule refuses, for a skip reason. This
    only describes a refusal; bundles.executable() makes the decision."""
    for part in [*reversed(path.parents), path]:
        try:
            info = part.lstat()
        except OSError as error:
            return f'{part}: {error.strerror}'
        if stat.S_ISLNK(info.st_mode):
            return f'{part} is a symlink'
        if info.st_mode & 0o022:
            return f'{part} is group- or world-writable ({stat.filemode(info.st_mode)})'
        if info.st_uid not in (0, os.geteuid()):
            return f'{part} is owned by uid {info.st_uid}, neither root nor this user'
    return f'{path} is not a regular executable file its owner may run, or is a #! script'


def version(path):
    """A candidate's (major, minor), or None if it does not answer."""
    try:
        run = subprocess.run([path, '-I', '-c', 'import sys; print(*sys.version_info[:2])'],
                             capture_output=True, text=True, timeout=30)
        return tuple(int(part) for part in run.stdout.split()) if run.returncode == 0 else None
    except (OSError, ValueError, subprocess.TimeoutExpired):
        return None


@functools.cache
def pinned_python():
    """(path, sha256) of the first pinnable Python at least as new as this one:
    this one resolved, then the usual absolute installs resolved. Otherwise
    (None, reason), the reason naming each refused path and why."""
    wanted = sys.version_info[:2]
    paths = [os.path.realpath(sys.executable)]
    paths += [os.path.realpath(path) for path in CANDIDATES if os.path.exists(path)]
    refusals = []
    for path in dict.fromkeys(paths):
        try:
            digest = bundles.executable(path)
        except (bundles.Refused, OSError):
            refusals.append(f'{path} refused: {refused_part(Path(path))}')
            continue
        found = wanted if path == paths[0] else version(path)
        if found is None or found < wanted:
            have = 'does not report a version' if found is None else 'is Python {}.{}'.format(*found)
            refusals.append(f'{path} {have}, older than this {wanted[0]}.{wanted[1]}')
            continue
        return path, digest
    return None, 'no pinnable Python to run a stdio server: ' + '; '.join(refusals)


def require_pinned_python(case):
    """The pinned server Python for a test that runs one, or skip it."""
    path, value = pinned_python()
    if path is None:
        case.skipTest(value)
    return path, value


def private_directory(case):
    """A directory under the checkout whose every ancestor the rule accepts,
    removed after `case`, or a skip: a checkout below a group-writable folder
    cannot hold a pinnable fixture."""
    temp = tempfile.TemporaryDirectory(prefix='.stdio-fixture-', dir=ROOT)
    case.addCleanup(temp.cleanup)
    path = Path(temp.name).resolve()
    path.chmod(0o755)
    for part in [*reversed(path.parents), path]:
        info = part.lstat()
        if info.st_mode & 0o022 or info.st_uid not in (0, os.geteuid()):
            case.skipTest(f'{part} is writable by others or not ours: the rule refuses every path below it')
    return path
