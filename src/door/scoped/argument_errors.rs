//! Trusted transport's value-free schema denial metadata, not a fleet operation.
use super::{Admission, Denial};
use serde_json::Value;

pub(super) fn metadata(args: &Value) -> Admission<(&'static [&'static str], String)> {
    super::shape(
        args,
        &["tool", "field", "rule"],
        &["tool", "field", "rule"],
        &[],
    )?;
    let tool = args["tool"].as_str().ok_or(Denial::Arguments)?;
    // This inventory must track the plugin's visibility scopes. Never accept a
    // caller-supplied scope or make a hidden tool's schema discoverable.
    let required: &'static [&'static str] = match tool {
        "fleet_ships" | "fleet_status" | "fleet_waiting" | "away_status" => &["fleet:read"],
        "mail_inbox" | "mail_show" | "mail_thread" | "mail_ack" => &["mail:read"],
        "send_message" | "reply" | "ask_liaison" => &["ship:message"],
        "dismiss" => &["work:manage"],
        "away" => &["captain:presence"],
        _ => return Err(Denial::UnknownCommand),
    };
    let field = args["field"].as_str().ok_or(Denial::Arguments)?;
    if field.is_empty()
        || field.len() > 64
        || !field
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        || !field.as_bytes()[0].is_ascii_alphabetic() && !field.starts_with('_')
    {
        return Err(Denial::Arguments);
    }
    let rule = args["rule"].as_str().ok_or(Denial::Arguments)?;
    if !matches!(
        rule,
        "additionalProperties"
            | "required"
            | "maxLength"
            | "minLength"
            | "type"
            | "minimum"
            | "maximum"
            | "minItems"
            | "maxItems"
            | "enum"
            | "pattern"
            | "uniqueItems"
            | "schema"
    ) {
        return Err(Denial::Arguments);
    }
    Ok((
        required,
        format!("invalid_arguments tool={tool} field={field} rule={rule}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_bounded_value_free_metadata_is_accepted() {
        let args = json!({"tool":"send_message","field":"body","rule":"maxLength"});
        assert_eq!(metadata(&args).unwrap().0, &["ship:message"]);
        for (key, value) in [
            ("tool", "hidden"),
            ("field", "body\nsecret"),
            ("field", ""),
            ("rule", "secret-value"),
            ("value", "never-log"),
        ] {
            let mut invalid = args.clone();
            invalid[key] = json!(value);
            assert!(metadata(&invalid).is_err());
        }
    }
}
