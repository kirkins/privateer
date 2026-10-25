"""Bounded trusted-adapter staging. Paths stay on the core relay, never MCP."""
import sys
sys.dont_write_bytecode = True
import base64
import binascii
from contextlib import contextmanager
import os
import re
import stat
import uuid

UPLOAD_BYTES = 8 * 1024 * 1024
BASE64_CHARS = 4 * ((UPLOAD_BYTES + 2) // 3)
UPLOAD_ENVELOPE = BASE64_CHARS + 65536
STAGE_PREFIX = ".mcp-upload-"


def decode(content):
    if len(content) > BASE64_CHARS:
        raise ValueError("content")
    try:
        data = base64.b64decode(content, validate=True)
    except (ValueError, binascii.Error):
        raise ValueError("content") from None
    if len(data) > UPLOAD_BYTES or base64.b64encode(data).decode("ascii") != content:
        raise ValueError("content")
    return data


def valid_name(name):
    return (0 < len(name.encode("utf-8")) <= 128 and not name.startswith(".")
            and not any(c in "/\\" or ord(c) < 32 or 127 <= ord(c) <= 159 for c in name))


class Staging:
    def __init__(self, state):
        self.state = os.path.abspath(state)
        self.fd = os.open(self.state, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        self.active = {}
        self.total = 0
        # Only our reserved random leaf namespace; unlink never follows links.
        for name in os.listdir(self.fd):
            if re.fullmatch(r"\.mcp-upload-[0-9a-f]{32}", name):
                try:
                    mode = os.stat(name, dir_fd=self.fd, follow_symlinks=False).st_mode
                    if stat.S_ISREG(mode) or stat.S_ISLNK(mode):
                        os.unlink(name, dir_fd=self.fd)
                except (FileNotFoundError, IsADirectoryError):
                    # Another cleanup may win, or a leaf may become a directory
                    # after lstat. Never recurse into or remove directories.
                    pass

    def close(self):
        os.close(self.fd)

    @contextmanager
    def stage(self, grant, data):
        count, used = self.active.get(grant, (0, 0))
        if count >= 2 or used + len(data) > 2 * UPLOAD_BYTES or self.total + len(data) > 8 * UPLOAD_BYTES:
            raise OSError("staging capacity")
        self.active[grant] = (count + 1, used + len(data))
        self.total += len(data)
        name = STAGE_PREFIX + uuid.uuid4().hex
        created = False
        try:
            fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                         0o600, dir_fd=self.fd)
            created = True
            with os.fdopen(fd, "wb") as file:
                info = os.fstat(file.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                    raise OSError("invalid stage")
                file.write(data)
            yield os.path.join(self.state, name)
        finally:
            try:
                if created:
                    os.unlink(name, dir_fd=self.fd)
            finally:
                count, used = self.active[grant]
                if count == 1:
                    del self.active[grant]
                else:
                    self.active[grant] = (count - 1, used - len(data))
                self.total -= len(data)
