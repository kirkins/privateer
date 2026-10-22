#!/usr/bin/env python3
"""Operator-approved bundle registry, selection and content pins, shared by
the Pi and Claude loaders. Exposure hygiene, not an OS/credential sandbox.

Only an operator-owned registry may approve resources. Plans contain selected
references and content pins, never credential values or unselected resources.
A bundle's credentials are names and stored refs; a launch gets only the path
of each selected ref's captain-stored file, never its value.
No installs, config writes, agentDir/HOME changes or provider selection here.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
from urllib.parse import urlsplit

sys.dont_write_bytecode = True  # a release holds only its own files
from protocol import serve

NAME = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*\Z")
REF = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*@[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}\Z")
HEX = re.compile(r"[0-9a-f]{64}\Z")
SEMVER = re.compile(r"[0-9]{1,4}\.[0-9]{1,4}\.[0-9]{1,6}\Z")
# Credential env names and stored refs: identifiers, never inline values.
CREDENTIAL_NAME = re.compile(r"[A-Z][A-Z0-9_]{0,63}\Z")
CREDENTIAL_REF = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*\Z")
CREDENTIAL_ENV = "PRIVATEER_CREDENTIAL_"
MAX_FILES = 128
MAX_BYTES = 2 * 1024 * 1024
# Placeholder recorded in plans that carry no Pi version (Claude plans).
NO_PI_VERSION = "0.0.0"
# An option row of a CLI's --help: exactly two spaces, then the flag spec.
OPTION_ROW = re.compile(r"  (-\S.*)")
FLAG = re.compile(r"(?<![\w-])(--?[A-Za-z][\w-]*)")
# Options declared by each installed CLI, keyed by (path, reported version).
_OPTIONS = {}


class Refused(ValueError):
    """A registry refusal. `field` names the failing rule, `bundle` the bundle
    it belongs to. Neither ever carries an input value: only the public message
    reaches the operator."""

    def __init__(self, field=None, bundle=None):
        super().__init__("unsupported bounded resources")
        self.field = field
        self.bundle = bundle

    @property
    def public_message(self):
        target = f"bundle {self.bundle}" if self.bundle else "registry"
        return f"{target} refused: {self.field or 'shape'}"

    def __str__(self):
        return self.public_message


def require(condition):
    if not condition:
        raise Refused()


def at(field, check):
    """Run a zero-argument check and return its value; a refusal inside it is
    labelled with the innermost field that first names it."""
    try:
        return check()
    except Refused as error:
        if error.field is None:
            error.field = field
        raise


def declared_options(help_text):
    """Flag names in a CLI's option table. Only the spec before a column gap
    counts, so flags named inside descriptions never satisfy a requirement."""
    names = set()
    for line in help_text.splitlines():
        match = OPTION_ROW.fullmatch(line.rstrip())
        if match:
            names.update(FLAG.findall(re.split(r"\s{2,}", match.group(1), maxsplit=1)[0]))
    return names


def require_flags(binary, env, cwd, label, needed, help_args=("--help",)):
    """Refuse unless the installed CLI declares every needed flag. Asks the
    binary itself (--version, then its help arguments), never reads its config or
    credentials. Results are cached per executable path and version for the
    run, so a version string alone never decides support. Returns the version."""
    run = {"env": env, "cwd": cwd, "stdin": subprocess.DEVNULL, "stdout": subprocess.PIPE,
           "stderr": subprocess.PIPE, "timeout": 10, "check": False}
    probe = subprocess.run([binary, "--version"], **run)
    words = probe.stdout.decode("utf-8", "replace").split()[:1]
    require(probe.returncode == 0 and bool(words) and SEMVER.fullmatch(words[0]))
    version = words[0]
    key = (str(binary), version)
    if key not in _OPTIONS:
        # A CLI may load ambient code to print help (Pi loads its extensions),
        # so the caller passes the flags that keep the probe itself bounded.
        helped = subprocess.run([binary, *help_args], **run)
        require(helped.returncode == 0)
        _OPTIONS[key] = declared_options(helped.stdout.decode("utf-8", "replace"))
    missing = [flag for flag in needed if flag not in _OPTIONS[key]]
    if missing:
        raise ValueError(f"installed {label} {version} lacks bounded-loading flags: {', '.join(missing)}")
    return version


def keys(value, expected):
    require(isinstance(value, dict) and set(value) == set(expected))


def strings(value, allowed, maximum=32):
    require(isinstance(value, list) and len(value) <= maximum
            and all(isinstance(v, str) and v in allowed for v in value)
            and len(set(value)) == len(value))


def present(path):
    try:
        path.lstat()
        return True
    except FileNotFoundError:
        return False


def safe_path(value, directory=False):
    require(isinstance(value, str) and len(value) <= 4096)
    path = Path(value)
    require(path.is_absolute() and str(path) == value and '..' not in path.parts
            and not any(c in value for c in "\0\n\r"))
    # Reject symlinks at every component, not just at the final file.
    for part in [*reversed(path.parents), path]:
        info = part.lstat()
        require(not stat.S_ISLNK(info.st_mode) and (not info.st_mode & 0o022
                or (stat.S_ISDIR(info.st_mode) and info.st_mode & stat.S_ISVTX
                    and info.st_uid == 0)))
    info = path.stat()
    require((stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode))
            and info.st_uid == os.geteuid())
    return path


def file_bytes(path):
    with path.open('rb') as stream:
        data = stream.read(MAX_BYTES + 1)
    require(len(data) <= MAX_BYTES)
    return data


def tree_digest(root):
    """sha256 of sorted relative-posix-path NUL sha256(file) LF records."""
    root = safe_path(str(root), directory=True)
    digest = hashlib.sha256()
    paths = []
    entries = size = 0
    def walk(directory):
        nonlocal entries
        with os.scandir(directory) as iterator:
            for entry in iterator:
                entries += 1
                require(entries <= MAX_FILES)
                path = Path(entry.path)
                if entry.is_dir(follow_symlinks=False):
                    safe_path(str(path), directory=True)
                    walk(path)
                else:
                    paths.append(safe_path(str(path)))
    walk(root)
    for path in sorted(paths, key=lambda p: p.relative_to(root).as_posix()):
        relative = path.relative_to(root).as_posix()
        require(not any(c in relative for c in '\0\n\r'))
        data = file_bytes(path)
        size += len(data)
        require(size <= MAX_BYTES)
        digest.update(relative.encode() + b'\0' + hashlib.sha256(data).hexdigest().encode() + b'\n')
    return digest.hexdigest()


def extension(value):
    keys(value, ('path', 'sha256'))
    require(isinstance(value['sha256'], str) and HEX.fullmatch(value['sha256']))
    path = safe_path(value['path'])
    require(path.suffix in ('.ts', '.js', '.mjs'))
    require(hashlib.sha256(file_bytes(path)).hexdigest() == value['sha256'])


def skill(value):
    keys(value, ('name', 'root', 'sha256'))
    require(isinstance(value['name'], str) and len(value['name']) <= 64 and NAME.fullmatch(value['name']))
    require(isinstance(value['sha256'], str) and HEX.fullmatch(value['sha256']))
    root = at('skills.root', lambda: safe_path(value['root'], directory=True))
    at('skills.sha256', lambda: require(tree_digest(root) == value['sha256']))
    # Intentionally narrow frontmatter, so Pi cannot quietly skip/rename an
    # approved skill or let first-discovered collisions decide the inventory.
    text = file_bytes(safe_path(str(root / 'SKILL.md'))).decode('utf-8')
    lines = text.splitlines()
    require(lines and lines[0] == '---' and '---' in lines[1:])
    fields = {}
    for line in lines[1:lines[1:].index('---') + 1]:
        key, sep, content = line.partition(':')
        require(sep and key in ('name', 'description') and key not in fields)
        fields[key] = content.strip()
    description = fields.get('description', '')
    if description.startswith('"'):
        description = json.loads(description)
        require(isinstance(description, str) and not any(ord(c) < 32 for c in description))
    else:
        # A deliberately small YAML subset, not a second YAML parser. Reject
        # mappings, arrays, tags, anchors, booleans/numbers and multiline forms
        # which Pi could interpret differently or silently skip.
        require(re.fullmatch(r'[A-Za-z][A-Za-z0-9 .,;()/!?_-]*', description)
                and description.lower() not in ('true', 'false', 'null', 'yes', 'no', 'on', 'off'))
    at('skills.name', lambda: require(fields.get('name') == value['name']))
    at('skills.description', lambda: require(0 < len(description) <= 1024))


def credentials(value):
    """A bundle's credentials: env-safe names and stored refs only. Any other
    field, or a ref too long or shaped unlike a short identifier, refuses."""
    require(isinstance(value, list) and len(value) <= 8)
    names = set()
    for item in value:
        keys(item, ('name', 'ref'))
        require(isinstance(item['name'], str) and CREDENTIAL_NAME.fullmatch(item['name'])
                and isinstance(item['ref'], str) and len(item['ref']) <= 40
                and CREDENTIAL_REF.fullmatch(item['ref']) and item['name'] not in names)
        names.add(item['name'])


def plain(text):
    """Refuse shell/env expansion markers and control characters in any MCP
    string: Pi would expand '$' and run a leading '!' header value."""
    return '$' not in text and not text.startswith('!') and text.isprintable()


HOST = re.compile(r"(?:[A-Za-z0-9.-]+|\[[0-9A-Fa-f:.]+\])(?::[0-9]{1,5})?\Z")
LOOPBACK = ('127.0.0.1', '::1', 'localhost')


def url(value):
    """https, or http only to loopback. No userinfo, fragment, backslash or
    whitespace. The authority is a plain host (letters, digits, dots, hyphens)
    or a bracketed IPv6 literal, with an optional port that must parse."""
    require(isinstance(value, str) and len(value) <= 2048 and plain(value) and '#' not in value)
    # WHATWG parsers read "\" as "/": a reviewed host could differ from the one connected.
    require('\\' not in value and not any(c.isspace() for c in value))
    parts = urlsplit(value)
    require(parts.scheme in ('https', 'http') and HOST.fullmatch(parts.netloc))
    try:
        parts.port
    except ValueError:
        raise Refused() from None
    require(parts.scheme == 'https' or parts.hostname in LOOPBACK)


# A stdio server's executable may not be a shell or env: either runs whatever
# its arguments say, so the pin would cover nothing.
SHELLS = ('sh', 'bash', 'zsh', 'dash', 'fish', 'ksh', 'mksh', 'csh', 'tcsh', 'ash', 'busybox', 'env')
MAX_COMMAND = 32
# Bounds the hash read; a native MCP server is far smaller.
MAX_EXECUTABLE = 512 * 1024 * 1024


def argument(text):
    """One stdio argv element: plain, bounded, and no leading '~', which Pi
    reads as the home directory and Claude would pass through unchanged."""
    require(isinstance(text, str) and 0 < len(text) <= 4096 and plain(text) and not text.startswith('~'))


def executable(value, keep=False):
    """The sha256 of a stdio server's executable: an absolute path, no shell or
    env, owned by root or this user at every component, none a symlink or group-
    or world-writable, and a regular file its owner may execute that is not a
    #! script. With `keep`, also the open close-on-exec descriptor it was
    hashed through, for the launcher to exec those same bytes."""
    argument(value)
    path = Path(value)
    require(path.is_absolute() and str(path) == value and '..' not in path.parts and path.name not in SHELLS)
    for part in [*reversed(path.parents), path]:
        info = part.lstat()
        require(not stat.S_ISLNK(info.st_mode) and not info.st_mode & 0o022
                and info.st_uid in (0, os.geteuid()))
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        with os.fdopen(descriptor, 'rb', closefd=False) as stream:
            info = os.fstat(descriptor)
            # The file hashed is the one checked: same inode as the path's last component.
            require(stat.S_ISREG(info.st_mode) and info.st_mode & 0o100 and info.st_size <= MAX_EXECUTABLE
                    and (info.st_dev, info.st_ino) == (path.lstat().st_dev, path.lstat().st_ino))
            # A #! script runs its interpreter by path, which its pin does not
            # cover, and cannot be exec'd through a close-on-exec descriptor:
            # pin the interpreter and pass the script as an argument instead.
            require(os.pread(descriptor, 2, 0) != b'#!')
            digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    except BaseException:
        os.close(descriptor)
        raise
    if keep:
        return digest, descriptor
    os.close(descriptor)
    return digest


def command(value, keep=False):
    """A stdio server's argv, element 0 its executable: that file's sha256
    (and, with `keep`, its open descriptor; see executable())."""
    require(isinstance(value, list) and 0 < len(value) <= MAX_COMMAND)
    for item in value:
        argument(item)
    try:
        return executable(value[0], keep)
    except OSError:
        # Missing or unreadable: a refusal that carries no path.
        raise Refused() from None


def interpreter():
    """This Python's own executable by absolute path, for running a runtime
    helper: never a PATH lookup, which can find an activation shim (mise)
    first. Linux names it in /proc; elsewhere sys.executable resolved."""
    proc = Path('/proc/self/exe')
    path = Path(os.readlink(proc)) if proc.exists() else Path(sys.executable).resolve()
    require(path.is_absolute() and path.name.startswith('python') and plain(str(path)))
    return str(path)


# The runtime's stdio launcher, run by interpreter() for every stdio server.
STDIO_LAUNCHER = Path(__file__).resolve().with_name('mcp_stdio.py')


def stdio_argv(server, credential_file=None):
    """The argv a harness runs for one stdio server: the launcher and its
    SPEC, which carries the pinned command and its credential's path only."""
    value = {'command': server['command'], 'sha256': server['sha256']}
    if 'credential' in server:
        require(isinstance(credential_file, str) and plain(credential_file))
        value['credential'] = {'name': server['credential'], 'file': credential_file}
    return [interpreter(), '-I', str(STDIO_LAUNCHER), json.dumps(value, separators=(',', ':'), sort_keys=True)]


