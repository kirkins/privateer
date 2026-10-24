//! Typed machine-scoped administration. The receiver never fans out; the
//! originating door reports each machine independently, including uncertainty.
use super::scoped::{DelegatedContext, Denial};
use anyhow::Result;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub(crate) fn row(name: &str, local: bool, result: Result<Value>) -> Value {
    match result {
        Ok(result) => json!({"name":name,"local":local,"confirmed":true,"result":result}),
        Err(error) => {
            let denial = error
                .downcast_ref::<Denial>()
                .copied()
                .unwrap_or(Denial::ExecutionFailed);
            json!({"name":name,"local":local,"confirmed":false,"denial_code":denial.reason()})
        }
    }
}

pub(crate) fn permissions(
    store: &crate::store::Store,
    args: &Value,
    context: &DelegatedContext,
) -> Result<Value> {
    super::scoped::gateway_admin("permissions", args, context)?;
    let targets = crate::machines::permission_targets()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut rows = vec![row(
        &crate::machines::this_machine(),
        true,
        crate::permissions::local_delegated(store, args, context),
    )];
    for (name, reachable) in targets {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = if !reachable {
            Err(Denial::LinkedUnavailable.into())
        } else if remaining.is_zero() {
            Err(Denial::ExecutionFailed.into())
        } else {
            crate::machines::delegated_machine_call(
                &name,
                "permissions",
                args.clone(),
                context,
                remaining.min(Duration::from_secs(5)),
            )
        };
        rows.push(row(&name, false, result));
    }
    Ok(json!({"partial":rows.iter().any(|r| r["confirmed"] != true),"machines":rows}))
}
