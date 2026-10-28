"""Fixed scoped inventory; no arbitrary core command or argument forwarding."""
import sys
sys.dont_write_bytecode = True
import json
import mcp_types as types
from file_transfer import BASE64_CHARS


def schema(properties=None, required=()):
    return {"type": "object", "properties": properties or {},
            "required": list(required), "additionalProperties": False}


STRING = {"type": "string", "minLength": 1, "maxLength": 4096}
BODY = {"type": "string", "minLength": 1, "maxLength": 32000}
BODY_MAX_BYTES = 48000  # Leave 17536 bytes for mail metadata and delegated envelopes.
BODY_LIMIT = (" Body: 1–32000 characters and at most 48000 bytes of UTF-8, "
              "also at most 48000 bytes after JSON escaping (excluding quotes). "
              "Exceeding a body budget returns invalid_arguments (field body, byte-limit rule). "
              "The core's complete mail/envelope limit is 65536 bytes; unusually large grant metadata can still exceed it. "
              "HTTP defaults to 65536 bytes for the whole request including JSON escaping; exceeding it returns HTTP 413 "
              "before tool validation. Raising HTTP limits does not raise body or core limits.")
SHIP = {"type": "string", "minLength": 1, "maxLength": 128}
PAGE = {"limit": {"type": "integer", "minimum": 1, "maximum": 50},
        "continuation": STRING}
KEY = {"idempotency_key": {"type": "string", "minLength": 1, "maxLength": 128,
                           "pattern": r"^[\x20-\x7e]+$(?![\s\S])",
                           "description": "1–128 printable ASCII bytes; no control characters."}}
WRITES = {"send_message", "reply", "ask_liaison", "dismiss", "away", "send_file"}
MAIL_COVERAGE = (" mail_delivery.summary/warning, when present, describes this door's local coverage. "
                 "Reader coverage describes only the selected mailbox (which may be on a peer), not the fleet. "
                 "Mail held only at another machine's captain desk is not included in this door's local coverage. "
                 "coverage.scope says what was read; complete:true is never fleet-wide or proof the inbox is drained.")
PAGING = (" Error continuation_invalid: the token expired (24 h), was evicted or is not this reader's; list again without one. "
          "Served refs beyond the grant's 256-row window are evicted oldest-unacknowledged first, so a refused ack means a stale ref: "
          "list again for fresh ones. Error reader_window_full: acknowledge or list earlier inbox mail.")
# Results whose coverage the door renders, never upgrading what the core reported.
COVERED = {"fleet_waiting", "mail_inbox", "mail_show", "mail_thread", "mail_ack", "get_report",
           "ship_jobs", "job_inspect", "ship_work", "machine_capacity", "runtime_needs"}
COVERAGE_SCHEMA = "coverage.v1"
COVERAGE_NOTE = "Coverage partly unreported; not a fleet-wide all-clear."
RUNTIME = {"schema": {"type": "string", "enum": ["runtime.v1"]}, "machine": SHIP, "ship": SHIP}
RUNTIME_SCHEMA = {**schema(RUNTIME), "dependentRequired": {"machine": ["schema"], "ship": ["schema"]}}
RUNTIME_NOTE = (" Opt in with schema:runtime.v1 for safe metadata and coverage.v1 runtime-domain facets. "
                "Selectors require that opt-in and resolve only allowed directory handles (local for this core). "
                "Callable linked cores supply bounded runtime metadata; reused, cached and carried facts retain contact provenance. "
                "Carried/cache facts never count as current. False requires complete fresh local negative evidence, otherwise unknown. "
                "Recorded job states are not active workers; truncated state totals are partial. No authored text, worker census, "
                "mail-complete claim or work/presence/mail writes. Only a bounded process-memory observation cache is updated.")
JOB_STATES = ["starting", "queued", "launching", "running", "parked", "orphaned",
              "succeeded", "failed", "cancelled", "timed_out", "resumed", "completed"]
JOB_REF = schema({k: {"type": "string", "minLength": 1, "maxLength": 160}
                  for k in ("origin", "incarnation", "ship", "task_id", "id")},
                 ["origin", "incarnation", "ship", "task_id", "id"])
TEXT_OPT = {"include_text": {"type": "boolean", "description": "Opt in to masked, bounded authored text; also requires mail:read. Otherwise text is withheld."}}
INSPECTION_NOTE = (" Selected local or callable linked ship only. Metadata needs fleet:read; authored text is opt-in and also needs mail:read. "
                   "Recorded states are not current liveness or active-worker counts. No argv, paths, logs, screens, reconciliation or work writes. "
                   "Pages default to 25, max 50, with byte/scan/time budgets; counts identify their authorized scan window and completeness. "
                   "Continuations expire after 10 minutes, are source/grant/filter/visibility-revision bound, and may be evicted or invalidated on source/boot change; "
                   "on continuation_invalid start again. Terminal history is retained-only; earlier gaps are unknown. Resumed is lineage, not success.")
