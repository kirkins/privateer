"""Fixed, metadata-only selected recorded reads. No actions or authored speech.

Core owns references, paging, observation, custody and admission. Opaque refs and
continuations are returned unchanged as tool-only metadata, never synthesized.
"""
import json
import re
import sys
import time
import uuid

sys.dont_write_bytecode = True  # before sibling imports, including standalone use
from scoped_runtime import (MAIL, SPEECH_BUDGET, LIMIT, HARNESSES, STATES, require,
                            number, optional_number, relative_age, observed_count)

UNAVAILABLE = "Recorded detail unavailable; could not check. Progress or question text is withheld. " + MAIL
COMMANDS = ("ship_jobs", "job_inspect", "ship_work", "machine_capacity", "runtime_needs")
JOB_STATES = STATES + ("succeeded", "failed", "cancelled", "timed_out", "resumed")
WORK_STATES = ("ready", "offered", "claimed", "executing", "waiting", "stopped", "fault", "unscheduled")
KINDS = ("first_mate", "crew", "errand", "command")
REF_KEYS = {"origin", "incarnation", "ship", "task_id", "id"}
AGE_BASES = ("creation", "payload_spawn", "finish", "state_transition", "recorded_text",
             "park_updated", "wait_transition_unknown", "schedule_creation", "schedule_check", "not_before")
EVIDENCE = ("capacity_wait", "orphaned_job", "scheduling_fault", "stale_remote_wait",
            "workspace_disk_wait", "first_mate_no_route")
UNKNOWN = ("runtime_observation", "live_process_liveness", "provider_health",
           "aggregate_mail_coverage", "task_captain_actionability")


def handle(value, limit=160):
    return isinstance(value, str) and re.fullmatch(r"[a-zA-Z0-9_.:@-]{1," + str(limit) + "}", value) is not None


def token(value):
    try:
        return isinstance(value, str) and str(uuid.UUID(value)) == value
    except ValueError:
        return False


def reference(value, ship, origin=None):
    require(isinstance(value, dict) and set(value) == REF_KEYS
            and all(handle(v) for v in value.values()) and value["ship"] == ship
            and (origin is None or value["origin"] == origin))
    return dict(value)


def request(command, args):
    """Model arguments cannot choose include_text, RPC, schema, route or action."""
    require(command in COMMANDS and isinstance(args, dict))
    allowed = {
        "ship_jobs": {"ship", "state", "kind", "limit", "continuation"},
        "job_inspect": {"ship", "reference"},
        "ship_work": {"ship", "limit", "continuation"},
        "machine_capacity": {"machine", "ship", "task_id"},
        "runtime_needs": {"ship", "limit", "continuation"},
    }[command]
    require(set(args) <= allowed)
    required = "machine" if command == "machine_capacity" else "ship"
    require(required in args)
    require(all(handle(v, 128) for k, v in args.items() if k in ("machine", "ship", "task_id")))
    require("task_id" not in args or "ship" in args)
    require("limit" not in args or number(args["limit"]) and 1 <= args["limit"] <= 50)
    require("continuation" not in args or token(args["continuation"]))
    require("state" not in args or args["state"] in JOB_STATES + ("completed",))
    require("kind" not in args or args["kind"] in KINDS)
    if command == "job_inspect":
        reference(args["reference"], args["ship"])
    result = dict(args)
    if command != "machine_capacity":
        result["include_text"] = False
    return command, result


def tools():
    specs = []
    ref = {"type": "object", "properties": {k: {"type": "string"} for k in sorted(REF_KEYS)},
           "required": sorted(REF_KEYS), "additionalProperties": False}
    for command in COMMANDS:
        properties = {"ship": {"type": "string"}}
        required = ["ship"]
        if command in ("ship_jobs", "ship_work", "runtime_needs"):
            properties.update(limit={"type": "integer", "minimum": 1, "maximum": 50},
                              continuation={"type": "string"})
        if command == "ship_jobs":
            properties.update(state={"type": "string", "enum": list(JOB_STATES + ("completed",))},
                              kind={"type": "string", "enum": list(KINDS)})
        if command == "job_inspect":
            properties["reference"] = ref
            required.append("reference")
        if command == "machine_capacity":
            properties.update(machine={"type": "string"}, task_id={"type": "string"})
            required = ["machine"]
        specs.append({"type": "function", "name": "privateer_" + command,
                      "description": "Read-only recorded metadata; text withheld. Use exact backend refs/continuations, never read opaque metadata aloud. Not liveness or admission.",
                      "inputSchema": {"type": "object", "properties": properties,
                                      "required": required, "additionalProperties": False}})
    return specs