def mcp_server(value, names):
    """One selected MCP server. Either remote streamable HTTP (`url`), whose
    Bearer token is the named credential, or a local stdio process (`command`),
    an argv whose executable is pinned by `sha256`, given at most its named
    credential's file path. Never both; no headers, env, cwd or shell keys."""
    stdio = isinstance(value, dict) and 'command' in value
    allowed = {'name', 'command', 'sha256', 'credential', 'timeout', 'description'} if stdio else \
        {'name', 'url', 'credential', 'timeout', 'description'}
    needed = {'name', 'command', 'sha256', 'timeout'} if stdio else {'name', 'url', 'credential', 'timeout'}
    at('mcp_servers', lambda: require(isinstance(value, dict) and set(value) <= allowed and needed <= set(value)))
    at('mcp_servers.name', lambda: require(isinstance(value['name'], str) and len(value['name']) <= 32
            and NAME.fullmatch(value['name']) and plain(value['name'])))
    if stdio:
        at('mcp_servers.sha256', lambda: require(isinstance(value['sha256'], str) and HEX.fullmatch(value['sha256'])))
        digest = at('mcp_servers.command', lambda: command(value['command']))
        # Checked at selection and again at each launch's preflight.
        at('mcp_servers.sha256', lambda: require(digest == value['sha256']))
    else:
        at('mcp_servers.url', lambda: url(value['url']))
    if 'credential' in value:
        at('mcp_servers.credential', lambda: require(isinstance(value['credential'], str)
                and value['credential'] in names))
    at('mcp_servers.timeout', lambda: require(type(value['timeout']) is int and 1 <= value['timeout'] <= 300))
    if 'description' in value:
        description = value['description']
        at('mcp_servers.description', lambda: require(isinstance(description, str)
                and len(description) <= 200 and plain(description)))


