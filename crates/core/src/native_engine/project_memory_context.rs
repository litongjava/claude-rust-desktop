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

    pub(super) fn refresh_compacted_memory(&mut self) -> Result<Option<String>> {
        let Some(memory) = &self.file_memory else {
            return Ok(None);
        };
        let current = memory.current_prompt_index(self.memory_read_only())?;
        if let Some(at) = self.context_reminders.iter().position(|s| is_index(s)) {
            self.context_reminders.splice(at..=at, current.clone());
        } else {
            self.context_reminders.extend(current.clone());
        }
        for entry in &mut self.turn_context {
            entry.blocks.retain(|s| !is_index(s));
        }
        Ok(current)
    }

    pub(super) fn add_compacted_memory(&mut self, messages: &mut Vec<Value>) -> Result<()> {
        if let Some(index) = self.refresh_compacted_memory()? {
            let at = messages
                .iter()
                .take_while(|m| m["role"] == "system")
                .count();
            messages.insert(at, json!({"role":"user","content":index}));
        }
        Ok(())
    }
}
