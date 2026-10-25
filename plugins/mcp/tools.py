"""Fixed scoped inventory; no arbitrary core command or argument forwarding."""
import sys
sys.dont_write_bytecode = True
import json
import mcp_types as types


def schema(properties=None, required=()):
    return {"type": "object", "properties": properties or {},
            "required": list(required), "additionalProperties": False}


STRING = {"type": "string", "minLength": 1, "maxLength": 4096}
SHIP = {"type": "string", "minLength": 1, "maxLength": 128}
PAGE = {"limit": {"type": "integer", "minimum": 1, "maximum": 50},
        "continuation": STRING}
KEY = {"idempotency_key": {"type": "string", "minLength": 1, "maxLength": 128,
                           "pattern": r"^[\x20-\x7e]+$(?![\s\S])",
                           "description": "1–128 printable ASCII bytes; no control characters."}}
WRITES = {"send_message", "reply", "ask_liaison", "dismiss", "away"}
MAIL_COVERAGE = (" mail_delivery.summary/warning, when present, describes this door's local coverage. "
                 "Reader coverage describes only the selected mailbox (which may be on a peer), not the fleet. "
                 "Mail held only at another machine's captain desk is not included in this door's local coverage.")
# name -> (one core command, scope, schema, description)
TOOLS = {
    "fleet_ships": ("ships", "fleet:read", schema(),
                    "Visible local and cached linked ship inventory; not remote live status."),
    "fleet_status": ("status", "fleet:read", schema(),
                     "Visible local status and delivery coverage; no remote live status query."),
    "fleet_waiting": ("waiting", "fleet:read", schema(),
                      "Outstanding asks and decisions in this machine's captain mailboxes only; not a fleet-wide all-clear." + MAIL_COVERAGE),
    "away_status": ("away", "fleet:read", schema(), "Read away metadata without changing presence."),
    "mail_inbox": ("inbox", "mail:read", schema({"ship": SHIP, **PAGE}, ["ship"]),
                   "Page incoming captain mail in the selected local or reachable linked mailbox; listing does not acknowledge." + MAIL_COVERAGE),
    "mail_show": ("show", "mail:read", schema({"ship": SHIP, "id": STRING}, ["ship", "id"]),
                  "Show one exact stable mail ID in a captain conversation, including outgoing mail delivery/state. "
                  "A stored/delivered message is not completed work. Reverse-link queued envelopes are not mailbox rows." + MAIL_COVERAGE),
    "mail_thread": ("thread", "mail:read", schema({"ship": SHIP, "id": STRING, **PAGE}, ["ship", "id"]),
                    "Page one selected mailbox's correlation thread, with explicit continuation and coverage." + MAIL_COVERAGE),
    "send_message": ("send", "ship:message", schema({"to": SHIP, "body": STRING,
        "kind": {"type": "string", "enum": ["request", "notice"]},
        "urgency": {"type": "string", "enum": ["low", "normal", "high"]}, **KEY}, ["to", "body", "kind"]),
        "Send a fleet request or notice. Returns a message ID and delivery state, never completed work. "
        "For route reverse_collect, read delivery_summary: queued on this machine for ship on machine, "
        "not delivered until that machine's dispatcher collects it with delegated-send support. "
        "An outcome notice goes to the captain's conversation on this machine. Only if outcome_notice_ship is returned, "
        "watch mail_inbox there; otherwise this grant cannot read that notice. The queued ID is not a mail_show row. "
        "Use idempotency_key for durable retries; without it each call is a new send."),
    "reply": ("send", "ship:message", schema({"correlation": STRING, "body": STRING, **KEY}, ["correlation", "body"]),
        "Reply to an exact stored ask at the door's normal recipient. Approval-bearing asks also need approval:answer; "
        "production/privileged answers remain refused. Optional idempotency_key prevents repeat writes."),
    "ask_liaison": ("send", "ship:message", schema({"body": STRING, **KEY}, ["body"]),
        "Send a delegated request to the door default, otherwise the one liaison, otherwise the sole local ship. "
        "Ambiguous or forbidden recipients fail closed. No additional relay/approval authority. Use idempotency_key for retries."),
    "dismiss": ("dismiss", "work:manage", schema({"id": STRING, **KEY}, ["id"]),
        "Dismiss visible work by ID; production/privileged approvals remain refused. Optional idempotency_key prevents repeat writes."),
    "away": ("away", "captain:presence", schema({"action": {"type": "string", "enum": ["start", "back"]}, **KEY}, ["action"]),
        "Explicitly enter or leave fleet-wide away mode, including routing/catch-up side effects. "
        "Use away_status to read presence and idempotency_key for retries."),
    "mail_ack": ("ack", "mail:read", schema({"ship": SHIP, "refs": {
        "type": "array", "items": STRING, "minItems": 1, "maxItems": 64, "uniqueItems": True}, **KEY}, ["refs"]),
                 "Acknowledge served refs for this grant only, never the dashboard or another reader. "
                 "Omit ship only for already served digest refs. Optional idempotency_key preserves the original acknowledgment result."),
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
