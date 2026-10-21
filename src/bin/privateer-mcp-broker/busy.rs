//! The one shape every refusal for capacity takes: never a held-open call.
//! Admission (a later step) reuses this.
use serde_json::{Value, json};

/// The answer to request `id` (method `method`) when the upstream cannot take
/// it now. A `tools/call` gets a tool error the model reads; anything else a
/// JSON-RPC error carrying `data.retry_after_secs`.
pub fn busy(id: &Value, method: &str, label: &str, reason: &str, retry_after_secs: u64) -> Value {
    let text = format!("{label} tools busy: {reason}; retry after {retry_after_secs} s");
    if method == "tools/call" {
        json!({"jsonrpc": "2.0", "id": id, "result": {
            "isError": true,
            "content": [{"type": "text", "text": text}],
            "_meta": {"privateer/retry_after_secs": retry_after_secs},
        }})
    } else {
        json!({"jsonrpc": "2.0", "id": id, "error": {
            "code": -32000,
            "message": text,
            "data": {"retry_after_secs": retry_after_secs},
        }})
    }
}

/// `xcode` reads as `Xcode` in the busy text.
pub fn label(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_call_is_a_tool_error_and_anything_else_a_retry_error() {
        let call = busy(&json!(7), "tools/call", "Xcode", "starting", 5);
        assert_eq!(call["id"], 7);
        assert_eq!(call["result"]["isError"], true);
        assert_eq!(
            call["result"]["content"][0]["text"],
            "Xcode tools busy: starting; retry after 5 s"
        );
        assert_eq!(call["result"]["_meta"]["privateer/retry_after_secs"], 5);
        let list = busy(&json!("a"), "tools/list", "Xcode", "starting", 5);
        assert_eq!(list["error"]["data"]["retry_after_secs"], 5);
        assert_eq!(label("xcode"), "Xcode");
    }
}
