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

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct IndexState {
    #[serde(default)]
    epoch: u64,
    first: Option<String>,
    #[serde(default)]
    first_hash: Option<String>,
}

const INDEX: Setting<IndexState> = Setting::new("memory.prompt.index");
const EPOCH: Setting<u64> = Setting::new("memory.prompt.epoch");
const CHANGE: &str = "MEMORY.md changed. The current index follows.\n";
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

impl FileMemory {
    pub(super) fn render_index(&self, snapshot: Option<&crate::memory::files::Snapshot>) -> String {
        let index = snapshot.map_or("", |s| s.index.as_str());
        let path = xml(&self.directory.join("MEMORY.md").to_string_lossy());
        format!("<system-reminder>\n<project_memory>\nIndex: {path}\n{}\n{}\n</project_memory>\n</system-reminder>",
            include_str!("../../prompt/file_memory.md"), xml(&index_excerpt(index)))
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Receipt {
    /// Older receipts predate tracking delivery to a provider.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observation: Option<String>,
    #[serde(default)]
    epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    announced: Option<bool>,
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

fn index_key(boundary: Option<&str>) -> Setting<IndexState> {
    boundary.map_or(INDEX, |id| {
        Setting::memory_receipt(format!("prompt.index.boundary.{id}"))
    })
}

pub(crate) fn freeze_empty_boundary(tx: &rusqlite::Connection, id: &str) -> Result<()> {
    index_key(Some(id)).write(tx, &IndexState::default())
}

fn observation(hash: &str) -> Setting<IndexState> {
    Setting::memory_receipt(format!("prompt.observation.{hash}"))
}

impl IndexState {
    pub(crate) fn seal(&self, c: &rusqlite::Connection, id: &str) -> Result<()> {
        let mut frozen = self.clone();
        frozen.epoch = EPOCH.read(c)?.unwrap_or_default() + 1;
        EPOCH.write(c, &frozen.epoch)?;
        index_key(Some(id)).write(c, &frozen)
    }

    fn announcement(&self) -> Option<String> {
        self.first.as_ref().map(|index| {
            index.replacen("<project_memory>", &format!("{CHANGE}<project_memory>"), 1)
        })
    }
}

// Physical row order matches model replay. Tool-addressed inputs follow their
// parent, in tool and payload order. The same positions drive live admission.
pub(crate) const INDEX_HISTORY: &str = "WITH history(id,pos,seq,tool,tool_seq,part,boundary) AS (
    SELECT m.id,m.sort_order,m.rowid,-1,0,0,m.is_compact_boundary FROM messages m
    LEFT JOIN message_metadata meta ON meta.message_id=m.id
    WHERE m.is_compact_boundary OR (m.role='user' AND json_extract(meta.value,'$.promptOrigin') IN ('typed','user'))
    UNION ALL
    SELECT json_extract(j.value,'$.message_id'),m.sort_order,m.rowid,t.sort_order,t.rowid,j.key,0
    FROM messages m JOIN tool_calls t ON t.message_id=m.id
    JOIN tool_queued_inputs q ON q.tool_call_id=t.id JOIN json_each(q.inputs_json) j
    WHERE m.role='assistant' AND json_extract(j.value,'$.prompt_origin')='typed'
    AND NOT EXISTS(SELECT 1 FROM tool_call_origins n WHERE n.tool_call_id=t.id)
)";

/// Rebuild the announcements from immutable observations. A boundary supersedes
/// observations made before it was sealed, for as long as that boundary precedes them.
pub(crate) fn reorder_receipts(c: &rusqlite::Connection) -> Result<()> {
    let mut last = INDEX.read(c)?.unwrap_or_default();
    let mut boundary_epoch = last.epoch;
    let mut q = c.prepare(&format!(
        "{INDEX_HISTORY} SELECT id,boundary FROM history ORDER BY pos,seq,tool,tool_seq,part"
    ))?;
    let positions = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, boundary) in positions {
        if boundary {
            last = index_key(Some(&id)).read(c)?.unwrap_or_default();
            boundary_epoch = last.epoch;
            continue;
        }
        let key = receipt(&id);
        let Some(mut saved) = key.read(c)? else {
            continue;
        };
        let before = saved.clone();
        let original = match saved.observation.as_deref() {
            Some("") => IndexState::default(),
            Some(hash) => observation(hash)
                .read(c)?
                .ok_or_else(|| anyhow::anyhow!("Missing stored memory index observation"))?,
            None => {
                // Upgrade pre-observation receipts while their old order is still available.
                let state = saved
                    .blocks
                    .iter()
                    .find(|b| b.starts_with("<system-reminder>\nMEMORY.md changed."))
                    .map(|block| IndexState {
                        first: Some(block.replacen(CHANGE, "", 1)),
                        first_hash: saved.index_hash.clone(),
                        epoch: 0,
                    })
                    .unwrap_or_else(|| last.clone());
                if let Some(hash) = &state.first_hash {
                    observation(hash).write(c, &state)?;
                    saved.observation = Some(hash.clone());
                }
                state
            }
        };
        saved
            .blocks
            .retain(|b| !b.starts_with("<system-reminder>\nMEMORY.md changed."));
        saved.index_hash = None;
        saved.announced = Some(false);
        if saved.epoch >= boundary_epoch && original.first_hash != last.first_hash {
            if let Some(block) = original.announcement() {
                saved.blocks.insert(0, block);
                saved.index_hash = original.first_hash.clone();
                saved.announced = Some(true);
                last = original;
            }
        }
        if saved != before {
            key.write(c, &saved)?;
        }
    }
    Ok(())
}

pub(crate) fn copy_prefix(src: &rusqlite::Connection, dst: &rusqlite::Connection) -> Result<()> {
    if let Some(epoch) = EPOCH.read(src)? {
        EPOCH.write(dst, &epoch)?;
    }
    if let Some(prefix) = INDEX.read(src)? {
        INDEX.write(dst, &prefix)?;
    }
    Ok(())
}

pub(crate) fn copy_message(
    src: &rusqlite::Connection,
    dst: &rusqlite::Connection,
    old: &str,
    new: &str,
) -> Result<()> {
    if let Some(saved) = receipt(old).read(src)? {
        if let Some(hash) = &saved.observation {
            if let Some(state) = observation(hash).read(src)? {
                observation(hash).write(dst, &state)?;
            }
        }
        receipt(new).write(dst, &saved)?;
    }
    if let Some(index) = index_key(Some(old)).read(src)? {
        index_key(Some(new)).write(dst, &index)?;
    }
    Ok(())
}

/// Resolve directory authority before entering the shard writer. File observation,
/// receipt selection and the message insert then share one transaction.
pub(crate) struct Admission {
    memory: Option<FileMemory>,
    top: usize,
}

impl Admission {
    fn context(stores: crate::db::session_store::Stores) -> Result<super::ServiceContext> {
        Ok(super::ServiceContext {
            paths: crate::runtime::RuntimePaths::new(stores.data_dir().into())?,
            credentials: std::sync::Arc::new(crate::services::EnvironmentCredentials),
            db: std::sync::Arc::new(stores),
        })
    }

