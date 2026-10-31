"""Metadata-only speech for scoped runtime.v1. Never use trusted-v1 status_text.

Core owns census, freshness and admission. This module only validates and renders
allowlisted fields; arbitrary record text, reasons, refs and diagnostics stay out.
"""
import re

MAIL = "Mail disabled; not checked."
UNAVAILABLE = "Runtime unavailable; could not check. " + MAIL
WORKERS = ("active_workers", "first_mate", "crew", "errand", "commands", "uncertain_jobs")
SESSIONS = ("live", "idle", "asleep", "blocked", "viewer", "unknown")
STATES = ("starting", "queued", "launching", "running", "parked", "orphaned")
MODES = ("local_live", "remote_live", "reused", "cache", "carried", "directory")
SOURCE_STATES = ("covered", "cached", "unavailable", "reverse_only", "unsupported",
                 "history_gap", "unreadable", "backpressure", "handoff_pending",
                 "stale_generation", "ambiguous_owner")
FACETS = ("inventory", "workers", "sessions", "jobs", "work", "capacity", "needs", "mail")
HARNESSES = ("pi", "pi-signed", "claude", "codex", "gemini", "opencode", "opencode-v2",
             "omp", "cursor", "agy", "kimi", "grok", "devin", "muse", "rovo")
LIMIT = 12  # speech bounds, not a completeness assertion


def handle(value):
    return isinstance(value, str) and re.fullmatch(r"[a-zA-Z0-9_.:@-]{1,128}", value) is not None


def request(args):
    """Fixed mapping; no generic RPC, schema override, text or action escape."""
    if (not isinstance(args, dict) or set(args) - {"machine", "ship"}
            or any(not handle(v) for v in args.values())):
        raise ValueError("Invalid runtime selectors")
    return ("ship_runtime", dict(args)) if "ship" in args else ("status", dict(args, schema="runtime.v1"))


def require(condition):
    if not condition:
        raise ValueError("Invalid runtime metadata")


def number(value):
    return type(value) is int and 0 <= value <= 2**64 - 1


def optional_number(value):
    return value is None or number(value)


def counts(value, keys):
    require(isinstance(value, dict) and set(value) <= set(keys))
    require(all(number(v) for v in value.values()))


def labels(values):
    require(isinstance(values, list) and len(values) <= 2000 and all(handle(v) for v in values))
    return ", ".join(sorted(set(values))[:LIMIT]) or "none"


def age(row):
    require(all(optional_number(row.get(k)) for k in ("observed_at", "received_at", "age_ms")))
    at, received, elapsed = (row.get(k) for k in ("observed_at", "received_at", "age_ms"))
    return (f"observed at {at}" if at is not None else "observation time unknown") + (
        f", age {elapsed} milliseconds" if elapsed is not None else ", age unknown") + (
        f", received at {received}" if received is not None else "")


def values(value, keys):
    return ", ".join(k.replace("_", " ") + " " + str(value[k]) for k in keys if k in value)


def render(raw, snapshot, selectors):
    """Fail closed on malformed coverage; never turn missing evidence into zero."""
    try:
        return _render(raw, snapshot, selectors)
    except (KeyError, TypeError, ValueError, RecursionError):
        return UNAVAILABLE


