//! Exact hold metadata and application of target-terminal-issued permits.
//! This module never issues authority, forwards native dispatch, or recurses.
use super::*;
use crate::jobs::recovery as native;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryBinding {
    ledger: String,
    incarnation: Option<String>,
    reference: native::Reference,
}

fn probe(
    registry: &Connection,
    a: &Value,
    c: &DelegatedContext,
    link: Option<String>,
) -> Result<Value> {
    native::gateway_target(
        &home(registry, a)?,
        &a["reference"],
        native::Authority::gateway(c, link),
        a["idempotency_key"].as_str().ok_or(Denial::Arguments)?,
        a["approval_id"].as_str().ok_or(Denial::Arguments)?,
    )
    .map_err(native_error)
}

pub(in crate::door) fn validate(a: &Value, read: bool) -> Result<(), Denial> {
    let name = if read {
        "resource_recovery_reference"
    } else {
        "resource_recover"
    };
    let allowed = super::super::ACCEPTED
        .iter()
        .find(|(command, _)| *command == name)
        .ok_or(Denial::Arguments)?
        .1;
    if !keys(a, allowed, allowed) || !handle(&a["machine"]) || !handle(&a["ship"]) {
        return Err(Denial::Arguments);
    }
    if read {
        if !handle(&a["job_id"]) {
            return Err(Denial::Arguments);
        }
    } else {
        let reference: native::Reference =
            serde_json::from_value(a["reference"].clone()).map_err(|_| Denial::Arguments)?;
        if !reference.valid_for(a["ship"].as_str().unwrap())
            || a["approval_id"]
                .as_str()
                .is_none_or(|s| uuid::Uuid::parse_str(s).is_err())
            || a["idempotency_key"].as_str().is_none_or(|s| {
                s.is_empty() || s.len() > 128 || !s.bytes().all(|b| (32..=126).contains(&b))
            })
        {
            return Err(Denial::Arguments);
        }
    }
    Ok(())
}
fn read_authority(c: &DelegatedContext, a: &Value) -> Result<()> {
    ensure!(c.has("fleet:read"), Denial::Scope);
    ensure!(
        a["ship"].as_str().is_some_and(|s| c.allows(s)),
        Denial::RecoveryTargetStale
    );
    Ok(())
}
fn home(registry: &Connection, a: &Value) -> Result<PathBuf> {
    registry
        .query_row(
            "SELECT home FROM ships WHERE name=?1 AND retired_ms IS NULL",
            [a["ship"].as_str().ok_or(Denial::Arguments)?],
            |r| r.get::<_, String>(0),
        )
        .map(PathBuf::from)
        .map_err(|_| Denial::RecoveryTargetStale.into())
}
fn native_error(e: anyhow::Error) -> anyhow::Error {
    // Only typed native refusals cross this boundary. Never serialize prose.
    e.downcast_ref::<native::Refusal>()
        .and_then(|r| Denial::from_code(&r.to_string()))
        .unwrap_or(Denial::ExecutionFailed)
        .into()
}
fn route(registry: &Connection, a: &Value, c: &DelegatedContext) -> Result<Option<String>> {
    if a["machine"] == "local" {
        return Ok(None);
    }
    let machine = a["machine"].as_str().ok_or(Denial::Arguments)?;
    let callable: bool = registry.query_row(
        "SELECT EXISTS(SELECT 1 FROM machines WHERE name=?1 AND role='collects')",
        [machine],
        |r| r.get(0),
    )?;
    ensure!(callable, Denial::LinkedUnavailable);
    let present: bool = registry.query_row(
        "SELECT EXISTS(SELECT 1 FROM machine_ships WHERE name=?1 AND machine=?2)",
        params![a["ship"].as_str(), machine],
        |r| r.get(0),
    )?;
    ensure!(present, Denial::RecoveryTargetStale);
    let link = crate::machines::link_incarnation(registry, machine)?;
    let answer = crate::machines::resource_action_of(
        machine,
        &link,
        &json!({"context":c.gateway(),"operation":"capabilities","args":{}}),
        false,
    )?;
    ensure!(
        keys(
            &answer,
            &["schema", "operations"],
            &["schema", "operations"]
        ) && answer["schema"] == "resource.action.capabilities.v1"
            && answer["operations"]
                .as_array()
                .is_some_and(|ops| ops.contains(&json!("resource_recover"))),
        Denial::Unsupported
    );
    Ok(Some(link))
}