def stamp(at, elapsed, basis, now):
    require(optional_number(at) and optional_number(elapsed))
    return ((f"at {at}" if at is not None else "time unknown")
            + (f", age {elapsed} milliseconds" if elapsed is not None else ", age unknown")
            + ", basis " + basis.replace("_", " ") + ", " + relative_age(at, now))


def age(value, now):
    require(isinstance(value, dict) and value["basis"] in AGE_BASES)
    return stamp(value["at_ms"], value["age_ms"], value["basis"], now)


def label(value, choices):
    require(value in choices)
    return value.replace("_", " ")


def boolean(value):
    require(type(value) is bool)
    return "yes" if value else "no"


def allowed_ship(ship, snapshot):
    return snapshot[1] is None or ship in snapshot[1]


def wait(value, ship, origin, now, refs, task_ids):
    if value is None:
        return "Wait: none recorded."
    kind = label(value["kind"], ("capacity", "disk", "job", "task", "request", "time", "remote", "attention", "manual"))
    boolean(value["reference_withheld"])
    parts = ["Wait " + kind + "; " + age(value["since"], now)]
    if value.get("job") is not None:
        r = reference(value["job"], ship, origin)
        refs.append(r)
        parts.append("recorded child job")
    if value.get("task_id") is not None:
        require(handle(value["task_id"]))
        task_ids.append(value["task_id"])
        parts.append("recorded dependency task")
    if value["reference_withheld"]:
        parts.append("wait target withheld or could not check")
    return "; ".join(parts) + "."


def job_line(row, ship, origin, now, refs, task_ids):
    r = reference(row["reference"], ship, origin)
    refs.append(r)
    require(row["owner"] == ship and row["liveness"] == "unknown_recorded_state")
    state = label(row["state"], JOB_STATES)
    completed = row["state"] in JOB_STATES[6:]
    require(type(row["completed"]) is bool and row["completed"] == completed)
    require(row["exit"] == (row["state"] if completed else "not_recorded"))
    worker = row.get("worker")
    require(worker is None or handle(worker))
    parts = ["Recorded job, " + label(row["kind"], KINDS)
             + ", state " + state + "; terminal " + boolean(row["completed"])
             + "; exit " + label(row["exit"], JOB_STATES + ("not_recorded",))
             + "; live process liveness could not check."]
    parts.append("Created " + age(row["created"], now) + "; spawned " + age(row["spawned"], now)
                 + "; finished " + age(row["finished"], now) + "; state since " + age(row["state_since"], now) + ".")
    parts.append("Recorded harness " + (row.get("harness") if row.get("harness") in HARNESSES else "unknown") + ".")
    parts.append(wait(row.get("wait"), ship, origin, now, refs, task_ids))
    for key in ("parent", "retry_of", "resume_of", "relaunch_of", "resumed_as"):
        if row.get(key) is not None:
            related = reference(row[key], ship, origin)
            refs.append(related)
            parts.append(key.replace("_", " ") + " recorded job.")
    if row["state"] == "resumed":
        parts.append("Resumed is lineage, not success.")
    for key in ("progress", "error"):
        if row.get(key) is not None:
            parts.append("Recorded " + key + " " + age(row[key]["at"], now) + "; text withheld.")
    return " ".join(parts)


