//! Scoped door v2 admission. This module never dispatches as an operator.
//! The command enum is shared with v1 dispatch: both execution and scope
//! classification are exhaustive matches, so adding a command needs both.
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Command {
    Send,
    Ships,
    Status,
    Waiting,
    Held,
    Away,
    Digest,
    Dismiss,
    FilePut,
    Approval,
    Permissions,
    Stow,
    Upgrade,
}

impl Command {
    const ALL: &'static [(Self, &'static str)] = &[
        (Self::Send, "send"),
        (Self::Ships, "ships"),
        (Self::Status, "status"),
        (Self::Waiting, "waiting"),
        (Self::Held, "held"),
        (Self::Away, "away"),
        (Self::Digest, "digest"),
        (Self::Dismiss, "dismiss"),
        (Self::FilePut, "file.put"),
        (Self::Approval, "approval"),
        (Self::Permissions, "permissions"),
        (Self::Stow, "stow"),
        (Self::Upgrade, "upgrade"),
    ];

    pub(super) fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Denial {
    Envelope,
    Protocol,
    Identity,
    UnknownCommand,
    Arguments,
    MissingCredential,
    InvalidCredential,
    ExecutionUnavailable,
}

impl Denial {
    /// Internal audit only. Never include credentials or untrusted arguments.
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::Envelope => "invalid_envelope",
            Self::Protocol => "protocol_mismatch",
            Self::Identity => "reserved_identity",
            Self::UnknownCommand => "unknown_command",
            Self::Arguments => "unsupported_arguments",
            Self::MissingCredential => "missing_credential",
            Self::InvalidCredential => "invalid_credential",
            Self::ExecutionUnavailable => "delegated_execution_unavailable",
        }
    }
}

type Admission<T> = Result<T, Denial>;

/// Refuse identity/provenance claims at any nesting depth, including payloads.
/// Bound traversal before descending into client-controlled JSON.
fn identity_claim(value: &Value, depth: usize) -> bool {
    if depth > 64 {
        return true;
    }
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "from"
                    | "actor"
                    | "operator"
                    | "captain"
                    | "grant"
                    | "liaison"
                    | "provenance"
                    | "delegated_context"
            ) || identity_claim(value, depth + 1)
        }),
        Value::Array(values) => values.iter().any(|v| identity_claim(v, depth + 1)),
        _ => false,
    }
}

fn shape(args: &Value, strings: &[&str], required: &[&str], extra: &[&str]) -> Admission<()> {
    let fields = args.as_object().ok_or(Denial::Arguments)?;
    if fields
        .keys()
        .any(|k| !strings.contains(&k.as_str()) && !extra.contains(&k.as_str()))
        || required.iter().any(|k| !args[*k].is_string())
        || strings
            .iter()
            .any(|k| fields.get(*k).is_some_and(|v| !v.is_string()))
    {
        return Err(Denial::Arguments);
    }
    Ok(())
}

fn action<'a>(args: &'a Value, default: &'a str) -> &'a str {
    args.get("action")
        .and_then(Value::as_str)
        .unwrap_or(default)
}