    pub(crate) fn session(shard: &SessionDb) -> Result<Self> {
        let context = Self::context(shard.scoped_stores())?;
        let config = super::config(&context)?;
        let memory = config
            .enabled
            .then(|| FileMemory::from_session(&context, shard.clone(), None))
            .transpose()?;
        Ok(Self {
            memory,
            top: config.search_top_k,
        })
    }

    pub(crate) fn starting(
        stores: &crate::db::session_store::Stores,
        roots: &[std::path::PathBuf],
        owner: &str,
    ) -> Result<Self> {
        let context = Self::context(stores.clone())?;
        let config = super::config(&context)?;
        let memory = if config.enabled {
            let scope = if super::private_roots(roots) {
                super::MemoryScope::private(owner)?
            } else {
                super::MemoryScope::project(roots, true)?
            };
            Some(FileMemory::new(&context, scope, None)?)
        } else {
            None
        };
        Ok(Self {
            memory,
            top: config.search_top_k,
        })
    }

    fn observe(&self) -> Result<(IndexState, Option<super::Snapshot>)> {
        let Some(memory) = &self.memory else {
            return Ok((IndexState::default(), None));
        };
        let snapshot = if memory.directory.try_exists()? {
            Some(
                super::FileMemoryStore::open_read_only(
                    &memory.context.paths.data_dir,
                    &memory.scope,
                )?
                .prompt_snapshot()?,
            )
        } else {
            None
        };
        let state = IndexState {
            epoch: 0,
            first: Some(memory.render_index(snapshot.as_ref())),
            first_hash: Some(format!(
                "{}/{}",
                memory.scope.key(),
                snapshot.as_ref().map_or("", |s| s.index_hash.as_str())
            )),
        };
        Ok((state, snapshot))
    }

    pub(crate) fn freeze(&self, tx: &rusqlite::Connection, boundary: Option<&str>) -> Result<()> {
        let index = self.frozen_index()?;
        match boundary {
            Some(id) => index.seal(tx, id),
            None => INDEX.write(tx, &index),
        }
    }