# name -> (one core command, scope, schema, description)
TOOLS = {
    "fleet_ships": ("ships", "fleet:read", RUNTIME_SCHEMA,
                    "Visible local and directory ship inventory; runtime opt-in also reads callable linked cores." + RUNTIME_NOTE),
    "fleet_status": ("status", "fleet:read", RUNTIME_SCHEMA,
                     "Legacy local status unchanged without opt-in; no remote live query." + RUNTIME_NOTE),
    "ship_jobs": ("ship_jobs", "fleet:read", schema({"ship": SHIP, **PAGE, **TEXT_OPT,
        "state": {"type": "string", "enum": JOB_STATES},
        "kind": {"type": "string", "enum": ["first_mate", "crew", "errand", "command"]}}, ["ship"]),
        "Read safe live and retained terminal job rows and state counts. completed groups succeeded/failed/cancelled/timed_out/resumed." + INSPECTION_NOTE),
    "job_inspect": ("job_inspect", "fleet:read", schema({"ship": SHIP, "reference": JOB_REF, **TEXT_OPT}, ["ship", "reference"]),
        "Inspect the exact origin/incarnation/custody-bound reference returned by ship_jobs, with safe lineage, wait, timestamps and last progress/error category." + INSPECTION_NOTE),
    "ship_work": ("ship_work", "fleet:read", schema({"ship": SHIP, **PAGE, **TEXT_OPT}, ["ship"]),
        "Read scheduled and unscheduled tasks with separate state, scheduling and backlog counts. Open tasks are backlog, never workers." + INSPECTION_NOTE),
    "machine_capacity": ("machine_capacity", "fleet:read", schema({"machine": SHIP, "ship": SHIP, "task_id": SHIP}, ["machine"]),
        "Read one allowed machine's capacity ledger (local for this core), optionally selected ship/task effective policy. "
        "Heavy-slot headroom only, not free agents or a launch promise. Null known caps are unbounded by that policy. "
        "Named-ship grants must select a ship and see only its holds/queue; shared limits, totals, readings and routes are withheld even if the named list covers every ship. "
        "Queue blockers are last head classifications with timestamps, not fresh per-waiter diagnoses. No configure, recovery, pruning, probing or route changes."),
    "runtime_needs": ("runtime_needs", "fleet:read", schema({"ship": SHIP, **PAGE, **TEXT_OPT}, ["ship"]),
        "Read typed blockers and captain ask metadata for a selected allowed local or callable ship. "
        "Question and choices additionally require include_text and mail:read. No approval argv/cwd or action authority. "
        "Aggregate mail coverage stays partial, never an all-clear. Continuations are grant/filter/source/boot bound for 10 minutes; on continuation_invalid restart listing."),
    "fleet_waiting": ("waiting", "fleet:read", schema(),
                      "Outstanding asks and decisions in this machine's captain mailboxes only; not a fleet-wide all-clear." + MAIL_COVERAGE),
    "away_status": ("away", "fleet:read", schema(), "Read away metadata without changing presence."),
    "mail_inbox": ("inbox", "mail:read", schema({"ship": SHIP, **PAGE}, ["ship"]),
                   "Page incoming captain mail in the selected local or reachable linked mailbox; listing does not acknowledge." + MAIL_COVERAGE + PAGING),
    "mail_show": ("show", "mail:read", schema({"ship": SHIP, "id": STRING}, ["ship", "id"]),
                  "Show one exact stable mail ID in a captain conversation, including outgoing mail delivery/state. "
                  "A stored/delivered message is not completed work. Reverse-link queued envelopes are not mailbox rows." + MAIL_COVERAGE),
    "mail_thread": ("thread", "mail:read", schema({"ship": SHIP, "id": STRING, **PAGE}, ["ship", "id"]),
                    "Page one selected mailbox's correlation thread, with explicit continuation and coverage." + MAIL_COVERAGE + PAGING),
    "send_message": ("send", "ship:message", schema({"to": SHIP, "body": BODY,
        "kind": {"type": "string", "enum": ["request", "notice"]},
        "urgency": {"type": "string", "enum": ["low", "normal", "high"]}, **KEY}, ["to", "body", "kind"]),
        "Send a fleet request or notice. Returns a message ID and delivery state, never completed work. "
        "For route reverse_collect, read delivery_summary: queued on this machine for ship on machine, "
        "not delivered until that machine's dispatcher collects it with delegated-send support. "
        "An outcome notice goes to the captain's conversation on this machine. Only if outcome_notice_ship is returned, "
        "watch mail_inbox there; otherwise this grant cannot read that notice. The queued ID is not a mail_show row. "
        "Use idempotency_key for durable retries; without it each call is a new send. "
        "Accepted fields: to, body, kind (required: request or notice); urgency, idempotency_key (optional). "
        "All extra fields are refused, including task_id and correlation; use reply for correlation." + BODY_LIMIT),
    "reply": ("send", "ship:message", schema({"correlation": STRING, "body": BODY, **KEY}, ["correlation", "body"]),
        "Reply to an exact stored ask at the door's normal recipient. Approval-bearing asks also need approval:answer; "
        "production/privileged answers remain refused. Optional idempotency_key prevents repeat writes. "
        "Accepted fields: correlation, body (required); idempotency_key (optional). All extra fields are refused; kind is implied." + BODY_LIMIT),
    "ask_liaison": ("send", "ship:message", schema({"body": BODY, **KEY}, ["body"]),
        "Send a delegated request to the door default, otherwise the one liaison, otherwise the sole local ship. "
        "Ambiguous or forbidden recipients fail closed. No additional relay/approval authority. Use idempotency_key for retries. "
        "Accepted fields: body (required); idempotency_key (optional). All extra fields are refused, including to and kind; "
        "recipient and request kind are implied. Use send_message for an explicit recipient." + BODY_LIMIT),
    "dismiss": ("dismiss", "work:manage", schema({"id": STRING, **KEY}, ["id"]),
        "Dismiss visible work by ID; production/privileged approvals remain refused. Optional idempotency_key prevents repeat writes. "
        "Accepted fields: id (required); idempotency_key (optional). All extra fields are refused."),
    "away": ("away", "captain:presence", schema({"action": {"type": "string", "enum": ["start", "back"]}, **KEY}, ["action"]),
        "Explicitly enter or leave fleet-wide away mode, including routing/catch-up side effects. "
        "Use away_status to read presence and idempotency_key for retries. "
        "Accepted fields: action (required: start or back); idempotency_key (optional). All extra fields are refused."),
    "send_file": ("file.put", "file:transfer", schema({"to": SHIP,
        "name": {"type": "string", "minLength": 1, "maxLength": 128},
        "content": {"type": "string", "maxLength": BASE64_CHARS}, **KEY}, ["to", "name", "content"]),
        "Store at most 8 MiB in an allowed ship's home. content is canonical padded base64 (at most 11184812 characters). "
        "name is one component, 1–128 UTF-8 bytes, no leading dot, slash, backslash or controls. "
        "Returns file_id and delivery_state stored: NOT notified, NOT completed work. "
        "Then use send_message naming the file_id; the recipient runs file.describe {file_id} to find and read it. "
        "No body/note or paths accepted. Optional idempotency_key gives durable content-bound retries. "
        "Staging: two active files / 16 MiB per grant, 64 MiB globally. HTTP upload envelope: 11250348 bytes."),
    "get_report": ("report.read", "mail:read", schema({"report_id": STRING}, ["report_id"]),
        "Read a stored report referenced by incoming captain mail from an allowed ship in an allowed local active home. "
        "At most 256 KiB UTF-8 content; HTML is untrusted. Local only: remote report coverage unavailable. "
        "Does not acknowledge mail or advance any cursor. No host paths."),
    "mail_ack": ("ack", "mail:read", schema({"ship": SHIP, "refs": {
        "type": "array", "items": STRING, "minItems": 1, "maxItems": 64, "uniqueItems": True}, **KEY}, ["refs"]),
                 "Acknowledge served refs for this grant only, never the dashboard or another reader. "
                 "Omit ship only for already served digest refs. Optional idempotency_key preserves the original acknowledgment result. "
                 "Accepted fields: refs (required); ship, idempotency_key (optional). All extra fields are refused."),
}


