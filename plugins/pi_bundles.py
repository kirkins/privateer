"""Pi 1.2 bounded loader: supported flags plus the checks Pi 1.2.0 needs.
Registry, selection and pins live in bundles.py.
"""
import json
import os
from pathlib import Path
import subprocess

from bundles import PI_VERSION as VERSION, file_bytes, present, require, safe_path, validate_plan

FLAGS = ["--no-extensions", "--no-mcp", "--no-skills", "--no-prompt-templates",
         "--no-context-files", "--no-themes"]


def preflight(value, argv, cwd=None, env=None, runtime_extensions=()):
    """Recheck pins immediately before native execution. No native probe until
    package/settings checks pass. Auth/models files are never read here.
    """
    validate_plan(value)
    require('pi_version' in value)
    env = os.environ if env is None else env
    cwd = Path(cwd or os.getcwd())
    agent = Path(env.get('PI_CODING_AGENT_DIR', str(Path.home() / '.pi' / 'agent')))
    require(agent.is_absolute())
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
