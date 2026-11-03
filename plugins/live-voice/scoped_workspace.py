"""Volatile, device-owned read workspaces independent of audio sessions.

Only explicit reads contact the runtime. A device keeps one current workspace
and one in-flight read reservation, including across replacement generations.
"""
import secrets
import sys
import threading
from dataclasses import dataclass, field

sys.dont_write_bytecode = True
import scoped_runtime
import web
from scoped_access import denied

TTL_SECS = 900
VIEWS = ("directory", "runtime")


def stale():
    return web.ApiError(409, "stale_state", "Stale state")


def bad_body():
    return web.ApiError(400, "bad_body", "Invalid workspace request")


@dataclass(eq=False)
class Workspace:
    binding: object
    snapshot: tuple
    seen: float
    ident: str = field(default_factory=lambda: secrets.token_urlsafe(24))


class Workspaces:
    def __init__(self, access, capabilities, reads, clock):
        self.access, self.capabilities, self.reads, self.clock = access, capabilities, reads, clock
        self.lock = threading.RLock()
        self.current = {}
        self.reading = set()
        self.closed = False

    def sweep(self):
        """Local expiry only; browser/maintenance polling never admits a read."""
        with self.lock:
            now = self.clock()
            for device, ctx in list(self.current.items()):
                if now < ctx.seen or now - ctx.seen >= TTL_SECS:
                    self.current.pop(device)

    def discard(self, ctx):
        with self.lock:
            if self.current.get(ctx.binding.device) is ctx:
                self.current.pop(ctx.binding.device)

    def owner(self, ctx):
        # Call under self.lock, including the final response construction. Local
        # binding checks do no core I/O and prevent old tokens adopting new state.
        now = self.clock()
        if (self.closed or self.current.get(ctx.binding.device) is not ctx
                or now < ctx.seen or now - ctx.seen >= TTL_SECS):
            self.discard(ctx)
            raise stale()
        if not self.access.current(ctx.binding):
            self.discard(ctx)
            raise denied()

    def invalidate_replaced(self, device):
        """Pairing must not delete a newer workspace opened after that pairing."""
        with self.lock:
            ctx = self.current.get(device)
            if ctx and not self.access.current(ctx.binding):
                self.discard(ctx)

    def shutdown(self):
        with self.lock:
            self.closed = True
            self.current.clear()
        # Leave reservations until their workers unwind. A closed adapter cannot
        # admit replacements and an old worker must never free a newer reservation.

    def request(self, path, binding, data):
        if not isinstance(data, dict):
            raise bad_body()
        if path == "/api/workspace/open":
            return self.open(binding, data)
        if path == "/api/workspace/read":
            return self.read(binding, data)
        if path == "/api/workspace/close":
            return self.close(binding, data)
        raise web.ApiError(404, "not_found", "Not found")

    def open(self, binding, data):
        from door import client_of
        if set(data) - {"client"}:
            raise bad_body()
        if "client" in data and not isinstance(data["client"], dict):
            raise web.ApiError(400, "bad_client", "Invalid client")
        client_of(data.get("client"))  # description only; never retained/instructions
        snapshot = self.capabilities(binding)
        with self.lock:
            if self.closed:
                raise stale()
            if not self.access.current(binding):
                raise denied()
            self.sweep()
            ctx = Workspace(binding, snapshot, self.clock())
            self.current[binding.device] = ctx
            return dict(device_id=binding.device, workspace_id=ctx.ident,
                        views=list(VIEWS) if "fleet:read" in snapshot[0] else [],
                        expires_in_secs=TTL_SECS)

    def lookup(self, binding, ident):
        ctx = self.current.get(binding.device)
        if ctx is None or ctx.binding != binding or ctx.ident != ident:
            raise stale()
        self.owner(ctx)
        return ctx

    def close(self, binding, data):
        if set(data) != {"workspace_id"} or not isinstance(data["workspace_id"], str):
            raise bad_body()
        with self.lock:
            ctx = self.lookup(binding, data["workspace_id"])
            self.discard(ctx)
            return dict(device_id=binding.device, workspace_id=ctx.ident, ok=True)

    def read(self, binding, data):
        if (not {"workspace_id", "view"} <= set(data)
                or set(data) - {"workspace_id", "view", "args"}
                or not isinstance(data["workspace_id"], str)
                or data["view"] not in VIEWS):
            raise bad_body()
        args = data.get("args", {})
        if not isinstance(args, dict) or data["view"] == "directory" and args:
            raise bad_body()
        if data["view"] == "runtime":
            try:
                scoped_runtime.request(args)
            except (TypeError, ValueError):
                raise bad_body() from None
        with self.lock:
            ctx = self.lookup(binding, data["workspace_id"])
            if "fleet:read" not in ctx.snapshot[0]:
                raise denied()
            if binding.device in self.reading:
                raise web.ApiError(409, "workspace_busy", "Workspace read already running")
            self.reading.add(binding.device)
            ctx.seen = self.clock()
        try:
            error, result = None, None
            try:
                if data["view"] == "directory":
                    result = self.reads.directory(binding, ctx.snapshot)
                else:
                    result = self.reads.runtime(binding, ctx.snapshot, args)
            except web.ApiError as failure:
                # The transport normally already uses fixed errors. Do not make
                # that an implicit trust boundary for another read-service user.
                error = denied() if failure.code == "access_denied" else web.ApiError(
                    503, "unavailable", "Core unavailable")
            # The actual read is core-admitted; fence once after even failed I/O.
            # Never release a cached result/error under cached capabilities.
            with self.lock:
                self.owner(ctx)
            snapshot = self.capabilities(binding)
            if snapshot != ctx.snapshot or "fleet:read" not in snapshot[0]:
                raise denied()
            result = self.access.clean(result)
            with self.lock:
                self.owner(ctx)
                if error:
                    raise error
                ctx.seen = self.clock()
                return dict(device_id=binding.device, workspace_id=ctx.ident,
                            view=data["view"], data=result)
        except web.ApiError:
            # A failed read cannot keep stale protected state alive. Only this
            # generation is removed; another open remains independently owned.
            self.discard(ctx)
            raise
        finally:
            with self.lock:
                self.reading.discard(binding.device)