/// The single core command/argument scope table. These are *requirements*,
/// not permission to execute: target filtering, stored-request reply
/// classification, delegated provenance and confirmations are still required.
/// Until stored-request classification exists, every reply conservatively
/// requires approval:answer. Sensitive answers remain execution-disabled.
fn requirements(command: Command, args: &Value) -> Admission<&'static [&'static str]> {
    use Command::*;
    match command {
        Ships | Status | Waiting => {
            shape(args, &[], &[], &[])?;
            Ok(&["fleet:read"])
        }
        Held => {
            shape(args, &[], &[], &[])?;
            Ok(&["mail:read"])
        }
        Digest => {
            shape(args, &[], &[], &[])?;
            Ok(&["captain:presence", "mail:read"])
        }
        Away => match action(args, "status") {
            "status" => {
                shape(args, &["action"], &[], &[])?;
                Ok(&["fleet:read"])
            }
            "start" => {
                shape(args, &["action", "words"], &[], &[])?;
                Ok(&["captain:presence"])
            }
            "back" => {
                shape(args, &["action"], &[], &[])?;
                Ok(&["captain:presence"])
            }
            _ => Err(Denial::Arguments),
        },
        Send => {
            shape(
                args,
                &["to", "kind", "id", "body", "correlation", "urgency"],
                &["body"],
                &["payload", "reply_within_secs"],
            )?;
            if args.get("reply_within_secs").is_some_and(|v| {
                v.as_u64().is_none_or(|n| !(1..=2_592_000).contains(&n))
            })
                || args.get("urgency").is_some_and(|v| !matches!(v.as_str(), Some("low" | "normal" | "high")))
                // Payload variants need a separate provenance/approval review
                // in 2.3; no arbitrary nested dispatch or approval evidence.
                || args.get("payload").is_some_and(|v| v.as_object().is_none_or(|p| !p.is_empty()))
            {
                return Err(Denial::Arguments);
            }
            match args
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("request")
            {
                "request" => Ok(&["ship:message"]),
                "notice" if args.get("reply_within_secs").is_none() => Ok(&["ship:message"]),
                "reply"
                    if args["correlation"].is_string()
                        && args.get("reply_within_secs").is_none() =>
                {
                    Ok(&["ship:message", "approval:answer"])
                }
                _ => Err(Denial::Arguments),
            }
        }
        Dismiss => {
            shape(args, &["id", "note"], &["id"], &[])?;
            Ok(&["work:manage"])
        }
        FilePut => {
            shape(
                args,
                &["to", "name", "content_file"],
                &["to", "name", "content_file"],
                &[],
            )?;
            Ok(&["file:transfer"])
        }
        Approval => {
            shape(
                args,
                &["id", "action", "hash"],
                &["id", "action", "hash"],
                &[],
            )?;
            if !matches!(args["action"].as_str(), Some("grant" | "deny")) {
                return Err(Denial::Arguments);
            }
            Ok(&["approval:answer"])
        }
        Permissions => {
            shape(args, &["mode"], &[], &[])?;
            match args.get("mode") {
                None => Ok(&["fleet:read"]),
                Some(mode) if matches!(mode.as_str(), Some("standard" | "yolo")) => {
                    Ok(&["fleet:admin"])
                }
                _ => Err(Denial::Arguments),
            }
        }
        Stow => {
            shape(args, &["action", "machine"], &[], &["all"])?;
            if args.get("all").is_some_and(|v| !v.is_boolean())
                || (args.get("machine").is_some() && args["all"] == true)
            {
                return Err(Denial::Arguments);
            }
            match action(args, "status") {
                "status" => Ok(&["fleet:read"]),
                "start" | "resume" => Ok(&["fleet:admin"]),
                _ => Err(Denial::Arguments),
            }
        }
        Upgrade => {
            match action(args, "plan") {
                "plan" => shape(args, &["action", "commit"], &["commit"], &[])?,
                "confirm" => shape(args, &["action", "confirmation"], &["confirmation"], &[])?,
                _ => return Err(Denial::Arguments),
            }
            Ok(&["fleet:admin"])
        }
    }
}

/// Phase 2.2 integration point. Resolve an opaque token in core-owned state,
/// bound to this registered door/resource, and check the *current* grant and
/// these requirements. Never return client-supplied identity as context.
/// No credential is accepted in phase 2.1 (including empty or malformed ones).
fn check_credential(_door: &str, _token: &str, _required: &[&str]) -> Admission<DelegatedContext> {
    Err(Denial::InvalidCredential)
}

/// Only the credential resolver may construct core-owned delegated provenance.
/// Successful token lookup must never call trusted v1 dispatch.
struct DelegatedContext {
    _core_owned: (),
}

