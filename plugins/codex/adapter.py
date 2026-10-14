#!/usr/bin/env python3
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from protocol import serve
from native import handle as native_handle


def handle(request):
    return native_handle("codex", request)


if __name__ == "__main__":
    serve(handle)
