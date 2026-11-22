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
}

/// Walk generated reminders in reverse transcript order. Keep the newest index
/// on its own message when that message survives. If it is in the prefix being
/// summarized, return it for the new window instead. User content is untouched.
pub(super) fn replay_index(
    additions: &mut [(Vec<String>, Vec<String>)],
    retain_from: usize,
) -> (bool, Option<String>) {
    let mut changed = false;
    let mut seen = false;
    let mut prefix = None;
    for (row, (before, after)) in additions.iter_mut().enumerate().rev() {
        for blocks in [after, before] {
            for i in (0..blocks.len()).rev() {
                if !is_index(&blocks[i]) {
                    continue;
                }
                if seen {
                    changed = true;
                    blocks.remove(i);
                } else if row < retain_from {
                    changed = true;
                    prefix = Some(blocks.remove(i).replacen(
                        "MEMORY.md changed. The current index follows.\n",
                        "",
                        1,
                    ));
                }
                seen = true;
            }
        }
    }
    (changed, prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_replay_keeps_only_the_newest_announcement_with_protected_steers() {
        let index = "<system-reminder>\n<project_memory>\nC".to_owned();
        let change = |content| {
            format!("<system-reminder>\nMEMORY.md changed. The current index follows.\n<project_memory>\n{content}")
        };
        let mut rows = vec![
            (vec![index], vec![]),
            (vec![], vec![change("C"), "first recall".into()]),
            (vec![], vec![change("D")]),
            (vec![], vec![change("C"), "steer recall".into()]),
        ];
        let (changed, prefix) = replay_index(&mut rows, 1);
        assert!(changed && prefix.is_none());
        assert!(rows[0].0.is_empty() && rows[2].1.is_empty());
        assert_eq!(rows[1].1, ["first recall"]);
        assert_eq!(rows[3].1, [change("C"), "steer recall".to_owned()]);
    }
}