    pub(crate) fn frozen_index(&self) -> Result<IndexState> {
        Ok(self.observe()?.0)
    }

    pub(crate) fn first(&self, tx: &rusqlite::Connection, id: &str, content: &Value) -> Result<()> {
        let (current, snapshot) = self.observe()?;
        INDEX.write(tx, &current)?;
        self.save_receipt(tx, id, content, current, snapshot)
    }

    pub(crate) fn typed(&self, tx: &rusqlite::Connection, id: &str, content: &Value) -> Result<()> {
        let (current, snapshot) = self.observe()?;
        self.save_receipt(tx, id, content, current, snapshot)
    }

    fn save_receipt(
        &self,
        tx: &rusqlite::Connection,
        id: &str,
        content: &Value,
        current: IndexState,
        snapshot: Option<super::Snapshot>,
    ) -> Result<()> {
        let (boundary, changed_hash) = crate::db::session_store::visible_memory_index(tx, id)?;
        let frozen = index_key(boundary.as_deref()).read(tx)?.unwrap_or_default();
        let previous = changed_hash.or(frozen.first_hash);
        let changed = previous.map_or_else(
            || frozen.first != current.first,
            |h| Some(h) != current.first_hash,
        );
        let mut blocks = Vec::new();
        if changed {
            blocks.extend(current.announcement());
        }
        if let (Some(memory), Some(snapshot)) = (&self.memory, snapshot) {
            let query = query_text(content);
            if let Some(text) = memory.recall_files(&snapshot, &query, 4096, self.top)? {
                blocks.push(format!("<system-reminder>\n{text}</system-reminder>"));
            }
        }
        if let Some(hash) = &current.first_hash {
            if observation(hash).read(tx)?.is_none() {
                observation(hash).write(tx, &current)?;
            }
        }
        receipt(id).write(
            tx,
            &Receipt {
                pending: true,
                epoch: EPOCH.read(tx)?.unwrap_or_default(),
                observation: Some(current.first_hash.clone().unwrap_or_default()),
                announced: Some(changed && current.first.is_some()),
                index_hash: if changed { current.first_hash } else { None },
                blocks,
            },
        )
    }
}

fn query_text(content: &Value) -> Cow<'_, str> {
    match content.as_str() {
        Some(text) => Cow::Borrowed(text),
        None => Cow::Owned(
            content
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    }
}

/// A summary can retain user text or opaque user items. Include stored recall
/// there, while reserving the single index for the newly committed boundary.
pub(crate) fn summary_blocks(c: &rusqlite::Connection, id: &str) -> Result<Vec<String>> {
    Ok(receipt(id)
        .read(c)?
        .into_iter()
        .flat_map(|r| r.blocks)
        .filter(|b| !b.starts_with("<system-reminder>\nMEMORY.md changed."))
        .collect())
}

pub(crate) fn attach_summary_recall(
    conn: &rusqlite::Connection,
    messages: &mut [Value],
) -> Result<()> {
    for message in messages {
        if message["origin"] != "typed" {
            continue;
        }
        if let Some(id) = message["id"].as_str() {
            let blocks = summary_blocks(conn, id)?;
            if !blocks.is_empty() {
                let content = &mut message["content"];
                if !content.is_array() {
                    *content = serde_json::json!([{"type":"text","text":content}]);
                }
                content.as_array_mut().unwrap().extend(
                    blocks
                        .into_iter()
                        .map(|text| serde_json::json!({"type":"text","text":text})),
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn replay(
    shard: &SessionDb,
    boundary: Option<&str>,
    messages: &[Value],
) -> Result<(Option<String>, Vec<Entry>)> {
    shard.read(|r| replay_at(r, boundary, messages))
}

pub(crate) fn replay_at(
    r: &crate::db::session_store::SessionRead<'_>,
    boundary: Option<&str>,
    messages: &[Value],
) -> Result<(Option<String>, Vec<Entry>)> {
    let prefix = r.setting(index_key(boundary))?.and_then(|s| s.first);
    let mut entries = Vec::new();
    for message in messages
        .iter()
        .filter(|m| m["role"] == "user" && m["origin"] == "typed")
    {
        if let Some(id) = message["id"].as_str() {
            if let Some(saved) = r.setting(receipt(id))? {
                if !saved.blocks.is_empty() {
                    entries.push(attachment(id, saved.blocks));
                }
            }
        }
    }
    Ok((prefix, entries))
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
        assert_eq!(
            query_text(&json!([{"type":"text","text":"hello"},{"type":"image_url"}])),
            "hello"
        );
        assert_eq!(
            query_text(&json!("<task-notification>human input")),
            "<task-notification>human input"
        );
    }
}
