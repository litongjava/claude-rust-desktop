use crate::db::session_store::settings::Microcompact;
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

/// Render the standalone frontend's durable tool-result window. The caller
/// decides whether this is an actual turn that should advance the cursor.
pub fn replay(
    history: &mut [Value],
    state: &mut Microcompact,
    window: Option<u32>,
) -> anyhow::Result<bool> {
    let len = history.len();
    clear_tool_results(&mut history[..state.through.min(len)]);
    let estimated = serde_json::to_string(&history)?.len().div_ceil(3);
    let through = through(state.through, len, estimated, window);
    if through == state.through {
        return Ok(false);
    }
    state.through = through;
    clear_tool_results(&mut history[..through.min(len)]);
    Ok(true)
}

pub fn through(current: usize, len: usize, estimated_tokens: usize, window: Option<u32>) -> usize {
    const KEEP_RECENT: usize = 10;
    match window.filter(|window| *window > 0) {
        Some(window) if estimated_tokens > window as usize / 2 => {
            current.max(len.saturating_sub(KEEP_RECENT))
        }
        _ => current,
    }
}
