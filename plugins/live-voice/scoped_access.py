"""Private per-device enrollment. Pairing proves code custody, never core authority."""

import hashlib
import hmac
import json
import os
import re
import secrets
import stat
import sys
import threading
import time
import uuid
from dataclasses import dataclass, field
from collections import deque

sys.dont_write_bytecode = True  # releases must remain byte-for-byte immutable
import web
import voice

MARKER = "scoped-bindings"
STORE = "scoped-devices"
DAY_MS = 24 * 60 * 60 * 1000
CODE_MS = 10 * 60 * 1000


def digest(value):
    return hashlib.sha256(value.encode()).hexdigest()


def denied():
    return web.ApiError(403, "access_denied", "Access denied")


def private_dir(path):
    """Open without following *any* path component; check the custody directory."""
    if not os.path.isabs(path):
        raise ValueError("private path must be absolute")
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in path.split("/")[1:]:
            if part in ("", ".", ".."):
                raise ValueError("invalid private path")
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise ValueError("private directory requires owner-only custody")
        return fd
    except BaseException:
        os.close(fd)
        raise


def private_read(path, limit):
    parent, name = os.path.split(path)
    directory = private_dir(parent)
    try:
        fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
        with os.fdopen(fd, "rb") as stream:
            info = os.fstat(stream.fileno())
            if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1
                    or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600
                    or info.st_size > limit):
                raise ValueError("invalid private file custody")
            raw = stream.read(limit + 1)
            if len(raw) > limit:
                raise ValueError("private file too large")
            return raw.decode("utf-8")
    finally:
        os.close(directory)


def private_write(path, text):
    parent, name = os.path.split(path)
    directory = private_dir(parent)
    temporary = ".stage-" + secrets.token_hex(16)
    try:
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                     0o600, dir_fd=directory)
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            stream.write(text)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, name, src_dir_fd=directory, dst_dir_fd=directory)
        os.fsync(directory)
    finally:
        try:
            os.unlink(temporary, dir_fd=directory)
        except FileNotFoundError:
            pass
        os.close(directory)


def enrollments(cfg):
    rows = cfg.get("devices")
    if not isinstance(rows, list) or not 1 <= len(rows) <= 16:
        raise ValueError("scoped mode requires 1 to 16 devices")
    seen = {key: set() for key in ("id", "label", "grant", "credential_file")}
    for row in rows:
        if not isinstance(row, dict) or set(row) != set(seen):
            raise ValueError("each device needs id, label, grant and credential_file")
        for key, value in row.items():
            if not isinstance(value, str) or not value or value in seen[key]:
                raise ValueError("duplicate or invalid enrollment")
            seen[key].add(value)
        for key in ("id", "grant"):
            if str(uuid.UUID(row[key])) != row[key]:
                raise ValueError("enrollment identifiers must be canonical UUIDs")
        if not re.fullmatch(r"[a-z][a-z0-9-]{0,31}", row["label"]):
            raise ValueError("invalid device label")
        if not os.path.isabs(row["credential_file"]):
            raise ValueError("credential_file must be absolute")
    return rows


@dataclass(frozen=True)
class Binding:
    device: str
    grant: str
    credential_hash: str
    generation: str
    bearer: str = field(repr=False)


