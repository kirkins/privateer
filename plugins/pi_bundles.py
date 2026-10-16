#!/usr/bin/env python3
"""Pi 1.2 resource exposure hygiene, not an OS/credential sandbox.

Only an operator-owned registry may approve resources. Plans contain selected
references and content pins, never credential values or unselected resources.
No installs, config writes, agentDir/HOME changes or provider selection here.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess

from protocol import serve

VERSION = "1.2.0"
FLAGS = ["--no-extensions", "--no-mcp", "--no-skills", "--no-prompt-templates",
         "--no-context-files", "--no-themes"]
NAME = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*\Z")
REF = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*@[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}\Z")
HEX = re.compile(r"[0-9a-f]{64}\Z")
MAX_FILES = 128
MAX_BYTES = 2 * 1024 * 1024


def require(condition):
    if not condition:
        raise ValueError("unsupported bounded Pi resources")


def keys(value, expected):
    require(isinstance(value, dict) and set(value) == set(expected))


def strings(value, allowed, maximum=32):
    require(isinstance(value, list) and len(value) <= maximum
            and all(isinstance(v, str) and v in allowed for v in value)
            and len(set(value)) == len(value))


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
    root = safe_path(value['root'], directory=True)
    require(tree_digest(root) == value['sha256'])
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
    require(fields.get('name') == value['name'] and 0 < len(description) <= 1024)


def validate_registry(value):
    keys(value, ('schema_version', 'pi_version', 'integrations', 'bundles'))
    require(type(value['schema_version']) is int and value['schema_version'] == 1 and value['pi_version'] == VERSION)
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
    for bundle in bundles:
        # MCP, optional executable extensions and credential injection are not
        # silently discarded: not supported in this first stage.
        keys(bundle, ('ref', 'roles', 'skills'))
        require(isinstance(bundle['ref'], str) and REF.fullmatch(bundle['ref']) and bundle['ref'] not in refs)
        refs.add(bundle['ref'])
        strings(bundle['roles'], ('first_mate', 'crew'), 2)
        require(bundle['roles'] and isinstance(bundle['skills'], list) and 0 < len(bundle['skills']) <= 8)
        names = set()
        for item in bundle['skills']:
            skill(item)
            require(item['name'] not in names)
            names.add(item['name'])
    require(len(json.dumps(value)) <= 48 * 1024)


def selection(value):
    require(isinstance(value, list) and len(value) <= 16
            and all(isinstance(v, str) and REF.fullmatch(v) for v in value)
            and len(value) == len(set(value)))


def plan(registry, selected, role):
    validate_registry(registry)
    selection(selected)
    require(role in ('first_mate', 'crew'))
    by_ref = {b['ref']: b for b in registry['bundles']}
    skills = []
    for reference in selected:
        require(reference in by_ref and role in by_ref[reference]['roles'])
        skills.extend(by_ref[reference]['skills'])
    require(len({s['name'] for s in skills}) == len(skills) and len(skills) <= 16)
    result = {'schema_version': 1, 'pi_version': VERSION, 'selection': selected,
              'integrations': registry['integrations'], 'skills': skills}
    require(len(json.dumps(result)) <= 24 * 1024)
    return result


def validate_plan(value):
    keys(value, ('schema_version', 'pi_version', 'selection', 'integrations', 'skills'))
    selection(value['selection'])
    # Reuse registry qualification without carrying unselected definitions.
    skills = value['skills']
    require(isinstance(skills, list) and len(skills) <= 16)
    bundles = [{'ref': 'selected@1', 'roles': ['crew'], 'skills': skills[i:i+8]}
               for i in range(0, len(skills), 8)]
    for index, item in enumerate(bundles):
        item['ref'] = f'selected@{index}'
    validate_registry({'schema_version': value['schema_version'], 'pi_version': value['pi_version'],
                       'integrations': value['integrations'], 'bundles': bundles})
    require(len({s['name'] for s in skills}) == len(skills))


def preflight(value, argv, cwd=None, env=None, runtime_extensions=()):
    """Recheck pins immediately before native execution. No native probe until
    package/settings checks pass. Auth/models files are never read here.
    """
    validate_plan(value)
    env = os.environ if env is None else env
    cwd = Path(cwd or os.getcwd())
    agent = Path(env.get('PI_CODING_AGENT_DIR', str(Path.home() / '.pi' / 'agent')))
    require(agent.is_absolute())
    def present(path):
        try:
            path.lstat()
            return True
        except FileNotFoundError:
            return False

    for directory in (agent, cwd / '.pi'):
        settings = directory / 'settings.json'
        if present(settings):
            # Read only settings, not auth/models/credential stores. Bad JSON
            # or configured packages refuse, never fetch/install as a fallback.
            config = json.loads(file_bytes(safe_path(str(settings))))
            require(isinstance(config, dict) and ('packages' not in config or config['packages'] == []))
        require(not present(directory / 'SYSTEM.md') and not present(directory / 'APPEND_SYSTEM.md'))
    require(argv and argv[0] == 'pi')
    # Call the same binary execution will use, not a mise activation wrapper
    # with state-changing `mise use` behavior.
    import shutil
    binary = shutil.which(argv[0], path=env.get('PATH'))
    require(binary is not None)
    binary = str(Path(binary).resolve(strict=True))
    require(Path(binary).name == 'pi' and Path(binary).is_file())
    # Reject common activation wrappers before running --version.
    with open(binary, 'rb') as stream:
        require(stream.read(2) != b'#!')
    args = argv[1:]
    flags = args[:args.index('--')] if '--' in args else args
    # Native session history could carry old skill/tool facts; qualification of
    # bounded resume/fork is deferred rather than pretending a fresh load erases it.
    forbidden = ('--continue', '-c', '--resume', '-r', '--session', '--session-id', '--fork',
                 '--extension', '-e', '--skill', '--prompt-template', '--theme', '--system-prompt',
                 '--append-system-prompt', '--approve', '-a')
    runtime_paths = [str(path) for path in runtime_extensions]
    require(len(runtime_paths) <= 3 and len(set(runtime_paths)) == len(runtime_paths))
    seen_runtime = set()
    clean = []
    i = 0
    while i < len(flags):
        arg = flags[i]
        # Privateer owns these explicit mail/crew-lifecycle adapters, not
        # ambient or registry-selected integrations. Permit only exact paths
        # chosen by the native launcher for this effective execution mode.
        if arg == '-e' and i + 1 < len(flags) and flags[i+1] in runtime_paths:
            require(flags[i+1] not in seen_runtime)
            seen_runtime.add(flags[i+1])
            i += 2
            continue
        require(not any(arg == f or arg.startswith(f + '=') for f in forbidden))
        # Reject combined/attached short options rather than allow -ePATH/-c.
        require(not arg.startswith(('-e', '-c', '-r', '-a')))
        clean.append(arg)
        i += 1
    probe = subprocess.run([binary, '--version'], env=env, cwd=cwd,
                           stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           timeout=5, check=False)
    require(probe.returncode == 0 and probe.stdout.strip() == VERSION.encode())
    loaded = [argv[0], *FLAGS]
    for item in value['integrations']['extensions']:
        loaded.extend(['-e', item['path']])
    for path in runtime_paths:
        loaded.extend(['-e', path])
    for item in value['skills']:
        loaded.extend(['--skill', str(Path(item['root']) / 'SKILL.md')])
    return [*loaded, *clean, *args[len(flags):]]


def handle(request):
    if request.get('selection') is None:
        validate_registry(request['registry'])
        return {'valid': True}
    return {'valid': True, 'plan': plan(request['registry'], request['selection'], request['role'])}


if __name__ == '__main__':
    import sys
    if len(sys.argv) == 3 and sys.argv[1] == '--digest':
        try:
            print(tree_digest(sys.argv[2]))
        except Exception:
            raise SystemExit('Cannot qualify bundle content; inspect local paths and permissions.')
    else:
        serve(handle)
