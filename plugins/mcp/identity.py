"""Bounded, path-free identities; no config, credentials or venv inspection."""
import hashlib
import json
from pathlib import Path

# Same framed file set as src/door/identity.rs, in the same order.
FILES = ("argument_errors.py", "door.py", "file_transfer.py", "identity.py",
         "launch.py", "oauth_http.py", "request_log.py", "requirements.lock", "tools.py")
FILE_LIMIT = 1024 * 1024


def read(path):
    with path.open("rb") as source:
        data = source.read(FILE_LIMIT + 1)
    if len(data) > FILE_LIMIT:
        raise ValueError("identity file too large")
    return data


def observe(core_identity=None, root=None):
    """Startup snapshot of this adapter, independently of the core's bundle."""
    root = Path(__file__).resolve().parent if root is None else root
    core = (core_identity or {}).get("core")
    identity = {"schema": "door-identity.v1", "observation": "session_start",
                "core": dict(core) if core else None,
                "adapter": None, "adapter_drift": None, "identity_status": "unavailable"}
    try:
        digest = hashlib.sha256()
        complete = True
        for name in FILES:
            try:
                data = read(root / name)
            except FileNotFoundError:
                # Match Rust: absence is mismatch evidence, not an I/O failure.
                digest.update(len(name).to_bytes(8, "big"))
                digest.update(name.encode("ascii"))
                digest.update((2**64 - 1).to_bytes(8, "big"))
                complete = False
                continue
            digest.update(len(name).to_bytes(8, "big"))
            digest.update(name.encode("ascii"))
            digest.update(len(data).to_bytes(8, "big"))
            digest.update(data)
        # The selected source manifest can have a different filename. Its
        # digest and recognized release layout come from the core, not an
        # arbitrary manifest version string or private configuration.
        source = (core_identity or {}).get("adapter") or {}
        manifest_revision = source.get("manifest_revision")
        if manifest_revision is None:
            manifest_revision = hashlib.sha256(read(root / "manifest.json")).hexdigest()
        identity["adapter"] = {"revision": digest.hexdigest(), "complete": complete,
                               "manifest_revision": manifest_revision,
                               "release_commit": source.get("release_commit")}
        if identity["core"]:
            core, adapter = identity["core"], identity["adapter"]
            identity["adapter_drift"] = (
                not complete
                or core["adapter_revision"] != adapter["revision"]
                or core["manifest_revision"] != adapter["manifest_revision"]
                or bool(core.get("release_commit") and adapter["release_commit"]
                        and core["release_commit"] != adapter["release_commit"]))
            identity["identity_status"] = "ok" if complete else "incomplete"
        else:
            identity["identity_status"] = "core_unknown"
    except (OSError, ValueError):
        pass  # No exception text, filesystem paths or file content in diagnostics.
    return identity


def server_version(identity):
    # Content-derived even for non-release checkouts where every package still
    # has the same semantic version. Any adapter, manifest or core identity
    # change changes the stamp. This is a diagnostic, not an authenticity proof.
    stamp = hashlib.sha256(json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    return "mcp-" + stamp