def mcp_servers(value, names, maximum):
    at('mcp_servers', lambda: require(isinstance(value, list) and 0 < len(value) <= maximum))
    for item in value:
        mcp_server(item, names)


def server_keys(servers):
    """Per plan, at most 8 servers, unique across bundles after lower() and '-' to '_'."""
    at('mcp_servers', lambda: require(len(servers) <= 8))
    keys = [server['name'].lower().replace('-', '_') for server in servers]
    at('mcp_servers.name', lambda: require(len(set(keys)) == len(keys)))


class StdioRole(Refused):
    """A stdio server selected for a launch that is not a crew job. A crew
    job's teardown ends its whole session, the server included. A hosted
    first mate's pane is not a job: sleep, approval refresh and upgrade signal
    its harness pid alone, so a server there could outlive it. A wake plans
    as first_mate too, and is refused with it. The role named is one of a
    fixed set, never an input value."""

    def __init__(self, role):
        super().__init__('mcp_servers.command')
        self.role = role

    @property
    def public_message(self):
        return (f"stdio MCP servers run only in crew jobs, whose teardown ends them; "
                f"{self.role} launches are not jobs")


def stdio_role(servers, role):
    """Refuse any stdio server unless `role` is crew; HTTP servers pass."""
    if role != 'crew' and any(isinstance(server, dict) and 'command' in server for server in servers):
        raise StdioRole(role if role in ('first_mate', 'reviewer') else 'non-crew')


