//! Reading a user message whose content carries context reminders.
use serde_json::Value;

/// The user's own text in a message's `content`.
///
/// Context reminders are `<system-reminder>` blocks. They open the first user
/// message (environment, instructions, loadable tools, skills, date, git) and
/// can also follow the text as recalled context. `content` may be a string or
/// an array of text parts: reminder blocks and parts that hold only reminders
/// are dropped, and the whitespace left between the remaining pieces is
/// trimmed.
pub fn user_text(content: &Value) -> String {
    let mut pieces = Vec::new();
    match content {
        Value::String(text) => pieces.push(text.as_str()),
        Value::Array(parts) => parts
            .iter()
            .filter_map(part_text)
            .for_each(|text| pieces.push(text)),
        _ => return String::new(),
    }
    pieces
        .into_iter()
        .map(strip_reminders)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn part_text(part: &Value) -> Option<&str> {
    match part {
        Value::String(text) => Some(text),
        _ => part.get("text").and_then(Value::as_str),
    }
}

/// Remove every `<system-reminder>...</system-reminder>` block and trim the
/// whitespace between the pieces that remain.
fn strip_reminders(mut text: &str) -> String {
    let mut pieces = Vec::new();
    loop {
        let Some(start) = text.find("<system-reminder>") else {
            pieces.push(text);
            break;
        };
        pieces.push(&text[..start]);
        let Some(end) = text[start..].find("</system-reminder>") else {
            pieces.push(&text[start..]);
            break;
        };
        text = &text[start + end + "</system-reminder>".len()..];
    }
    pieces
        .into_iter()
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