class Enrollments:
    def __init__(self, state, cfg, check, clock=lambda: int(time.time() * 1000)):
        self.clock, self.check_core = clock, check
        self.lock = threading.RLock()
        self.rows = {row["id"]: row for row in enrollments(cfg)}
        self.state = state
        fd = private_dir(state)
        os.close(fd)
        private_write(os.path.join(state, MARKER), "protocol 2\n")
        root = os.path.join(state, STORE)
        os.makedirs(root, mode=0o700, exist_ok=True)
        fd = private_dir(root)
        os.close(fd)
        self.bindings, self.tokens, self.codes, self.failures = {}, {}, {}, []
        self.page_secrets = {}
        self.secrets = deque(maxlen=256)
        # Duplicate readable credentials are ambiguous configuration, not two grants.
        hashes = set()
        for row in self.rows.values():
            try:
                hashed = digest(private_read(row["credential_file"], 512).strip())
            except (OSError, ValueError, UnicodeError):
                continue
            if hashed in hashes:
                raise ValueError("shared enrollment credential")
            hashes.add(hashed)
        self.last_ms = self.clock()
        for device in self.rows:
            path = os.path.join(root, device)
            os.makedirs(path, mode=0o700, exist_ok=True)
            fd = private_dir(path)
            os.close(fd)
            self.rows[device] = dict(self.rows[device], directory=path)
            binding = self.reload(device)
            if binding:
                try:
                    self.check_core(binding)
                except web.ApiError:
                    pass  # only this enrollment is unavailable; never another credential
            try:
                record = json.loads(private_read(self.path(device, "binding.json"), 4096))
                if (binding and record["device"] == device and record["grant"] == binding.grant
                        and record["credential_hash"] == binding.credential_hash
                        and record["issued_ms"] <= self.last_ms < record["expires_ms"]
                        and record["expires_ms"] - record["issued_ms"] == DAY_MS
                        and record["last_ms"] <= self.last_ms
                        and re.fullmatch(r"[a-f0-9]{64}", record["token_hash"])):
                    self.bindings[device] = Binding(device, binding.grant, binding.credential_hash,
                                                    record["generation"], binding.bearer)
                    self.tokens[device] = record
            except (OSError, ValueError, KeyError, TypeError):
                pass
            self.new_code(device)

    def path(self, device, name):
        return os.path.join(self.rows[device]["directory"], name)

    def now(self):
        now = self.clock()
        if now < self.last_ms:
            for device in list(self.tokens):
                self.invalidate(device)
            for device in self.codes:
                self.new_code(device)
        self.last_ms = now
        return now

    def invalidate(self, device):
        self.tokens.pop(device, None)
        private_write(self.path(device, "binding.json"), "{}")

    def reload(self, device):
        with self.lock:
            row = self.rows[device]
            try:
                bearer = private_read(row["credential_file"], 512).strip()
                if not re.fullmatch(r"pv_local_[A-Za-z0-9_-]+", bearer):
                    raise ValueError("invalid local credential")
                hashed = digest(bearer)
                if any(b.credential_hash == hashed for d, b in self.bindings.items() if d != device):
                    raise ValueError("shared credential")
            except (OSError, ValueError, UnicodeError):
                self.invalidate(device)
                self.bindings.pop(device, None)
                return None
            if bearer not in self.secrets:
                self.secrets.append(bearer)
            old = self.bindings.get(device)
            if old and old.credential_hash == hashed:
                return old
            if old or device in self.tokens:
                self.invalidate(device)
            binding = Binding(device, row["grant"], hashed, secrets.token_hex(16), bearer)
            self.bindings[device] = binding
            return binding

    def new_code(self, device):
        code = "".join(secrets.choice(web.CODE_ALPHABET) for _ in range(16))
        self.secrets.append(code)
        self.codes[device] = (digest(code), self.clock() + CODE_MS)
        private_write(self.path(device, web.CODE_FILE), code + "\n")

    def sweep(self):
        with self.lock:
            now = self.now()
            for device, (_, expiry) in list(self.codes.items()):
                if now >= expiry:
                    self.new_code(device)

    def pair(self, data):
        if set(data) != {"code"} or not isinstance(data["code"], str):
            raise web.ApiError(400, "bad_body", "Pairing requires only code")
        with self.lock:
            self.sweep()
            now = self.now()
            self.failures = [at for at in self.failures if now - at < 60000]
            if len(self.failures) >= 5:
                raise web.ApiError(429, "rate_limited", "Pairing rate limited")
            given = digest("".join(data["code"].split()).upper())
            device = next((d for d, (code, _) in self.codes.items()
                           if hmac.compare_digest(given, code)), None)
            if device is None:
                self.failures.append(now)
                if len(self.failures) >= 5:
                    for d in self.rows:
                        self.new_code(d)
                raise web.ApiError(403, "wrong_code", "Wrong pairing code")
            binding = self.reload(device)
            if binding is None:
                raise denied()
            self.check_core(binding)
            token = secrets.token_urlsafe(32)
            record = dict(device=device, grant=binding.grant,
                          credential_hash=binding.credential_hash, generation=secrets.token_hex(16),
                          token_hash=digest(token), issued_ms=now, last_ms=now,
                          expires_ms=now + DAY_MS)
            private_write(self.path(device, "binding.json"), json.dumps(record))
            self.bindings[device] = Binding(device, binding.grant, binding.credential_hash,
                                            record["generation"], binding.bearer)
            self.tokens[device] = record
            self.page_secrets[device] = token
            if token not in self.secrets:
                self.secrets.append(token)
            self.new_code(device)
            return dict(token=token, device_id=device, expires_ms=record["expires_ms"])

    def authenticate(self, token):
        with self.lock:
            now = self.now()
            hashed = digest(token)
            device = next((d for d, r in self.tokens.items()
                           if hmac.compare_digest(hashed, r["token_hash"])), None)
            if device is not None:
                record = self.tokens[device]
                binding = self.reload(device)
                if (binding and self.tokens.get(device) is record
                        and record["issued_ms"] <= now < record["expires_ms"]):
                    # Persist a high-water mark, at most once per minute, not per heartbeat.
                    if now - record["last_ms"] >= 60000:
                        record["last_ms"] = now
                        private_write(self.path(device, "binding.json"), json.dumps(record))
                    self.page_secrets[device] = token
                    if token not in self.secrets:
                        self.secrets.append(token)
                    return binding
            raise web.ApiError(401, "not_paired", "Not paired")

    def current(self, binding):
        with self.lock:
            self.now()
            fresh = self.reload(binding.device)
            record = self.tokens.get(binding.device)
            return (fresh == binding and record is not None
                    and record["issued_ms"] <= self.last_ms < record["expires_ms"])

    def clean(self, value):
        """Redact before callers truncate; never serialize credential-bearing objects."""
        if isinstance(value, str):
            with self.lock:
                known = (tuple(self.secrets) + tuple(self.page_secrets.values())
                         + tuple(b.bearer for b in self.bindings.values()))
            for secret in known:
                value = value.replace(secret, "[redacted]")
            value = voice.redact(value, len(value))  # existing bearer/API/JWT defenses, no truncation
            return re.sub(r"(?:pv_local_|pv_access_|pv_refresh_)[A-Za-z0-9_-]+",
                          "[redacted]", value)
        if isinstance(value, list):
            return [self.clean(v) for v in value]
        if isinstance(value, dict):
            return {self.clean(k): self.clean(v) for k, v in value.items()}
        return value
