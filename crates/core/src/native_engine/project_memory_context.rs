//! A compressed window starts with the active project's index. Keep previously
//! attached indexes out of summaries and provider-preserved user items.
use super::*;

fn is_index(block: &str) -> bool {
    block.starts_with("<system-reminder>\n<project_memory>\n")
        || block.starts_with(
            "<system-reminder>\nMEMORY.md changed. The current index follows.\n<project_memory>\n",
        )
}

impl ToolLoopExecutor {
    pub(super) fn remove_compaction_indexes(&self, messages: &mut Vec<Value>) {
        let indexes: Vec<_> = self
            .context_reminders
            .iter()
            .chain(self.turn_context.iter().flat_map(|entry| &entry.blocks))
            .filter(|s| is_index(s))
            .collect();
        if indexes.is_empty() {
            return;
        }
        messages.retain_mut(|m| {
            if m["role"] != "user" || !m["provider_content"].is_null() {
                return true;
            }
            match &mut m["content"] {
                Value::Array(parts) => {
                    parts.retain(|p| {
                        !indexes
                            .iter()
                            .any(|s| p["text"].as_str() == Some(s.as_str()))
                    });
                    !parts.is_empty()
                }
                Value::String(text) => {
                    for index in &indexes {
                        if text == *index {
                            return false;
                        }
                        // Chat Completions joins the prefix, user text and turn
                        // attachments with blank lines.
                        if let Some(at) = text.find(index.as_str()) {
                            let end = at + index.len();
                            if (at == 0 || text[..at].ends_with("\n\n"))
                                && (end == text.len() || text[end..].starts_with("\n\n"))
                            {
                                if end < text.len() {
                                    text.replace_range(at..end + 2, "");
                                } else {
                                    text.truncate(at.saturating_sub(2));
                                }
                            }
                        }
                    }
                    true
                }
                _ => true,
            }
        });
    }

    pub(super) fn compacted_index(&self) -> Option<String> {
        // In-flight compression has no durable boundary. Carry forward the most
        // recently announced index from this request's immutable receipts.
        self.turn_context
            .iter()
            .rev()
            .flat_map(|entry| &entry.blocks)
            .find(|block| is_index(block))
            .map(|block| block.replacen("MEMORY.md changed. The current index follows.\n", "", 1))
            .or_else(|| {
                self.context_reminders
                    .iter()
                    .find(|block| is_index(block))
                    .cloned()
            })
    }

    pub(super) fn add_compacted_memory(&self, messages: &mut Vec<Value>) {
        if let Some(index) = self.compacted_index() {
            let at = messages
                .iter()
                .take_while(|m| m["role"] == "system")
                .count();
            messages.insert(at, json!({"role":"user","content":index}));
        }
    }
}