def work_line(row, ship, origin, now, refs, work_refs, task_ids):
    require(row["owner"] == ship and handle(row["task_id"]))
    task_ids.append(row["task_id"])
    r = row.get("reference")
    if r is not None:
        require(isinstance(r, dict) and set(r) == {"origin", "ship", "task_id", "revision"}
                and r["origin"] == origin and r["ship"] == ship and r["task_id"] == row["task_id"]
                and number(r["revision"]))
        work_refs.append(dict(r))
    if row.get("job") is not None:
        refs.append(reference(row["job"], ship, origin))
    return ("Recorded task, task state "
            + label(row["task_state"], ("open", "running", "blocked", "pending", "done", "cancelled"))
            + ", schedule " + label(row["state"], WORK_STATES)
            + "; scheduled " + boolean(row["scheduled"]) + "; backlog " + boolean(row["backlog"])
            + "; claimed " + boolean(row["claimed"]) + "; offered " + boolean(row["offered"]) + ". "
            + "Created " + age(row["created"], now) + "; last checked " + age(row["last_checked"], now)
            + "; next check " + age(row["next_check"], now) + ". "
            + wait(row.get("wait"), ship, origin, now, refs, task_ids))


def inspection(raw, command, args, now):
    ship, c = args["ship"], raw["coverage"]
    require(raw["schema"] == "inspection.v1" and raw["ship"] == ship
            and c["count_basis"] == "authorized_scan_window"
            and type(c["counts_complete"]) is bool)
    history = "recorded_tasks" if command == "ship_work" else "retained_only_prior_history_unknown"
    require(c["history"] == history)
    source = raw["source"]
    require(handle(source) and (token(raw["snapshot"]) or command == "job_inspect" and raw["snapshot"] is None))
    lines = ["Count basis: authorized scan window; " + ("complete within that window." if c["counts_complete"] else "partial; could not check all.")]
    lines.append("Recorded tasks only; not workers." if command == "ship_work" else
                 "Retained terminal history only; prior pruned history unknown. Recorded state is not process liveness.")
    counts = raw["counts"]
    keys = JOB_STATES + ("completed",) if command != "ship_work" else WORK_STATES + ("scheduled", "unscheduled_total", "backlog")
    require(isinstance(counts, dict) and set(counts) <= set(keys) and all(number(v) for v in counts.values()))
    count_line = "Recorded counts: " + (", ".join(k.replace("_", " ") + " " + observed_count(counts[k], c["counts_complete"])
                                                       for k in keys if k in counts) or "no states observed in window") + "."
    refs, work_refs, task_ids, rows = [], [], [], []
    if command == "job_inspect":
        require(raw["job"] is not None and raw["job"]["reference"] == args["reference"])
        rows = [job_line(raw["job"], ship, source, now, refs, task_ids)]
    else:
        data = raw["work"] if command == "ship_work" else raw["jobs"]
        require(isinstance(data, list) and len(data) <= 50)
        for row in data:
            rows.append(work_line(row, ship, source, now, refs, work_refs, task_ids) if command == "ship_work"
                        else job_line(row, ship, source, now, refs, task_ids))
    if command == "job_inspect":
        count_line = "Counts not supplied for exact job detail."
    # The timestamp-bearing rows are intact units; counts never imply page totals.
    return lines + [count_line], rows, dict(source=source, snapshot=raw["snapshot"], references=refs,
                                          work_references=work_refs, task_ids=task_ids)


def cap_limit(value):
    require(optional_number(value))
    return str(value) if value is not None else "unbounded by this policy"