def tool(name):
    _, _, inputs, description = TOOLS[name]
    return types.Tool(name=name, title=name.replace("_", " ").title(),
                      description=description, input_schema=inputs,
                      annotations=types.ToolAnnotations(read_only_hint=name not in WRITES | {"mail_ack"},
                                                        destructive_hint=name in {"dismiss", "away"},
                                                        idempotent_hint=name not in WRITES | {"mail_ack"},
                                                        open_world_hint=False))


def result(value, error=False):
    return types.CallToolResult(content=[types.TextContent(type="text", text=json.dumps(value))],
                                structured_content=value, is_error=error)


def render(name, value):
    """Coverage as the core reported it. An omitted coverage, completeness or
    other_machines field reads as incomplete or unknown, never as covered; an
    unversioned desk-domain claim is not complete."""
    covered = name in COVERED or (name in {"fleet_ships", "fleet_status"}
                                  and isinstance(value, dict) and value.get("schema") == "runtime.v1")
    if not covered or not isinstance(value, dict) or "error" in value:
        return value
    reported = value.get("coverage")
    coverage = dict(reported) if isinstance(reported, dict) else {}
    filled = not coverage
    if not isinstance(coverage.get("scope"), str):
        coverage["scope"], coverage["complete"], filled = "unreported", False, True
    if not isinstance(coverage.get("complete"), bool) or (
            coverage["scope"] == "desk-domain" and coverage.get("schema") != COVERAGE_SCHEMA):
        coverage["complete"], filled = False, True
    if "other_machines" not in coverage and not (
            coverage.get("schema") == COVERAGE_SCHEMA and coverage["scope"] == "operation-only"):
        coverage["other_machines"], filled = "unknown", True
    if filled:
        # Repair is rendering, not schema validation; do not mislabel a malformed object.
        coverage.pop("schema", None)
        coverage["complete"] = False
        coverage["note"] = COVERAGE_NOTE
    return {**value, "coverage": coverage}
