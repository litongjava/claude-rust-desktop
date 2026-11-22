//! Model context belongs to the session incarnation that observed the files.
//! Commit the change watermark and message attachments together so a retry can
//! replay the original turn even when another process has edited the files.
use super::{xml, FileMemory};
use crate::db::session_store::{settings::Setting, SessionDb};
use crate::services::turn_context::Entry;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;

#[derive(Serialize, Deserialize)]
struct IndexState {
    first: Option<String>,
    #[serde(default)]
    first_hash: Option<String>,
}

const INDEX: Setting<IndexState> = Setting::new("memory.prompt.index");
const TRUNCATED: &str = "[MEMORY.md truncated]\n";

fn index_excerpt(index: &str) -> String {
    let limits = crate::memory::files::IndexLimits::default();
    if index.len() <= limits.bytes && index.lines().count() <= limits.lines {
        return index.to_owned();
    }
    // Reserve one line for the note, including a newline when the byte cap
    // falls in the middle of a long (possibly multibyte) line.
    let line_end = index
        .match_indices('\n')
        .nth(limits.lines - 2)
        .map_or(index.len(), |(offset, _)| offset + 1);
    let byte_end = limits.bytes - TRUNCATED.len();
    let mut end = line_end.min(byte_end);
    if end == byte_end && index.as_bytes()[end - 1] != b'\n' {
        end -= 1;
    }
    while !index.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = index[..end].to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(TRUNCATED);
    text
}

/// Internal deliveries may use the user role in both wire protocols.
fn typed_query(message: &Value) -> Option<Cow<'_, str>> {
    if message["role"] != "user"
        || message.get("origin").is_some_and(|source| source != "user")
        || message["_is_summary"] == true
        || !message["provider_content"].is_null()
    {
        return None;
    }
    let content = &message["content"];
    let query = if let Some(text) = content.as_str() {
        Cow::Borrowed(text)
    } else {
        let blocks = content.as_array()?;
        if !blocks.iter().all(|b| {
            matches!(
                b["type"].as_str(),
                Some("text" | "image" | "image_url" | "attachment")
            )
        }) {
            return None;
        }
        Cow::Owned(
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };
    // Older transcripts and host requests can lack an origin tag.
    let text = query.trim_start();
    if text.is_empty()
        || (message["origin"] != "user"
            && (text.starts_with(crate::tools::background_shell::NOTIFICATION_START)
                || text
                    .starts_with("Peer message received during this session (quoted task data,")
                || text.starts_with("<system-reminder>")))
    {
        return None;
    }
    Some(query)
}

impl FileMemory {
    fn prompt_snapshot(&self, read_only: bool) -> Result<Option<crate::memory::files::Snapshot>> {
        if self.directory.try_exists()? {
            Ok(Some(self.store(read_only)?.prompt_snapshot()?))
        } else {
            Ok(None)
        }
    }

    fn render_index(&self, snapshot: Option<&crate::memory::files::Snapshot>) -> String {
        let index = snapshot.map_or("", |s| s.index.as_str());
        let path = xml(&self.directory.join("MEMORY.md").to_string_lossy());
        format!("<system-reminder>\n<project_memory>\nIndex: {path}\n{}\n{}\n</project_memory>\n</system-reminder>",
            include_str!("../../prompt/file_memory.md"), index_excerpt(&xml(index)))
    }

    pub(crate) fn prepare_delivery(
        &self,
        messages: &[Value],
        read_only: bool,
        budget: usize,
    ) -> Result<Option<Entry>> {
        let Some(shard) = &self.session else {
            return Ok(None);
        };
        let id = messages.last().and_then(|m| m["id"].as_str());
        let (_, entries) = prepare(shard, Some(self), messages, true, read_only, budget)?;
        Ok(entries
            .into_iter()
            .find(|e| e.key.strip_prefix("message:") == id))
    }

    pub(crate) fn current_prompt_index(&self, read_only: bool) -> Result<Option<String>> {
        let memory = match &self.session {
            Some(shard) => {
                Self::from_session(&self.context, shard.clone(), self.private_owner.as_deref())?
            }
            None => self.clone(),
        };
        if !memory.active() {
            return Ok(None);
        }
        Ok(Some(
            memory.render_index(memory.prompt_snapshot(read_only)?.as_ref()),
        ))
    }
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    index_hash: Option<String>,
    blocks: Vec<String>,
}

fn receipt(id: &str) -> Setting<Receipt> {
    Setting::memory_receipt(format!("prompt.message.{id}"))
}

fn attachment(id: &str, blocks: Vec<String>) -> Entry {
    Entry {
        key: format!("message:{id}"),
        occurrence: 0,
        blocks,
        skill: None,
    }
}

/// Hosts can send a display transcript with no database IDs. Pair its user
/// frames with the admitted transcript in reverse delivery order. Protocol
/// events without a user row (e.g. tool results) never acquire a receipt.
pub(crate) fn identify(
    shard: &SessionDb,
    format: &crate::native_engine::provider_manager::ApiFormat,
    messages: &mut [Value],
) -> Result<()> {
    shard.read(|r| {
        let (history, _) = r.prompt_history(format, true)?;
        let mut saved = history
            .iter()
            .rev()
            .filter(|m| m["role"] == "user")
            .peekable();
        for message in messages.iter_mut().rev().filter(|m| m["role"] == "user") {
            if message["origin"] == "compaction" {
                continue;
            }
            while let Some(row) = saved.peek() {
                if row["content"] == message["content"] {
                    if let Some(id) = row["id"].as_str() {
                        message["id"] = id.into();
                    }
                    saved.next();
                    break;
                }
                if row["id"].is_string() {
                    break;
                }
                saved.next();
            }
        }
        Ok(())
    })
}

