//! Scoped door v2 admission. Never forwards arbitrary args to operator dispatch.
//! The command enum is shared with v1 dispatch: both execution and scope
//! classification are exhaustive matches, so adding a command needs both.
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Send,
    Capabilities,
    Ships,
    Status,
    Waiting,
    Held,
    Inbox,
    Show,
    Thread,
    Ack,
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
        (Self::Capabilities, "capabilities"),
        (Self::Ships, "ships"),
        (Self::Status, "status"),
        (Self::Waiting, "waiting"),
        (Self::Held, "held"),
        (Self::Inbox, "inbox"),
        (Self::Show, "show"),
        (Self::Thread, "thread"),
        (Self::Ack, "ack"),
        (Self::Away, "away"),
        (Self::Digest, "digest"),
        (Self::Dismiss, "dismiss"),
        (Self::FilePut, "file.put"),
        (Self::Approval, "approval"),
        (Self::Permissions, "permissions"),
        (Self::Stow, "stow"),
        (Self::Upgrade, "upgrade"),
    ];

    pub(crate) fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Denial {
    Envelope,
    Protocol,
    Identity,
    UnknownCommand,
    Arguments,
    MissingCredential,
    InvalidCredential,
    WrongDoor,
    Expired,
    Paused,
    Revoked,
    Scope,
    AuthUnavailable,
    OutcomeUnknown,
    UpgradeUnavailable,
    ConfirmationRequired,
    LinkedUnavailable,
    Ship,
    ExecutionFailed,
}

impl Denial {
    /// Only execution-stage codes can cross an already admitted machine call.
    /// A remote never authenticates the original door credential.
    pub(crate) fn from_code(code: &str) -> Option<Self> {
        use Denial::*;
        [
            Identity,
            Arguments,
            Scope,
            OutcomeUnknown,
            ConfirmationRequired,
            LinkedUnavailable,
            Ship,
            ExecutionFailed,
        ]
        .into_iter()
        .find(|denial| denial.reason() == code)
    }

    /// Internal audit only. Never include credentials or untrusted arguments.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Envelope => "invalid_envelope",
            Self::Protocol => "protocol_mismatch",
            Self::Identity => "reserved_identity",
            Self::UnknownCommand => "unknown_command",
            Self::Arguments => "unsupported_arguments",
            Self::MissingCredential => "missing_credential",
            Self::InvalidCredential => "invalid_credential",
            Self::WrongDoor => "wrong_door",
            Self::Expired => "expired_credential",
            Self::Paused => "paused_grant",
            Self::Revoked => "revoked_grant",
            Self::Scope => "insufficient_scope",
            Self::AuthUnavailable => "auth_store_unavailable",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::UpgradeUnavailable => "grant_bound_upgrade_unavailable",
            Self::ConfirmationRequired => "trusted_confirmation_required",
            Self::LinkedUnavailable => "linked_provenance_unavailable",
            Self::Ship => "ship_not_allowed",
            Self::ExecutionFailed => "execution_failed",
        }
    }
}