pub(in crate::door) fn reference(a: &Value, c: &DelegatedContext) -> Result<Value> {
    validate(a, true)?;
    read_authority(c, a)?;
    let registry = super::super::runtime::registry_readonly()?;
    let link = route(&registry, a, c)?;
    let reference = if let Some(link) = &link {
        crate::machines::resource_action_of(
            a["machine"].as_str().unwrap(),
            link,
            &json!({"context":c.gateway(),"operation":"recovery_reference","args":a}),
            false,
        )?
    } else {
        native::gateway_reference(
            &home(&registry, a)?,
            a["ship"].as_str().unwrap(),
            a["job_id"].as_str().unwrap(),
        )
        .map_err(native_error)?
    };
    let parsed: native::Reference =
        serde_json::from_value(reference.clone()).map_err(|_| Denial::ExecutionFailed)?;
    ensure!(
        parsed.valid_for(a["ship"].as_str().unwrap()) && reference["job"] == a["job_id"],
        Denial::ExecutionFailed
    );
    Ok(
        json!({"schema":"resource.recovery.reference.v1","machine":a["machine"],"ship":a["ship"],"reference":reference,"authority":native::Authority::gateway(c,link)}),
    )
}

pub(in crate::door) fn prepare(a: &mut Value, c: &DelegatedContext) -> Result<()> {
    validate(a, false)?;
    authority(c)?;
    let registry = super::super::runtime::registry_readonly()?;
    let link = route(&registry, a, c)?;
    let answer = if let Some(link) = &link {
        crate::machines::resource_action_of(
            a["machine"].as_str().unwrap(),
            link,
            &json!({"context":c.gateway(),"operation":"target","args":a,"incarnation":link}),
            false,
        )?
    } else {
        probe(&registry, a, c, None)?
    };
    ensure!(
        keys(&answer, &["ledger", "reference"], &["ledger", "reference"])
            && opaque(&answer["ledger"]),
        Denial::ExecutionFailed
    );
    ensure!(
        answer["ledger"] == a["reference"]["ledger"] && answer["reference"] == a["reference"],
        Denial::RecoveryTargetStale
    );
    a["_binding"] = json!(RecoveryBinding {
        ledger: answer["ledger"].as_str().unwrap().into(),
        incarnation: link,
        reference: serde_json::from_value(answer["reference"].clone())
            .map_err(|_| Denial::ExecutionFailed)?
    });
    Ok(())
}