def credential_directory(env):
    """Beside the fleet registry, resolved as the runtime resolves it:
    PRIVATEER_FLEET, else $XDG_STATE_HOME/privateer/fleet.sqlite, else
    ~/.local/state/privateer/fleet.sqlite, with .credentials for its suffix."""
    fleet = env.get('PRIVATEER_FLEET')
    if not fleet:
        state = env.get('XDG_STATE_HOME') or str(Path(env['HOME']) / '.local/state')
        fleet = str(Path(state) / 'privateer/fleet.sqlite')
    return Path(fleet).with_suffix('.credentials')


def credential_environment(value, env):
    """PRIVATEER_CREDENTIAL_<NAME>_FILE for each of the plan's own credentials,
    after checking its stored file: a regular 0600 file this user owns, in a
    0700 directory, with bundle-path checks on every component. Never reads it."""
    found = {}
    require(isinstance(value, dict))
    credentials(value.get('credentials', []))
    if not value.get('credentials'):
        return found
    directory = credential_directory(env)
    require(present(directory))
    safe_path(str(directory), directory=True)
    require(stat.S_IMODE(directory.lstat().st_mode) == 0o700)
    for item in value['credentials']:
        require(present(directory / item['ref']))
        path = safe_path(str(directory / item['ref']))
        info = path.lstat()
        require(stat.S_IMODE(info.st_mode) == 0o600 and info.st_nlink == 1)
        found[f"{CREDENTIAL_ENV}{item['name']}_FILE"] = str(path)
    return found