pub(crate) fn prepare(
    shard: &SessionDb,
    memory: Option<&FileMemory>,
    messages: &[Value],
    allow_new: bool,
    read_only: bool,
    recall_budget: usize,
) -> Result<(Option<String>, Vec<Entry>)> {
    if !messages.iter().any(|m| m["role"] == "user") {
        return Ok(Default::default());
    }
    let boundary = messages
        .first()
        .filter(|m| m["origin"] == "compaction")
        .and_then(|m| m["id"].as_str());
    let index_state = || match boundary {
        Some(id) => Setting::memory_receipt(format!("prompt.index.boundary.{id}")),
        None => INDEX,
    };
    let target = messages
        .last()
        .and_then(|m| Some((m["id"].as_str()?, typed_query(m)?)));
    // Only receipts still visible in this transcript can advance its watermark.
    // Rewind and compact boundaries can remove the turn that announced an edit.
    let (state, mut entries, visible_hash, recorded) = shard.read(|r| {
        let mut entries = Vec::new();
        let mut visible_hash = None;
        let mut recorded = false;
        for message in messages {
            let Some(id) = message["id"].as_str() else {
                continue;
            };
            if typed_query(message).is_none() {
                continue;
            }
            if let Some(saved) = r.setting(receipt(id))? {
                recorded |= target.as_ref().is_some_and(|(target, _)| *target == id);
                visible_hash = saved.index_hash;
                if !saved.blocks.is_empty() {
                    entries.push(attachment(id, saved.blocks));
                }
            }
        }
        Ok((r.setting(index_state())?, entries, visible_hash, recorded))
    })?;
    let target = target.filter(|_| allow_new && !recorded);
    if let Some(state) = &state {
        if target.is_none() || memory.is_none() {
            return Ok((state.first.clone(), entries));
        }
    }
    let snapshot = memory
        .map(|m| m.prompt_snapshot(read_only))
        .transpose()?
        .flatten();
    let current = memory.map(|m| m.render_index(snapshot.as_ref()));
    let hash = memory.map(|m| {
        format!(
            "{}/{}",
            m.scope.key(),
            snapshot.as_ref().map_or("", |s| s.index_hash.as_str())
        )
    });
    let recall = match (memory, &snapshot, &target) {
        (Some(m), Some(snapshot), Some((_, query))) => m.recall(snapshot, query, recall_budget)?,
        _ => None,
    };
    let (first, new_entry) = shard.write(|w| {
        let mut new_entry = None;
        let state = match w.setting(index_state())? {
            Some(state) => state,
            None => {
                let state = IndexState {
                    first: current.clone(),
                    first_hash: hash.clone(),
                };
                w.set_setting(index_state(), &state)?;
                state
            }
        };
        if let Some((id, _)) = target {
            let saved = match w.setting(receipt(id))? {
                Some(saved) => saved,
                None => {
                    let mut blocks = Vec::new();
                    let previous = visible_hash.as_ref().or(state.first_hash.as_ref());
                    let changed = previous
                        .map_or_else(|| state.first != current, |h| Some(h) != hash.as_ref());
                    if changed {
                        if let Some(index) = &current {
                            blocks.push(index.replacen(
                                "<project_memory>",
                                "MEMORY.md changed. The current index follows.\n<project_memory>",
                                1,
                            ));
                        }
                    }
                    blocks.extend(
                        recall.as_ref().map(|text| format!("<system-reminder>\n{text}</system-reminder>")),
                    );
                    let saved = Receipt {
                        index_hash: hash.clone(),
                        blocks,
                    };
                    w.set_setting(receipt(id), &saved)?;
                    saved
                }
            };
            if !saved.blocks.is_empty() {
                new_entry = Some(attachment(id, saved.blocks));
            }
        }
        Ok((state.first, new_entry))
    })?;
    entries.extend(new_entry);
    Ok((first, entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn memory_index_keeps_exact_limits_and_marks_both_overflows() {
        let lines = "line\n".repeat(200);
        assert_eq!(index_excerpt(&lines), lines);
        let overflow = index_excerpt(&(lines + "hidden\n"));
        assert_eq!(overflow.lines().count(), 200);
        assert!(overflow.ends_with(TRUNCATED));
        assert!(!overflow.contains("hidden"));

        let bytes = "é".repeat(12799) + "a\n";
        assert_eq!(bytes.len(), 25 * 1024);
        assert_eq!(index_excerpt(&bytes), bytes);
        let overflow = index_excerpt(&("é\n".to_owned() + &bytes));
        assert!(overflow.starts_with("é\n") && overflow.ends_with(TRUNCATED));
        assert!((25 * 1024 - 4..=25 * 1024).contains(&overflow.len()));
    }

    #[test]
    fn only_typed_text_is_a_memory_query() {
        assert_eq!(typed_query(&json!({"role":"user","content":[{"type":"text","text":"hello"},{"type":"image_url"}]})).as_deref(), Some("hello"));
        for message in [
            json!({"role":"tool","content":"hello"}),
            json!({"role":"user","content":[{"type":"tool_result","content":"hello"},{"type":"text","text":"hello"}]}),
            json!({"role":"user","origin":"task_notification","content":"hello"}),
            json!({"role":"user","origin":"peer","content":"hello"}),
            json!({"role":"user","content":"<task-notification>hello</task-notification>"}),
            json!({"role":"user","origin":"hook","content":"hello"}),
            json!({"role":"user","_is_summary":true,"content":"hello"}),
            json!({"role":"user","provider_content":{"summary":"hi"},"content":"hello"}),
        ] {
            assert_eq!(typed_query(&message), None, "{message}");
        }
    }
}
