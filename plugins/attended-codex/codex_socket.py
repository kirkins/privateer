"""Read-only experimental Codex daemon client (Linux, WebSocket over Unix)."""
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import stat
import struct
import time

LIMIT = 1 << 20


def safe(path, kind=stat.S_ISREG, private=False):
    info = Path(path).lstat()
    if (not kind(info.st_mode) or info.st_uid != os.getuid()
            or info.st_mode & (0o077 if private else 0o022)):
        raise ValueError("unsafe Codex binding path")
    return info


def load(path):
    safe(path)
    with open(path, "rb") as stream:
        data = stream.read(LIMIT + 1)
    if len(data) > LIMIT:
        raise ValueError("Codex binding record exceeds limit")
    return json.loads(data)


def process_identity(pid):
    # Start ticks AND boot id fence PID reuse, without uptime assumptions.
    fields = Path(f"/proc/{int(pid)}/stat").read_text().rsplit(") ", 1)[1].split()
    if fields[0] == "Z":
        raise ValueError("Codex binding process has exited")
    return {"bootId": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
            "startTicks": int(fields[19])}


def socket_paths(home):
    rendezvous = Path(home) / "app-server-control/app-server-control.sock"
    safe(rendezvous.parent, stat.S_ISDIR)
    canonical = rendezvous.parent.resolve() / rendezvous.name
    target = Path("/tmp").resolve() / f"codex-daemon-{os.getuid()}" / hashlib.sha256(os.fsencode(canonical)).hexdigest()
    return rendezvous, target


class Daemon:
    """No auto-start, embedded server, mutation, credential or network fallback."""
    METHODS = {"initialize", "hooks/list", "thread/read", "thread/loaded/list", "thread/queue/list"}

    def __init__(self, home, timeout=3):
        self.home = str(Path(home).resolve())
        self.deadline = time.monotonic() + timeout
        self.sequence = 0
        self.sock = socket.socket(socket.AF_UNIX)
        try:
            self.record = load(Path(home) / "app-server-daemon/daemon.pid")
            self.pid = self.record["pid"]
            self.identity = self.record["processIdentity"]
            self.rendezvous, self.target = socket_paths(home)
            self.socket_identity = None
            self.check()
            self.sock.settimeout(timeout)
            self.sock.connect(str(self.rendezvous))
            peer = struct.unpack("3i", self.sock.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            if peer[:2] != (self.pid, os.getuid()):
                raise ValueError("Codex daemon peer identity mismatch")
            self.check()
            key = base64.b64encode(os.urandom(16)).decode()
            self.sock.sendall(("GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
                               "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                               f"Sec-WebSocket-Key: {key}\r\n\r\n").encode())
            header = b""
            while not header.endswith(b"\r\n\r\n") and len(header) < 8192:
                header += self.read(1)
            expected = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
            headers = {key.lower(): value.strip() for key, value in
                       (line.split(b":", 1) for line in header.split(b"\r\n")[1:] if b":" in line)}
            if (not header.startswith(b"HTTP/1.1 101 ")
                    or headers.get(b"sec-websocket-accept") != expected):
                raise ValueError("Codex daemon WebSocket handshake refused")
            result = self.call("initialize", {"clientInfo": {"name": "privateer_attended", "version": "1"},
                                              "capabilities": {"experimentalApi": True}})
            if result.get("codexHome") != self.home:
                raise ValueError("Codex daemon native home mismatch")
            self.send({"method": "initialized"})
        except BaseException:
            self.close()
            raise

    def close(self):
        self.sock.close()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def check(self):
        safe(self.rendezvous.parent, stat.S_ISDIR)
        safe(self.target.parent, stat.S_ISDIR, private=True)
        info = safe(self.target, stat.S_ISSOCK, private=True)
        identity = (info.st_dev, info.st_ino)
        if self.socket_identity is not None and self.socket_identity != identity:
            raise ValueError("Codex daemon socket was replaced")
        self.socket_identity = identity
        if (not self.rendezvous.is_symlink() or self.rendezvous.lstat().st_uid != os.getuid()
                or self.rendezvous.readlink() != self.target
                or process_identity(self.pid) != self.identity
                or load(Path(self.home) / "app-server-daemon/daemon.pid") != self.record):
            raise ValueError("Codex daemon binding changed")

    def read(self, count):
        data = bytearray()
        while len(data) < count:
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("Codex daemon observation timed out")
            self.sock.settimeout(remaining)
            chunk = self.sock.recv(count - len(data))
            if not chunk:
                raise ValueError("Codex daemon disconnected")
            data.extend(chunk)
        return bytes(data)

    def frame(self, payload, opcode=1):
        if len(payload) > LIMIT:
            raise ValueError("Codex daemon request exceeds limit")
        length = len(payload)
        header = bytes([0x80 | opcode, 0x80 | length]) if length < 126 else bytes([0x80 | opcode, 0xfe]) + struct.pack("!H", length) if length <= 65535 else bytes([0x80 | opcode, 0xff]) + struct.pack("!Q", length)
        mask = os.urandom(4)
        self.sock.sendall(header + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def send(self, message):
        self.frame(json.dumps(message).encode())

    def receive(self):
        for _ in range(128):
            header = self.read(2)
            size = header[1] & 127
            if size == 126:
                size = struct.unpack("!H", self.read(2))[0]
            elif size == 127:
                size = struct.unpack("!Q", self.read(8))[0]
            if size > LIMIT or header[1] & 128 or not header[0] & 128:
                raise ValueError("unsupported Codex daemon frame")
            payload = self.read(size)
            opcode = header[0] & 15
            if opcode == 9 and size <= 125:
                self.frame(payload, 10)
            elif opcode == 1:
                return json.loads(payload)
            else:
                raise ValueError("unsupported Codex daemon frame")
        raise ValueError("Codex daemon frame budget exhausted")

    def call(self, method, params):
        if method not in self.METHODS:
            raise ValueError("Codex binding client is read-only")
        self.check()
        self.sequence += 1
        self.send({"id": self.sequence, "method": method, "params": params})
        for _ in range(128):
            message = self.receive()
            if message.get("id") == self.sequence:
                self.check()
                if "error" in message or not isinstance(message.get("result"), dict):
                    raise ValueError("Codex experimental daemon capability refused")
                return message["result"]
        raise ValueError("Codex daemon notification budget exhausted")
