#!/bin/sh
# Install only into an explicit, plugin-private venv. Never system/user Python.
set -eu
if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo 'usage: sh bootstrap.sh VENV [PYTHON_EXECUTABLE]' >&2
    exit 2
fi
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
python=${2:-$(command -v python3)}
"$python" -c 'import sys; assert (3,12) <= sys.version_info < (3,15), "requires Python 3.12–3.14"'
umask 077
uv venv --python "$python" "$1"
uv pip sync --python "$1/bin/python" --require-hashes "$here/requirements.lock"
"$1/bin/python" -c 'import importlib.metadata, sys; print(sys.version); print("mcp", importlib.metadata.version("mcp"))'