def capacity(raw, snapshot, args, now):
    cap = raw["capacity"]
    require(isinstance(cap, dict) and cap["headroom_basis"] == "heavy_slots_only_not_agent_capacity")
    restricted = snapshot[1] is not None
    require(cap["shared_host"] == ("withheld" if restricted else "visible"))
    for key in ("occupied_heavy_slots", "reservation_count", "memory_reservation_count", "queue_depth"):
        require(number(cap[key]))
    holds, queue = cap["holds"], cap["queue"]
    require(isinstance(holds, list) and len(holds) <= 2000 and isinstance(queue, list) and len(queue) <= 2000)
    require(len(holds) == cap["reservation_count"] and len(queue) == cap["queue_depth"])
    lead = "Selected-ship" if restricted else "Ledger"
    lines = []
    if restricted:
        require(all(cap[k] is None for k in ("policy", "readings", "route_availability", "charter_heavy_cap",
                "charter_occupied_heavy_slots", "free_heavy_slots")) and cap["route_state"] == "withheld"
                and cap["charter_policy_state"] == "withheld")
        require(all(h["ship"] == args["ship"] for h in holds))
        lines.append("Shared host policy, headroom, readings, totals and routes withheld; could not check shared capacity.")
    else:
        policy = cap["policy"]
        if policy is None:
            lines.append("Host heavy policy could not check.")
        else:
            source = policy["sources"]["max_heavy_jobs"]
            require(source in ("host_configuration", "runtime_default"))
            configured = cap_limit(policy["configured_host_heavy_cap"])
            lines.append("Host configured heavy cap " + (configured if source == "host_configuration" else "not configured; runtime default")
                         + "; resolved heavy cap " + cap_limit(policy["resolved_host_heavy_cap"]) + ".")
        state = label(cap["charter_policy_state"], ("known", "unknown", "not_requested", "withheld"))
        lines.append("Charter policy " + state + ", source " + label(cap["charter_policy_source"], ("active_task_charter", "unknown"))
                     + ("; heavy cap " + cap_limit(cap["charter_heavy_cap"]) if state == "known" else "; cap could not check") + ".")
        headroom = cap["free_heavy_slots"]
        require(optional_number(headroom) and optional_number(cap["charter_occupied_heavy_slots"]))
        lines.append("Heavy-slot headroom " + (str(headroom) if headroom is not None and state in ("known", "not requested") else "could not check finite headroom")
                     + "; not agent capacity, not a promise of admission.")
    lines.append(f"{lead} recorded occupied heavy slots {cap['occupied_heavy_slots']}; holds {cap['reservation_count']}, "
                 f"memory reservations {cap['memory_reservation_count']}; queue {cap['queue_depth']}; capacity wait recorded "
                 + boolean(cap["capacity_blocked"]) + ". Holds remain conservative, including uncertain payloads; this read releases nothing.")
    rows = []
    for hold in holds:
        require(hold["liveness"] == "unknown_held_conservatively")
        rows.append("Recorded hold: heavy " + boolean(hold["heavy"]) + "; admitted "
                    + stamp(hold["admitted_ms"], None, "admission", now) + "; liveness unknown, held conservatively.")
    for item in queue:
        if restricted:
            require(item["blocked"] == "capacity" and item["blocked_at_ms"] is None
                    and item["blocker_basis"] == "unattributed_capacity_wait")
            rows.append("Recorded queue entry: unattributed capacity wait; creation "
                        + stamp(item["created_ms"], None, "creation", now) + ".")
        else:
            require(item["blocker_basis"] == "last_admission_classification_not_current_diagnosis")
            blocker = "could not check" if item["blocked"] is None else label(item["blocked"], ("cpu", "disk", "heavy_slot"))
            rows.append("Recorded queue entry: creation " + stamp(item["created_ms"], None, "creation", now)
                        + "; last admission classification " + blocker + ", "
                        + stamp(item["blocked_at_ms"], None, "last admission classification", now) + "; not current diagnosis.")
    readings = cap["readings"]
    if not restricted:
        if readings is None:
            lines.append("Host readings could not check.")
        else:
            for key in ("cpus", "load", "released_load", "recent_cpus"):
                v = readings[key]
                require(type(v) in (int, float) and 0 <= v < 1e15)
                lines.append("Host reading " + key.replace("_", " ") + " " + str(v) + ".")
            for key in ("memory_available_bytes", "memory_total_bytes", "recent_memory_bytes"):
                require(number(readings[key]))
                lines.append("Host reading " + key.replace("_", " ") + " " + str(readings[key]) + ".")
            disks = readings["disks"]
            require(isinstance(disks, list) and len(disks) <= 2000)
            for disk in disks:
                require(disk["state"] in ("covered", "unknown"))
                parts = []
                for key in ("available_bytes", "total_bytes", "committed_bytes"):
                    require(optional_number(disk[key]))
                    parts.append(key.replace("_", " ") + " " + (str(disk[key]) if disk[key] is not None and disk["state"] == "covered" else "could not check"))
                rows.append("Filesystem reading: " + ", ".join(parts) + ".")
    return lines, rows, {}