def _render(raw, snapshot, selectors):
    require(raw["schema"] == "runtime.v1")
    coverage, rows, totals = raw["coverage"], raw["ships"], raw["observed_totals"]
    require(coverage["schema"] == "coverage.v1" and coverage["scope"] == "runtime-domain"
            and coverage["complete"] is False and coverage["domain"] is None
            and coverage["state"] == "unknown_domain"
            and type(coverage["inventory_complete"]) is bool
            and coverage["other_machines"] == "not_queried")
    require(isinstance(rows, list) and len(rows) <= 200)
    require(isinstance(totals, dict) and totals["scope"] in ("selected", "local", "observed_sources"))
    require(isinstance(raw["selection"], dict)
            and all(raw["selection"].get(k) == selectors.get(k) for k in ("machine", "ship")))
    machines = raw["machines"]
    labels(machines)
    pairs = set()
    for row in rows:
        name, machine = row["name"], row["machine"]
        require(handle(name) and handle(machine) and machine in machines)
        require(snapshot[1] is None or name in snapshot[1])
        require(all(row[k] == v for k, v in selectors.items() if k == "machine")
                and ("ship" not in selectors or name == selectors["ship"]))
        require((machine, name) not in pairs)
        pairs.add((machine, name))
    if selectors and not rows:
        return UNAVAILABLE  # hidden, absent and removed are deliberately indistinguishable
    sources = coverage["sources"]
    require(isinstance(sources, list) and len(sources) <= 1600)
    source_ids = {}
    covered_facets = set()
    observations = []
    for source in sources:
        ident = source["id"]
        require(isinstance(ident, str) and 0 < len(ident) <= 300 and ident not in source_ids)
        require(source["machine"] in machines and source["facet"] in FACETS
                and source["state"] in SOURCE_STATES and source["mode"] in MODES)
        require(source["ship"] is None or (source["machine"], source["ship"]) in pairs)
        require(source["barrier_mode"] in (None, "on_demand")
                and optional_number(source["max_source_age_ms"]))
        stamp = age(source)
        source_ids[ident] = source
        covered_facets.add((source["machine"], source["ship"], source["facet"]))
        if source["facet"] not in ("inventory", "workers", "sessions", "jobs"):
            continue
        dated = source["mode"] in ("cache", "carried") or source["state"] == "cached"
        state = "last observed" if dated else (
            "checked" if source["state"] == "covered" else "could not check")
        line = (f"{source['machine']} {source['ship'] or 'directory'} {source['facet']}: "
                f"{state}, {source['mode']}, {stamp}.")
        if source["state"] == "reverse_only":
            line += " Unsupported read direction / incomplete feature."
        observations.append(line)
    require(all((machine, name, facet) in covered_facets for machine, name in pairs
                for facet in ("inventory", "workers", "sessions", "jobs")))
    lines = ["Runtime coverage: domain unknown; incomplete. Inventory " + (
        "complete for this selection." if coverage["inventory_complete"] else "incomplete; could not check all entries.")]
    for field in ("machines", "requested_machines", "queried_machines", "reused_machines",
                  "cached_machines", "omitted_machines"):
        lines.append(field.replace("_", " ") + ": " + labels(raw[field]) + ".")
        if len(raw[field]) > LIMIT:
            lines.append(f"{len(raw[field]) - LIMIT} additional machine labels omitted from speech.")
    lines += observations[:LIMIT]
    if len(observations) > LIMIT:
        lines.append(f"{len(observations) - LIMIT} additional source observations omitted from speech.")
    if len(rows) > LIMIT:
        lines.append(f"{len(rows) - LIMIT} additional ship rows omitted from speech; totals remain scoped observations.")
    # Validate all totals and their contributors before speaking any counts.
    rendered_totals = []
    for metric in WORKERS + ("sessions", "recorded_running_jobs", "nonterminal_jobs", "job_states"):
        total = totals.get(metric)
        if total is None:
            rendered_totals.append(metric.replace("_", " ") + ": could not check.")
            continue
        require(isinstance(total, dict) and type(total["complete"]) is bool)
        refs = total["sources"]
        require(isinstance(refs, list) and len(refs) <= 1600 and len(set(refs)) == len(refs))
        facet = "sessions" if metric == "sessions" else "jobs" if metric in (
            "recorded_running_jobs", "nonterminal_jobs", "job_states") else "workers"
        require(all(ref in source_ids and source_ids[ref]["facet"] == facet for ref in refs))
        value = total["value"]
        if metric in ("sessions", "job_states") and value is not None:
            counts(value, SESSIONS if metric == "sessions" else STATES)
        else:
            require(optional_number(value))
        # Core's legacy recorded-running aggregate may include directory records.
        # Withhold any aggregate with dated contributors instead of presenting it as current.
        dated = any(source_ids[ref]["mode"] in ("cache", "carried", "directory")
                    or source_ids[ref]["state"] == "cached" for ref in refs)
        valid = value is not None and bool(refs) and not dated
        contributors = sorted({source_ids[ref]["machine"] + "/" + (source_ids[ref]["ship"] or "directory")
                               for ref in refs})
        status = "complete for contributing selection" if total["complete"] else "partial; could not check all"
        text = values(value, SESSIONS if metric == "sessions" else STATES) if isinstance(value, dict) else str(value)
        contribution = ", ".join(contributors[:LIMIT])
        if len(contributors) > LIMIT:
            lines.append(f"{len(contributors) - LIMIT} additional contributors omitted from speech for an aggregate below.")
            contribution += "; contributor list partial"
        rendered_totals.append(metric.replace("_", " ") + ": " + status + "; " + (
            text + "; contributors " + contribution if valid else "could not check current counts") + ".")
    # Observation ages and omissions precede both aggregate and per-ship counts.
    ship_lines = []
    for row in sorted(rows, key=lambda r: (r["machine"], r["name"]))[:LIMIT]:
        runtime = row.get("runtime")
        prefix = row["machine"] + "/" + row["name"]
        if runtime is None:
            ship_lines.append(prefix + ": could not check runtime.")
            continue
        require(type(runtime["current"]) is bool and runtime["mode"] in MODES)
        stamp = age(runtime)
        current = runtime["current"] and runtime["mode"] in ("local_live", "remote_live", "reused")
        lines.append(prefix + ": " + ("observation" if current else "last observed") + ", "
                     + runtime["mode"] + ", " + stamp + ".")
        facts = runtime["facts"]
        require(facts["name"] == row["name"])
        working = row["working"]
        require(type(working) is bool or working == "unknown")
        census = facts.get("census")
        checked = current and isinstance(census, dict)
        ship_lines.append(prefix + ": working " + (
            "yes" if checked and working is True else "no, checked idle" if checked and working is False
            else "could not check") + ".")
        lead = "Checked" if current else "Last observed (not current)"
        if census is None:
            ship_lines.append("Workers and sessions: could not check; details omitted or unsupported.")
        else:
            require(isinstance(census, dict) and number(census["observed_at"]))
            lines.append(prefix + ": " + ("census observed at " if current else "census last observed at ")
                         + str(census["observed_at"]) + ".")
            require(all(type(census[k]) is bool for k in ("workers_complete", "sessions_complete", "activity_complete")))
            require(all(number(census[k]) for k in WORKERS))
            counts(census["sessions"], SESSIONS)
            ship_lines.append(lead + " workers " + ("complete" if census["workers_complete"] else "partial; could not check all")
                              + ": " + values(census, WORKERS) + ". Commands and uncertain jobs are separate, not workers.")
            ship_lines.append(lead + " sessions " + ("complete" if census["sessions_complete"] else "partial; could not check all")
                              + ": " + values(census["sessions"], SESSIONS) + ". Live overlaps state categories; sessions are not additive workers.")
            details = census.get("session_details")
            if details is None:
                ship_lines.append("Session details omitted or unsupported; this does not mean no sessions.")
            else:
                require(isinstance(details, list) and len(details) <= 201)
                if len(details) > LIMIT:
                    lines.append(f"{prefix}: {len(details) - LIMIT} session rows omitted from speech.")
                for detail in details[:LIMIT]:
                    require(detail["role"] in ("first_mate", "crew", "errand", "command", "viewer", "unknown")
                            and detail["state"] in ("working", "idle", "asleep", "blocked", "viewer", "unknown"))
                    harness = detail.get("harness")
                    mode = detail.get("host_mode")
                    ship_lines.append(lead + " session: " + detail["role"].replace("_", " ") + ", "
                                      + detail["state"] + ", harness " + (harness if harness in HARNESSES else "unknown")
                                      + ", host mode " + (mode if mode == "hosted" else "unknown") + ".")
        harness, host = facts.get("harness"), facts.get("host_mode")
        ship_lines.append("Recorded configuration (not liveness): harness " + (harness if harness in HARNESSES else "unknown")
                          + ", host mode " + (host if host in ("hosted", "headless") else "unknown") + ".")
        counts(facts["job_states"], STATES)
        require(type(facts["jobs_truncated"]) is bool and optional_number(facts["open_tasks"])
                and optional_number(facts["jobs_total"]))
        if facts["jobs_total"] is None:
            ship_lines.append(lead + " recorded job states: could not check.")
        else:
            ship_lines.append(lead + " recorded job states " + ("partial; could not check all" if facts["jobs_truncated"] else "within bounded scan")
                              + ": " + (values(facts["job_states"], STATES) or "no states observed") + ".")
        ship_lines.append(lead + " open task backlog: " + (str(facts["open_tasks"]) if facts["open_tasks"] is not None
                          else "could not check") + ". Backlog and recorded running jobs do not establish workers.")
    lines += ["Observed totals, scope " + totals["scope"] + "; not fleet-wide."] + rendered_totals + ship_lines
    return " ".join(lines + [MAIL])