/// The only pre-auth request is `protocol {}`: fixed public metadata, no fleet
/// inventory, recipient, door configuration, mail, or implicit state writes.
pub(super) fn request(door: &str, message: &Value) -> Admission<Value> {
    if identity_claim(message, 0) {
        return Err(Denial::Identity);
    }
    if message.get("version").is_some_and(|v| *v != 2) {
        return Err(Denial::Protocol);
    }
    let fields = message.as_object().ok_or(Denial::Envelope)?;
    if fields
        .keys()
        .any(|k| !["type", "id", "command", "args", "auth", "version"].contains(&k.as_str()))
        || message["type"] != "request"
        || !(message["id"].is_string() || message["id"].is_number())
    {
        return Err(Denial::Envelope);
    }
    let name = message["command"].as_str().ok_or(Denial::UnknownCommand)?;
    let args = message.get("args").cloned().unwrap_or_else(|| json!({}));
    if name == "protocol" {
        shape(&args, &[], &[], &[])?;
        if message.get("auth").is_some() {
            return Err(Denial::Envelope);
        }
        return Ok(json!({"version": 2}));
    }
    let command = Command::parse(name).ok_or(Denial::UnknownCommand)?;
    let required = requirements(command, &args)?;
    let auth = message.get("auth").ok_or(Denial::MissingCredential)?;
    let fields = auth.as_object().ok_or(Denial::InvalidCredential)?;
    if fields.len() != 1 {
        return Err(Denial::InvalidCredential);
    }
    let token = auth["token"].as_str().ok_or(Denial::InvalidCredential)?;
    let _context = check_credential(door, token, required)?;
    Err(Denial::ExecutionUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dispatched_command_has_a_scope_and_rejects_extra_arguments() {
        let cases = [
            ("send", json!({"body":"hi"}), vec!["ship:message"]),
            ("ships", json!({}), vec!["fleet:read"]),
            ("status", json!({}), vec!["fleet:read"]),
            ("waiting", json!({}), vec!["fleet:read"]),
            ("held", json!({}), vec!["mail:read"]),
            ("away", json!({}), vec!["fleet:read"]),
            ("digest", json!({}), vec!["captain:presence", "mail:read"]),
            ("dismiss", json!({"id":"r"}), vec!["work:manage"]),
            (
                "file.put",
                json!({"to":"ship", "name":"file", "content_file":"file"}),
                vec!["file:transfer"],
            ),
            (
                "approval",
                json!({"id":"r","action":"deny","hash":"12345678"}),
                vec!["approval:answer"],
            ),
            ("permissions", json!({}), vec!["fleet:read"]),
            ("stow", json!({}), vec!["fleet:read"]),
            ("upgrade", json!({"commit":"abc"}), vec!["fleet:admin"]),
        ];
        assert_eq!(cases.len(), Command::ALL.len());
        for (name, args, scopes) in cases {
            let command = Command::parse(name).unwrap();
            assert_eq!(requirements(command, &args).unwrap(), scopes);
            let mut message = json!({"type":"request", "id":"r", "command":name, "args":args});
            assert_eq!(request("fixture", &message), Err(Denial::MissingCredential));
            message["auth"] = json!({"token":"not-a-credential"});
            assert_eq!(request("fixture", &message), Err(Denial::InvalidCredential));
            let mut extra = args;
            extra["unknown"] = json!(true);
            assert_eq!(requirements(command, &extra), Err(Denial::Arguments));
            assert_eq!(requirements(command, &Value::Null), Err(Denial::Arguments));
        }
    }

    #[test]
    fn argument_dependent_scopes_and_variants_fail_closed() {
        for (name, args, scopes) in [
            (
                "away",
                json!({"action":"start","words":"out"}),
                vec!["captain:presence"],
            ),
            ("away", json!({"action":"back"}), vec!["captain:presence"]),
            ("permissions", json!({"mode":"yolo"}), vec!["fleet:admin"]),
            (
                "stow",
                json!({"action":"resume","all":true}),
                vec!["fleet:admin"],
            ),
            (
                "stow",
                json!({"action":"start","machine":"peer"}),
                vec!["fleet:admin"],
            ),
            (
                "upgrade",
                json!({"action":"confirm","confirmation":"x"}),
                vec!["fleet:admin"],
            ),
            (
                "send",
                json!({"kind":"reply","body":"yes","correlation":"r"}),
                vec!["ship:message", "approval:answer"],
            ),
        ] {
            assert_eq!(
                requirements(Command::parse(name).unwrap(), &args).unwrap(),
                scopes
            );
        }
        for name in ["away", "stow", "upgrade", "approval"] {
            assert!(
                requirements(Command::parse(name).unwrap(), &json!({"action":"other"})).is_err()
            );
        }
        for args in [
            json!({"action":null}),
            json!({"machine":5}),
            json!({"all":"true"}),
        ] {
            assert!(requirements(Command::Stow, &args).is_err());
        }
        assert!(requirements(Command::Send, &json!({"body":"x","kind":"decision"})).is_err());
        assert!(requirements(Command::Send, &json!({"body":"x","kind":"reply"})).is_err());
    }
}