def bundle_name(index, bundle):
    """The ref when it is well formed (a plain name@version), else its position."""
    ref = bundle.get('ref') if isinstance(bundle, dict) else None
    if isinstance(ref, str) and REF.fullmatch(ref):
        return ref
    return f'at index {index}'


def bundle(value, refs):
    # Optional executable extensions are not silently discarded: not
    # supported in this stage. MCP servers are remote HTTP or pinned stdio.
    at('fields', lambda: keys(value, ('ref', 'roles', *(('skills',) if 'skills' in value else ()),
                                      *(('credentials',) if 'credentials' in value else ()),
                                      *(('mcp_servers',) if 'mcp_servers' in value else ()))))
    at('ref', lambda: require(isinstance(value['ref'], str) and REF.fullmatch(value['ref'])
                              and value['ref'] not in refs))
    refs.add(value['ref'])
    at('roles', lambda: strings(value['roles'], ('first_mate', 'crew'), 2))
    at('roles', lambda: require(value['roles']))
    # An absent skills key reads as none: a bundle may carry only MCP servers.
    skills = value.get('skills', [])
    at('skills', lambda: require(isinstance(skills, list) and len(skills) <= 8))
    names = set()
    for item in skills:
        at('skills', lambda: skill(item))
        at('skills.name', lambda: require(item['name'] not in names))
        names.add(item['name'])
    at('credentials', lambda: credentials(value.get('credentials', [])))
    if 'mcp_servers' in value:
        mcp_servers(value['mcp_servers'], {item['name'] for item in value.get('credentials', [])}, 4)
    # A bundle needs at least one skill or one server.
    at('skills or mcp_servers', lambda: require(skills or value.get('mcp_servers')))


def validate_registry(value):
    # claude_version is optional: only bounded Claude launches need the pin.
    at('registry', lambda: require(isinstance(value, dict)))
    at('registry', lambda: keys(value, ('schema_version', 'pi_version', 'integrations', 'bundles',
                                        *(('claude_version',) if 'claude_version' in value else ()))))
    # Versions are records of what the registry was made with, not pins: the
    # installed CLI must show the bounded-loading flags at launch instead.
    require(type(value['schema_version']) is int and value['schema_version'] == 1
            and isinstance(value['pi_version'], str) and SEMVER.fullmatch(value['pi_version'])
            and isinstance(value.get('claude_version', '0.0.0'), str)
            and SEMVER.fullmatch(value.get('claude_version', '0.0.0')))
    integration = value['integrations']
    # Explicit reviewed:true + extensions:[] means the operator asserts no
    # extension integrations are required. Omission is NOT that approval.
    keys(integration, ('reviewed', 'extensions'))
    require(integration['reviewed'] is True and isinstance(integration['extensions'], list)
            and len(integration['extensions']) <= 8)
    paths = set()
    for item in integration['extensions']:
        extension(item)
        require(item['path'] not in paths)
        paths.add(item['path'])
    bundles = value['bundles']
    require(isinstance(bundles, list) and len(bundles) <= 32)
    refs = set()
    for index, item in enumerate(bundles):
        try:
            bundle(item, refs)
        except Refused as error:
            raise Refused(error.field, bundle_name(index, item)) from None
    require(len(json.dumps(value)) <= 48 * 1024)


