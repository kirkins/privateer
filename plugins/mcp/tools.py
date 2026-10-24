"""Fixed read-only inventory; no arbitrary core command or argument forwarding."""
import json
import mcp_types as types


def schema(properties=None, required=()):
    return {"type": "object", "properties": properties or {},
            "required": list(required), "additionalProperties": False}


STRING = {"type": "string", "minLength": 1, "maxLength": 4096}
SHIP = {"type": "string", "minLength": 1, "maxLength": 128}
PAGE = {"limit": {"type": "integer", "minimum": 1, "maximum": 50},
        "continuation": STRING}
# name -> (one core command, scope, schema, description)
TOOLS = {
    "fleet_ships": ("ships", "fleet:read", schema(),
                    "Visible local and cached linked ship inventory; not remote live status."),
    "fleet_status": ("status", "fleet:read", schema(),
                     "Visible local status and delivery coverage; no remote live status query."),
    "fleet_waiting": ("waiting", "fleet:read", schema(),
                      "Visible local outstanding asks and decisions; not remote desk coverage."),
    "away_status": ("away", "fleet:read", schema(), "Read away metadata without changing presence."),
    "mail_inbox": ("inbox", "mail:read", schema({"ship": SHIP, **PAGE}, ["ship"]),
                   "Page local incoming captain mail; listing does not acknowledge. Preserve coverage."),
    "mail_show": ("show", "mail:read", schema({"ship": SHIP, "id": STRING}, ["ship", "id"]),
                  "Show one exact stable mail ID in a local captain conversation."),
    "mail_thread": ("thread", "mail:read", schema({"ship": SHIP, "id": STRING, **PAGE}, ["ship", "id"]),
                    "Page one local correlation thread, with explicit continuation and coverage."),
    "mail_ack": ("ack", "mail:read", schema({"ship": SHIP, "refs": {
        "type": "array", "items": STRING, "minItems": 1, "maxItems": 64, "uniqueItems": True}}, ["refs"]),
                 "Acknowledge served refs for this grant only, never the dashboard or another reader. "
                 "Omit ship only for already served digest refs."),
}


def tool(name):
    _, _, inputs, description = TOOLS[name]
    return types.Tool(name=name, description=description, input_schema=inputs,
                      annotations=types.ToolAnnotations(read_only_hint=name != "mail_ack",
                                                        destructive_hint=False,
                                                        open_world_hint=False))


def result(value, error=False):
    return types.CallToolResult(content=[types.TextContent(type="text", text=json.dumps(value))],
                                structured_content=value, is_error=error)
