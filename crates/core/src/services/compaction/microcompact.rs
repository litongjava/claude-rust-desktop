use serde_json::{json, Value};

pub const MICROCOMPACT_MIN_CHARS: usize = 1000;
pub const CLEARED_TOOL_RESULT: &str = "[Old tool result content cleared to save context]";

/// Replaces large tool results in `messages` with a short placeholder, in
/// either provider's history format.
pub fn clear_tool_results(messages: &mut [Value]) {
    let large = |content: &Value| match content {
        Value::String(text) => text.len() > MICROCOMPACT_MIN_CHARS,
        Value::Array(blocks) => {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .map(str::len)
                .sum::<usize>()
                > MICROCOMPACT_MIN_CHARS
                || blocks.iter().any(|block| block["type"] == "image")
        }
        _ => false,
    };
    for message in messages {
        if message["role"] == "tool" && large(&message["content"]) {
            message["content"] = json!(CLEARED_TOOL_RESULT);
        }
        if let Some(blocks) = message["content"].as_array_mut() {
            for block in blocks
                .iter_mut()
                .filter(|block| block["type"] == "tool_result")
            {
                if large(&block["content"]) {
                    block["content"] = json!(CLEARED_TOOL_RESULT);
                }
            }
        }
    }
}

pub(crate) fn has_cleared_result(message: &Value) -> bool {
    (message["role"] == "tool" && message["content"] == CLEARED_TOOL_RESULT)
        || message["content"].as_array().is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| b["type"] == "tool_result" && b["content"] == CLEARED_TOOL_RESULT)
        })
}