def selection(value):
    require(isinstance(value, list) and len(value) <= 16
            and all(isinstance(v, str) and REF.fullmatch(v) for v in value)
            and len(value) == len(set(value)))


def plan(registry, selected, role, harness='pi'):
    validate_registry(registry)
    selection(selected)
    require(role in ('first_mate', 'crew'))
    by_ref = {b['ref']: b for b in registry['bundles']}
    skills = []
    secrets = []
    servers = []
    for reference in selected:
        require(reference in by_ref and role in by_ref[reference]['roles'])
        skills.extend(by_ref[reference].get('skills', []))
        servers.extend(by_ref[reference].get('mcp_servers', []))
        for item in by_ref[reference].get('credentials', []):
            if item not in secrets:
                secrets.append(item)
    require(len({s['name'] for s in skills}) == len(skills) and len(skills) <= 16)
    # One name, one ref: two selected bundles cannot disagree on a variable.
    credentials(secrets)
    server_keys(servers)
    # Plans are where every launch's servers come from: no non-crew plan carries one.
    stdio_role(servers, role)
    if harness == 'claude':
        from claude_bundles import plan as claude_plan
        result = claude_plan(registry, selected, skills)
    else:
        require(harness == 'pi')
        result = {'schema_version': 1, 'pi_version': registry['pi_version'], 'selection': selected,
                  'integrations': registry['integrations'], 'skills': skills}
    # Only selected bundles' names and refs, and only when there are any, so
    # plans (and digests) without credentials or servers are unchanged.
    if secrets:
        result['credentials'] = secrets
    if servers:
        result['mcp_servers'] = servers
    require(len(json.dumps(result)) <= 24 * 1024)
    return result


def validate_plan(value):
    # A Claude plan pins its version and carried settings instead of Pi's
    # version and extension integrations.
    claude = isinstance(value, dict) and 'claude_version' in value
    keys(value, ('schema_version', 'selection', 'skills',
                 *(('claude_version', 'settings') if claude else ('pi_version', 'integrations')),
                 *(('credentials',) if 'credentials' in value else ()),
                 *(('mcp_servers',) if 'mcp_servers' in value else ())))
    selection(value['selection'])
    credentials(value.get('credentials', []))
    if 'mcp_servers' in value:
        # Only a non-empty list: plan() omits the key when no server is selected.
        mcp_servers(value['mcp_servers'], {item['name'] for item in value.get('credentials', [])}, 8)
        server_keys(value['mcp_servers'])
    # Reuse registry qualification without carrying unselected definitions.
    skills = value['skills']
    require(isinstance(skills, list) and len(skills) <= 16)
    bundles = [{'ref': 'selected@1', 'roles': ['crew'], 'skills': skills[i:i+8]}
               for i in range(0, len(skills), 8)]
    for index, item in enumerate(bundles):
        item['ref'] = f'selected@{index}'
    registry = {'schema_version': value['schema_version'], 'pi_version': value.get('pi_version', NO_PI_VERSION),
                'integrations': value.get('integrations', {'reviewed': True, 'extensions': []}), 'bundles': bundles}
    if claude:
        registry['claude_version'] = value['claude_version']
        require(isinstance(value['settings'], dict))
    validate_registry(registry)
    require(len({s['name'] for s in skills}) == len(skills))


def handle(request):
    if request.get('selection') is None:
        validate_registry(request['registry'])
        return {'valid': True}
    return {'valid': True, 'plan': plan(request['registry'], request['selection'], request['role'],
                                        request.get('harness', 'pi'))}


if __name__ == '__main__':
    import sys
    if len(sys.argv) == 3 and sys.argv[1] == '--digest':
        try:
            print(tree_digest(sys.argv[2]))
        except Exception:
            raise SystemExit('Cannot qualify bundle content; inspect local paths and permissions.')
    else:
        serve(handle)
