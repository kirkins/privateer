//! Shared extraction of completed assistant replies from harness events.
use serde_json::Value;

/// Text from a completed assistant reply event. Pi progress/tool-use messages
/// are deliberately excluded; only a stopped turn is a verdict.
pub(crate) fn text(event: &Value) -> Option<String> {
    match event["type"].as_str()? {
        "message_end"
            if event["message"]["role"] == "assistant"
                && event["message"]["stopReason"] == "stop" =>
        {
            content_text(&event["message"]["content"])
        }
        "item.completed" if event["item"]["type"] == "agent_message" => event["item"]["text"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        "result" => event["result"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

fn content_text(content: &Value) -> Option<String> {
    let pieces: Vec<&str> = content
        .as_array()?
        .iter()
        .filter(|item| item["type"] == "text")
        .filter_map(|item| item["text"].as_str())
        .collect();
    (!pieces.is_empty()).then(|| pieces.join("\n"))
}

/// Iterate complete JSON objects, ignoring rotation-cut fragments. Events are
/// normally JSONL, but scanning object boundaries also tolerates a log whose
/// records were concatenated without newlines.
pub(crate) fn events(log: &str) -> impl Iterator<Item = Value> + '_ {
    let bytes = log.as_bytes();
    let mut at = 0;
    let mut parsed = Vec::new();
    while at < bytes.len() {
        if bytes[at] != b'{' {
            at += 1;
            continue;
        }
        let mut decoder = serde_json::Deserializer::from_slice(&bytes[at..]).into_iter::<Value>();
        match decoder.next() {
            Some(Ok(value)) if value.is_object() => {
                let consumed = decoder.byte_offset();
                parsed.push(value);
                at += consumed.max(1);
            }
            _ => at += 1,
        }
    }
    parsed.into_iter()
}

pub(crate) fn parse_event(line: &str) -> Option<Value> {
    serde_json::from_str(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_completed_assistant_turns_are_replies() {
        let progress = serde_json::json!({"type":"message_end","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"text","text":"progress"}]}});
        let final_event = serde_json::json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"final verdict"}]}});
        assert_eq!(text(&progress), None);
        assert_eq!(text(&final_event).as_deref(), Some("final verdict"));
        assert_eq!(text(&serde_json::json!({"type":"agent_settled"})), None);
    }
}
