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

from protocol import serve

PI_VERSION = "1.2.0"
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


def require(condition):
    if not condition:
        raise ValueError("unsupported bounded resources")


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


def validate_registry(value):
    # claude_version is optional: only bounded Claude launches need the pin.
    require(isinstance(value, dict))
    keys(value, ('schema_version', 'pi_version', 'integrations', 'bundles',
                 *(('claude_version',) if 'claude_version' in value else ())))
    require(type(value['schema_version']) is int and value['schema_version'] == 1 and value['pi_version'] == PI_VERSION
            and isinstance(value.get('claude_version', '0.0.0'), str) and SEMVER.fullmatch(value.get('claude_version', '0.0.0')))
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
        # MCP and optional executable extensions are not silently discarded:
        # not supported in this first stage.
        keys(bundle, ('ref', 'roles', 'skills', *(('credentials',) if 'credentials' in bundle else ())))
        require(isinstance(bundle['ref'], str) and REF.fullmatch(bundle['ref']) and bundle['ref'] not in refs)
        refs.add(bundle['ref'])
        strings(bundle['roles'], ('first_mate', 'crew'), 2)
        require(bundle['roles'] and isinstance(bundle['skills'], list) and 0 < len(bundle['skills']) <= 8)
        names = set()
        for item in bundle['skills']:
            skill(item)
            require(item['name'] not in names)
            names.add(item['name'])
        credentials(bundle.get('credentials', []))
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
    for reference in selected:
        require(reference in by_ref and role in by_ref[reference]['roles'])
        skills.extend(by_ref[reference]['skills'])
        for item in by_ref[reference].get('credentials', []):
            if item not in secrets:
                secrets.append(item)
    require(len({s['name'] for s in skills}) == len(skills) and len(skills) <= 16)
    # One name, one ref: two selected bundles cannot disagree on a variable.
    credentials(secrets)
    if harness == 'claude':
        from claude_bundles import plan as claude_plan
        result = claude_plan(registry, selected, skills)
    else:
        require(harness == 'pi')
        result = {'schema_version': 1, 'pi_version': PI_VERSION, 'selection': selected,
                  'integrations': registry['integrations'], 'skills': skills}
    # Only selected bundles' names and refs, and only when there are any, so
    # plans (and digests) without credentials are unchanged.
    if secrets:
        result['credentials'] = secrets
    require(len(json.dumps(result)) <= 24 * 1024)
    return result


def validate_plan(value):
    # A Claude plan pins its version and carried settings instead of Pi's
    # version and extension integrations.
    claude = isinstance(value, dict) and 'claude_version' in value
    keys(value, ('schema_version', 'selection', 'skills',
                 *(('claude_version', 'settings') if claude else ('pi_version', 'integrations')),
                 *(('credentials',) if 'credentials' in value else ())))
    selection(value['selection'])
    credentials(value.get('credentials', []))
    # Reuse registry qualification without carrying unselected definitions.
    skills = value['skills']
    require(isinstance(skills, list) and len(skills) <= 16)
    bundles = [{'ref': 'selected@1', 'roles': ['crew'], 'skills': skills[i:i+8]}
               for i in range(0, len(skills), 8)]
    for index, item in enumerate(bundles):
        item['ref'] = f'selected@{index}'
    registry = {'schema_version': value['schema_version'], 'pi_version': value.get('pi_version', PI_VERSION),
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