impl std::fmt::Display for Denial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}
impl std::error::Error for Denial {}

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
                    | "client"
                    | "client_id"
                    | "grant_id"
                    | "resource"
                    | "owner"
                    | "approver"
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
/// not permission to execute. Mail checks approval authority against the stored
/// request inside its write transaction. Sensitive answers stay disabled.
fn requirements(command: Command, args: &Value) -> Admission<&'static [&'static str]> {
    use Command::*;
    match command {
        Capabilities => {
            shape(args, &[], &[], &[])?;
            Ok(&[])
        }
        Ships | Status | Waiting => {
            shape(args, &[], &[], &[])?;
            Ok(&["fleet:read"])
        }
        Held => {
            shape(args, &[], &[], &[])?;
            Ok(&["mail:read"])
        }
        Inbox | Thread => {
            let required: &[&str] = if command == Thread {
                &["ship", "id"]
            } else {
                &["ship"]
            };
            let strings: &[&str] = if command == Thread {
                &["ship", "id", "continuation"]
            } else {
                &["ship", "continuation"]
            };
            shape(args, strings, required, &["limit"])?;
            if args
                .get("limit")
                .is_some_and(|v| v.as_u64().is_none_or(|n| !(1..=50).contains(&n)))
            {
                return Err(Denial::Arguments);
            }
            Ok(&["mail:read"])
        }
        Show => {
            shape(args, &["ship", "id"], &["ship", "id"], &[])?;
            Ok(&["mail:read"])
        }
        Ack => {
            shape(args, &["ship"], &[], &["refs"])?;
            if args["refs"].as_array().is_none_or(|refs| {
                refs.is_empty() || refs.len() > 64 || refs.iter().any(|r| !r.is_string())
            }) {
                return Err(Denial::Arguments);
            }
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
                    Ok(&["ship:message"])
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

/// Validate a send at the receiving gateway as well as at the door. No
/// runtime identity fields are accepted in the secondary call either.
pub(crate) fn gateway_send(args: &Value, context: &DelegatedContext) -> anyhow::Result<()> {
    if identity_claim(args, 0) {
        return Err(Denial::Identity.into());
    }
    for scope in requirements(Command::Send, args)? {
        if !context.has(scope) {
            return Err(Denial::Scope.into());
        }
    }
    if !args["to"].as_str().is_some_and(|to| context.allows(to)) {
        return Err(Denial::Ship.into());
    }
    Ok(())
}

pub(crate) fn gateway_reader(
    name: &str,
    args: &Value,
    context: &DelegatedContext,
) -> anyhow::Result<Command> {
    let command = Command::parse(name).ok_or(Denial::UnknownCommand)?;
    if !matches!(
        command,
        Command::Inbox | Command::Show | Command::Thread | Command::Ack
    ) {
        return Err(Denial::UnknownCommand.into());
    }
    if identity_claim(args, 0) {
        return Err(Denial::Identity.into());
    }
    for scope in requirements(command, args)? {
        if !context.has(scope) {
            return Err(Denial::Scope.into());
        }
    }
    // This wire carries raw mail only, never remote digest acknowledgments.
    if !args["ship"].as_str().is_some_and(|s| context.allows(s)) {
        return Err(Denial::Ship.into());
    }
    Ok(command)
}

pub(crate) fn gateway_desk(
    name: &str,
    args: &Value,
    context: &DelegatedContext,
) -> anyhow::Result<Command> {
    let command = Command::parse(name).ok_or(Denial::UnknownCommand)?;
    if !matches!(
        command,
        Command::Away | Command::Held | Command::Digest | Command::Ack
    ) || (command == Command::Ack && args.get("ship").is_some())
    {
        return Err(Denial::UnknownCommand.into());
    }
    if identity_claim(args, 0) {
        return Err(Denial::Identity.into());
    }
    for scope in requirements(command, args)? {
        if !context.has(scope) {
            return Err(Denial::Scope.into());
        }
    }
    Ok(command)
}

/// Typed admin gateway variants touch this machine only. No recursive fan-out
/// or additional policy fields are accepted at a receiving gateway.
pub(crate) fn gateway_admin(
    name: &str,
    args: &Value,
    context: &DelegatedContext,
) -> anyhow::Result<()> {
    let command = match name {
        "permissions" => Command::Permissions,
        "stow" => Command::Stow,
        _ => return Err(Denial::UnknownCommand.into()),
    };
    if identity_claim(args, 0) {
        return Err(Denial::Identity.into());
    }
    for scope in requirements(command, args)? {
        if !context.has(scope) {
            return Err(Denial::Scope.into());
        }
    }
    if args.get("machine").is_some() || args.get("all").is_some() {
        return Err(Denial::Arguments.into());
    }
    Ok(())
}

/// Re-read the core-owned grant on every request, bound to this door/resource.
fn check_credential(
    door: &str,
    token: &str,
    required: &[&str],
    resource: Option<&str>,
) -> Admission<DelegatedContext> {
    super::clients::check(door, token, required, resource)
}

/// Immutable core-owned provenance, not deserializable from client claims.
pub(crate) struct DelegatedContext {
    client: String,
    resource: String,
    scopes: Vec<String>,
    ships: Option<Vec<String>>,
    result: String,
    identity: Option<String>,
    application: Option<String>,
}

impl DelegatedContext {
    pub(super) fn resolved(
        client: String,
        resource: String,
        scopes: Vec<String>,
        ships: Option<Vec<String>>,
    ) -> Self {
        Self {
            client,
            resource,
            scopes,
            ships,
            result: uuid::Uuid::new_v4().to_string(),
            identity: None,
            application: None,
        }
    }

    pub(super) fn with_application(mut self, application: Option<String>) -> Self {
        self.application = application;
        self
    }
}

impl DelegatedContext {
    /// Only an authenticated operator machine gateway may decode this wire
    /// context. It is an admitted snapshot, not a bearer token or owner proof.
    pub(crate) fn from_gateway(value: &Value) -> anyhow::Result<Self> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            version: u8,
            client: String,
            resource: String,
            scopes: Vec<String>,
            ships: Option<Vec<String>>,
            result_id: String,
            application: Option<String>,
        }
        let wire: Wire = serde_json::from_value(value.clone())?;
        anyhow::ensure!(wire.version == 1, "unsupported delegated context version");
        uuid::Uuid::parse_str(&wire.client)?;
        uuid::Uuid::parse_str(&wire.result_id)?;
        if let Some(application) = &wire.application {
            uuid::Uuid::parse_str(application)?;
        }
        let door = wire
            .resource
            .strip_prefix("door:")
            .ok_or_else(|| anyhow::anyhow!("invalid delegated resource"))?;
        crate::store::identifier(door)?;
        anyhow::ensure!(
            wire.scopes.len() <= super::clients::SCOPES.len()
                && wire
                    .scopes
                    .iter()
                    .all(|s| super::clients::SCOPES.contains(&s.as_str())),
            "invalid delegated scopes"
        );
        if let Some(ships) = &wire.ships {
            anyhow::ensure!(ships.len() <= 128, "invalid delegated ships");
            for ship in ships {
                crate::store::identifier(ship)?;
            }
        }
        Ok(Self {
            client: wire.client,
            resource: wire.resource,
            scopes: wire.scopes,
            ships: wire.ships,
            result: wire.result_id,
            identity: None,
            application: wire.application,
        })
    }

    pub(crate) fn gateway(&self) -> Value {
        let mut wire = json!({"version":1,"client":self.client,"resource":self.resource,
            "scopes":self.scopes,"ships":self.ships,"result_id":self.result});
        if let Some(application) = &self.application {
            wire["application"] = json!(application);
        }
        wire
    }

    /// Only this admitted client's effective authority, never other clients.
    pub(super) fn capabilities(&self) -> Value {
        json!({"scopes":self.scopes,"ships":self.ships})
    }

    pub(crate) fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    pub(crate) fn allows(&self, ship: &str) -> bool {
        self.ships
            .as_ref()
            .is_none_or(|ships| ships.iter().any(|s| s == ship))
    }

    /// Internal reader namespace only; never use this derived key as authority
    /// or provenance. The authenticated origin core supplied the admitted grant.
    pub(crate) fn reader_context(&self, origin: &str) -> Self {
        Self {
            client: format!("{origin}/{}", self.client),
            resource: self.resource.clone(),
            scopes: self.scopes.clone(),
            ships: self.ships.clone(),
            result: self.result.clone(),
            identity: Some(self.client.clone()),
            application: self.application.clone(),
        }
    }

    pub(crate) fn client(&self) -> &str {
        &self.client
    }

    pub(super) fn resource(&self) -> &str {
        &self.resource
    }

    pub(super) fn ships(&self) -> Option<&[String]> {
        self.ships.as_deref()
    }

    /// Local admission for a new relay. Remote snapshots are not live grants.
    pub(crate) fn recheck_relay(&self) -> Admission<()> {
        super::clients::recheck(&self.client, &self.resource)
    }

    pub(crate) fn provenance(&self) -> Value {
        // Local issuance remains one client / one grant. OAuth keeps a stable
        // application identity across independently revocable consent grants.
        let identity = self.identity.as_deref().unwrap_or(&self.client);
        let application = self.application.as_deref().unwrap_or(identity);
        json!({"client":application, "grant":identity, "resource":self.resource,
            "result_id":self.result, "origin":"scoped-door-v2"})
    }
}

/// Pre-auth requests are fixed protocol metadata and the explicit OAuth
/// bootstrap allowlist. Neither path exposes fleet state.
pub(super) fn request(session: &mut super::Session<'_>, message: &Value) -> Admission<Value> {
    let door = session.door.name.clone();
    let mut audited = false;
    let result = admit(&door, message, &mut audited, |command, args, context| {
        session.delegated(command, args, context)
    });
    if let Err(reason) = result {
        // Separate core sink: adapter stderr can neither forge these rows nor
        // evict them from door.log. Sink failure never permits execution.
        if !audited && super::clients::denial(&door, reason).is_err() {
            return Err(Denial::AuthUnavailable);
        }
    }
    result
}

fn admit(
    door: &str,
    message: &Value,
    audited: &mut bool,
    execute: impl FnOnce(Command, &Value, &DelegatedContext) -> Admission<Value>,
) -> Admission<Value> {
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
    if super::oauth::BOOTSTRAP.contains(&name) {
        if message.get("auth").is_some() {
            return Err(Denial::Envelope);
        }
        *audited = true;
        return Ok(super::oauth::request(door, name, &args));
    }
    // The audience is an authentication binding, not a delegated identity
    // assertion. All other reserved fields, including args.resource, remain
    // forbidden at every depth.
    let mut identity = message.clone();
    if let Some(auth) = identity.get_mut("auth").and_then(Value::as_object_mut) {
        auth.remove("resource");
    }
    if identity_claim(&identity, 0) {
        return Err(Denial::Identity);
    }
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
    if fields
        .keys()
        .any(|k| !["token", "resource"].contains(&k.as_str()))
        || fields.get("resource").is_some_and(|v| !v.is_string())
    {
        return Err(Denial::InvalidCredential);
    }
    let token = auth["token"].as_str().ok_or(Denial::InvalidCredential)?;
    let credential = check_credential(door, token, required, auth["resource"].as_str());
    // Failed credentials already have their single request row. Successful
    // admission is durably audited before effects, then finalized by result ID.
    *audited = !matches!(credential, Err(Denial::AuthUnavailable));
    let context = credential?;
    let result = execute(command, &args, &context);
    let recorded = super::clients::outcome(door, &context, result.as_ref().err().copied());
    finalize(result, &context, recorded)
}

fn finalize(
    result: Admission<Value>,
    context: &DelegatedContext,
    recorded: anyhow::Result<()>,
) -> Admission<Value> {
    // Execution already ran: an outcome-store failure is not failed auth and
    // must never suggest the effect can safely be retried with a fresh id.
    recorded.map_err(|_| Denial::OutcomeUnknown)?;
    result.map(|mut value| {
        value["provenance"] = context.provenance();
        value
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegated_gateway_is_versioned_target_scoped_and_not_client_identity() {
        let context = DelegatedContext::resolved(
            uuid::Uuid::new_v4().to_string(),
            "door:test".into(),
            vec!["ship:message".into()],
            Some(vec!["allowed".into()]),
        );
        let decoded = DelegatedContext::from_gateway(&context.gateway()).unwrap();
        assert_eq!(decoded.provenance(), context.provenance());
        let reader = decoded.reader_context("origin");
        assert_ne!(reader.client(), decoded.client());
        assert_eq!(reader.provenance(), decoded.provenance());
        assert!(
            DelegatedContext::from_gateway(&reader.gateway()).is_err(),
            "internal reader keys cannot be delegated as grant identities"
        );
        assert!(gateway_send(&json!({"to":"allowed","body":"hello"}), &decoded).is_ok());
        assert!(gateway_send(&json!({"to":"hidden","body":"hello"}), &decoded).is_err());
        assert!(
            gateway_send(
                &json!({"to":"allowed","body":"hello","payload":{"provenance":{}}}),
                &decoded
            )
            .is_err()
        );
        let mut wire = context.gateway();
        wire["version"] = json!(2);
        assert!(DelegatedContext::from_gateway(&wire).is_err());
        wire["version"] = json!(1);
        wire["scopes"] = json!(["ship:message", "future:authority"]);
        assert!(DelegatedContext::from_gateway(&wire).is_err());
        wire["scopes"] = json!(["ship:message"]);
        wire["operator"] = json!(true);
        assert!(DelegatedContext::from_gateway(&wire).is_err());
    }

    #[test]
    fn gateway_denial_codes_are_allowlisted() {
        for denial in [
            Denial::Scope,
            Denial::Ship,
            Denial::ConfirmationRequired,
            Denial::OutcomeUnknown,
        ] {
            assert_eq!(Denial::from_code(denial.reason()), Some(denial));
        }
        for denial in [
            Denial::Envelope,
            Denial::Protocol,
            Denial::UnknownCommand,
            Denial::MissingCredential,
            Denial::InvalidCredential,
            Denial::WrongDoor,
            Denial::Expired,
            Denial::Paused,
            Denial::Revoked,
            Denial::AuthUnavailable,
        ] {
            assert_eq!(Denial::from_code(denial.reason()), None);
        }
        assert_eq!(Denial::from_code("future_authority"), None);
        assert_eq!(Denial::from_code("machine remote: outcome_unknown"), None);
    }

    #[test]
    fn post_execution_audit_failure_is_outcome_unknown_not_auth_unavailable() {
        let context = DelegatedContext::resolved("client".into(), "door:test".into(), vec![], None);
        for result in [Ok(json!({"committed":true})), Err(Denial::ExecutionFailed)] {
            let finalized = finalize(
                result,
                &context,
                Err(anyhow::anyhow!("outcome store failed")),
            );
            assert_eq!(finalized, Err(Denial::OutcomeUnknown));
            assert_eq!(finalized.unwrap_err().reason(), "outcome_unknown");
        }
    }

    #[test]
    fn every_dispatched_command_has_a_scope_and_rejects_extra_arguments() {
        let cases = [
            ("send", json!({"body":"hi"}), vec!["ship:message"]),
            ("capabilities", json!({}), vec![]),
            ("ships", json!({}), vec!["fleet:read"]),
            ("status", json!({}), vec!["fleet:read"]),
            ("waiting", json!({}), vec!["fleet:read"]),
            ("held", json!({}), vec!["mail:read"]),
            ("inbox", json!({"ship":"ops"}), vec!["mail:read"]),
            ("show", json!({"ship":"ops","id":"m"}), vec!["mail:read"]),
            ("thread", json!({"ship":"ops","id":"m"}), vec!["mail:read"]),
            ("ack", json!({"refs":["r"]}), vec!["mail:read"]),
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
        assert_eq!(
            cases.iter().map(|(name, _, _)| *name).collect::<Vec<_>>(),
            Command::ALL
                .iter()
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
        );
        for (name, args, scopes) in cases {
            let command = Command::parse(name).unwrap();
            assert_eq!(requirements(command, &args).unwrap(), scopes);
            let message = json!({"type":"request", "id":"r", "command":name, "args":args});
            assert_eq!(
                admit("fixture", &message, &mut false, |_, _, _| unreachable!()),
                Err(Denial::MissingCredential)
            );
            // Persistence is exercised with isolated fleet state in the
            // subprocess suite, never the user's registry.
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
                vec!["ship:message"],
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