def needs(raw, args, now):
    require(raw["capacity"] is None and raw["coverage"]["complete"] is False)
    data = raw["needs"]
    require(isinstance(data, list) and len(data) <= 50)
    rows = []
    for row in data:
        require(row["ship"] == args["ship"] and handle(row["id"]))
        evidence = "could not check" if row["evidence"] is None else label(row["evidence"], EVIDENCE)
        rows.append("Recorded need, " + label(row["kind"], ("approval", "decision", "blocker"))
                    + ", source " + label(row["source"], ("mail", "job", "schedule", "remote_wait", "first_mate", "workspace"))
                    + ", evidence " + evidence + "; created " + stamp(row["created_ms"], None, "creation", now)
                    + "; deadline " + stamp(row["deadline_ms"], None, "deadline", now)
                    + "; observed " + stamp(row["observed_ms"], None, "needs observation", now) + ".")
    unknown = raw["unknown"]
    require(isinstance(unknown, list) and len(unknown) <= len(UNKNOWN) and all(v in UNKNOWN for v in unknown))
    return ["Needs partial; aggregate mail coverage could not check. Reading cannot answer or approve a need.",
            "Could not check: " + (", ".join(v.replace("_", " ") for v in unknown) or "other unobserved needs") + "."], rows, {"need_ids": [row["id"] for row in data]}


def render(raw, snapshot, command, args, clock=None):
    """Return bounded speech and closed, nonspoken continuation/ref metadata."""
    try:
        request(command, args)
        now = int((clock or (lambda: time.time() * 1000))())
        require(number(now) and ("ship" not in args or allowed_ship(args["ship"], snapshot)))
        c = raw["coverage"]
        facet = {"ship_jobs": "jobs", "job_inspect": "jobs", "ship_work": "work",
                 "machine_capacity": "capacity", "runtime_needs": "needs"}[command]
        require(c["schema"] == "coverage.v1" and c["facet"] == facet
                and c["ship"] == args.get("ship") and handle(c["machine"], 128)
                and c["scope"] == ("selected-ship" if "ship" in args else "selected-machine")
                and c["other_machines"] == "not_requested" and type(c["complete"]) is bool
                and c["state"] in ("covered", "unavailable", "unsupported", "reverse_only")
                and c["mode"] in ("local_live", "remote_live", "unavailable"))
        if command == "machine_capacity":
            require(c["machine"] == args["machine"])
        if c["state"] != "covered":
            return (UNAVAILABLE + (" Unsupported read direction / incomplete feature." if c["state"] == "reverse_only" else "")), {}
        require(c["mode"] != "unavailable" and raw["writes"] == "none"
                and raw["cache_writes"] == "bounded_registry_continuations_only")
        header = [f"Recorded {c['machine']}/{c['ship'] or 'machine'} {facet} coverage: "
                  + ("complete for selection" if c["complete"] else "incomplete; could not check all") + ", " + c["mode"]
                  + "; " + stamp(c["observed_at"], None, "observation", now) + "."]
        if command in ("ship_jobs", "job_inspect", "ship_work"):
            lines, rows, metadata = inspection(raw, command, args, now)
        else:
            require(raw["schema"] == "capacity-needs.v1" and raw["action_authority"] == "none"
                    and c["mail_complete"] is False)
            lines, rows, metadata = (capacity(raw, snapshot, args, now) if command == "machine_capacity" else needs(raw, args, now))
        require(type(raw["more"]) is bool and (raw["continuation"] is None or token(raw["continuation"])))
        require(raw["more"] == (raw["continuation"] is not None))
        metadata.update(more=raw["more"], continuation=raw["continuation"])
        header += ["Progress or question text is withheld; park-next and schedule-reason text also withheld.",
                   "More recorded pages available; this page is not a total." if raw["more"] else
                   "No further page in this scan; this is not a complete history or all-clear."]
        # Omission notice precedes counts; metadata retains exact references for all page rows.
        for spoken in range(min(len(rows), LIMIT), -1, -1):
            omissions = [f"{len(rows) - spoken} metadata rows not read out; spoken detail incomplete."] if spoken < len(rows) else []
            text = " ".join(header + omissions + lines + rows[:spoken] + [MAIL])
            if len(text) <= SPEECH_BUDGET:
                return text, metadata
        return UNAVAILABLE, {}
    except (KeyError, TypeError, ValueError, RecursionError):
        return UNAVAILABLE, {}


def tool_metadata(metadata):
    return {"type": "inputText", "text": "Read-only opaque metadata for follow-up tools only; do not read aloud: "
            + json.dumps(metadata, separators=(",", ":"), sort_keys=True)}
