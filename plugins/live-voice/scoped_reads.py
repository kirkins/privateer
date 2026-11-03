"""Closed read projections shared by scoped voice and microphone-free clients.

Callers own admission context and post-I/O authority/ownership fences. This module
never exposes an operator DTO, runs an action or falls back to trusted-v1 reads.
"""
import sys

sys.dont_write_bytecode = True
import scoped_runtime
import web
from scoped_send import ship_name


def unavailable():
    return web.ApiError(503, "unavailable", "Core unavailable")


COVERAGE_FIELDS = ("schema", "scope", "complete", "domain", "state", "inventory_complete",
                   "other_machines")
SOURCE_FIELDS = ("id", "state", "machine", "ship", "facet", "mode", "observed_at",
                 "received_at", "age_ms", "barrier_mode", "max_source_age_ms", "reason")


def directory(result, snapshot):
    """Construct a fresh DTO; never pass through an evolving runtime row/source."""
    try:
        if result["schema"] != "runtime.v1" or not isinstance(result["ships"], list):
            raise ValueError()
        names = [row["name"] for row in result["ships"]]
        if any(not ship_name(n) for n in names):
            raise ValueError()
        if snapshot[1] is not None and any(n not in snapshot[1] for n in names):
            raise ValueError()
        raw = result["coverage"]
        coverage = {k: raw[k] for k in COVERAGE_FIELDS}
        if (coverage["schema"] != "coverage.v1" or coverage["scope"] != "runtime-domain"
                or coverage["complete"] is not False or coverage["domain"] is not None
                or coverage["state"] != "unknown_domain"
                or not isinstance(coverage["inventory_complete"], bool)
                or coverage["other_machines"] != "not_queried"
                or not isinstance(raw["sources"], list)):
            raise ValueError()
        sources = []
        for source in raw["sources"]:
            if source["facet"] != "inventory":
                continue
            item = {k: source[k] for k in SOURCE_FIELDS}
            if (any(not isinstance(item[k], str) or not item[k] for k in ("id", "machine", "reason"))
                    or item["ship"] is not None and item["ship"] not in names
                    or item["state"] not in ("covered", "cached", "unavailable", "reverse_only",
                        "unsupported", "history_gap", "unreadable", "backpressure", "handoff_pending",
                        "stale_generation", "ambiguous_owner")
                    or item["mode"] not in ("local_live", "remote_live", "reused", "cache", "carried", "directory")
                    or item["barrier_mode"] not in (None, "on_demand")
                    or any(item[k] is not None and (type(item[k]) is not int or item[k] < 0)
                           for k in ("observed_at", "received_at", "age_ms", "max_source_age_ms"))):
                raise ValueError()
            sources.append(item)
        coverage["sources"] = sources
        return {"names": names, "coverage": coverage}
    except (KeyError, TypeError, ValueError):
        raise unavailable() from None


class Reads:
    def __init__(self, runtime):
        self.core = runtime

    def directory(self, binding, snapshot):
        raw = self.core.call(binding, "ships", {"schema": "runtime.v1"})
        return directory(raw, snapshot)

    def runtime(self, binding, snapshot, args):
        # Validation remains visible to the caller, so voice retains its existing
        # invalid-selector wording and HTTP can reject a malformed request.
        command, arguments = scoped_runtime.request(args)
        text = scoped_runtime.UNAVAILABLE
        if snapshot[1] is None or "ship" not in args or args["ship"] in snapshot[1]:
            try:
                raw = self.core.call(binding, command, arguments)
                text = scoped_runtime.render(raw, snapshot, args)
            except web.ApiError:
                pass  # fixed safe text; callers still fence this failed read
        return dict(available=text != scoped_runtime.UNAVAILABLE, text=text)