pub(in crate::door) fn request(a: &Value, c: &DelegatedContext) -> Result<Value> {
    authority(c)?;
    let mut public = a.clone();
    let binding: RecoveryBinding = serde_json::from_value(
        public
            .as_object_mut()
            .ok_or(Denial::Arguments)?
            .remove("_binding")
            .ok_or(Denial::Arguments)?,
    )?;
    validate(&public, false)?;
    ensure!(
        json!(binding.reference) == public["reference"],
        Denial::RecoveryTargetStale
    );
    let result = if let Some(link) = &binding.incarnation {
        crate::machines::resource_action_of(
            public["machine"].as_str().unwrap(),
            link,
            &json!({"context":c.gateway(),"operation":"resource_recover","args":public,"target":binding.ledger,"incarnation":link}),
            true,
        )?
    } else {
        ensure!(public["machine"] == "local", Denial::Arguments);
        apply(
            &super::super::runtime::registry_readonly()?,
            &public,
            c,
            &binding.ledger,
            None,
        )?
    };
    validate_result(&result, &public, c).map_err(|_| Denial::OutcomeUnknown)?;
    Ok(result)
}
fn validate_result(v: &Value, a: &Value, c: &DelegatedContext) -> Result<()> {
    ensure!(
        keys(
            v,
            &[
                "schema",
                "operation",
                "result_id",
                "reference",
                "applied",
                "outcome",
                "machine",
                "ship",
                "provenance"
            ],
            &[
                "schema",
                "operation",
                "result_id",
                "reference",
                "applied",
                "outcome",
                "machine",
                "ship",
                "provenance"
            ]
        ) && v["schema"] == "resource.action.v1"
            && v["operation"] == "resource_recover"
            && v["machine"] == a["machine"]
            && v["ship"] == a["ship"]
            && v["reference"] == a["reference"]
            && v["applied"] == true
            && v["outcome"] == "recovered"
            && v["result_id"]
                .as_str()
                .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok()),
        Denial::OutcomeUnknown
    );
    let mut provenance = c.provenance();
    provenance["result_id"] = v["result_id"].clone();
    ensure!(v["provenance"] == provenance, Denial::OutcomeUnknown);
    Ok(())
}
fn apply(
    registry: &Connection,
    a: &Value,
    c: &DelegatedContext,
    target: &str,
    link: Option<String>,
) -> Result<Value> {
    authority(c)?;
    validate(a, false)?;
    ensure!(
        a["reference"]["ledger"] == target,
        Denial::RecoveryTargetStale
    );
    let result = native::gateway_apply(
        &home(registry, a)?,
        &a["reference"],
        native::Authority::gateway(c, link),
        a["idempotency_key"].as_str().unwrap(),
        a["approval_id"].as_str().unwrap(),
    )
    .map_err(native_error)?;
    let mut provenance = c.provenance();
    provenance["result_id"] = result["result_id"].clone();
    // Native results stay unchanged in their atomic ledger transaction. The
    // scoped acknowledgment adds only authenticated originating provenance.
    let result = json!({"schema":"resource.action.v1","operation":"resource_recover","machine":a["machine"],"ship":a["ship"],"result_id":result["result_id"],"reference":result["reference"],"applied":true,"outcome":"recovered","provenance":provenance});
    validate_result(&result, a, c)?;
    Ok(result)
}
pub(super) fn serve(body: &Value, c: &DelegatedContext) -> Result<Value> {
    let a = &body["args"];
    let registry = super::super::runtime::registry_readonly()?;
    if body["operation"] == "recovery_reference" {
        ensure!(
            keys(
                body,
                &["context", "operation", "args"],
                &["context", "operation", "args"]
            ),
            Denial::Arguments
        );
        validate(a, true)?;
        read_authority(c, a)?;
        return native::gateway_reference(
            &home(&registry, a)?,
            a["ship"].as_str().unwrap(),
            a["job_id"].as_str().unwrap(),
        )
        .map_err(native_error);
    }
    authority(c)?;
    validate(a, false)?;
    if body["operation"] == "target" {
        ensure!(
            keys(
                body,
                &["context", "operation", "args", "incarnation"],
                &["context", "operation", "args", "incarnation"]
            ) && opaque(&body["incarnation"]),
            Denial::Arguments
        );
        return probe(
            &registry,
            a,
            c,
            Some(body["incarnation"].as_str().unwrap().into()),
        );
    }
    ensure!(
        keys(
            body,
            &["context", "operation", "args", "target", "incarnation"],
            &["context", "operation", "args", "target", "incarnation"]
        ) && opaque(&body["target"])
            && opaque(&body["incarnation"]),
        Denial::Arguments
    );
    apply(
        &registry,
        a,
        c,
        body["target"].as_str().unwrap(),
        Some(body["incarnation"].as_str().unwrap().into()),
    )
}
