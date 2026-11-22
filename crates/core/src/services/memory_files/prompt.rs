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

#[derive(Default, Serialize, Deserialize)]
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

impl FileMemory {
    pub(super) fn render_index(&self, snapshot: Option<&crate::memory::files::Snapshot>) -> String {
        let index = snapshot.map_or("", |s| s.index.as_str());
        let path = xml(&self.directory.join("MEMORY.md").to_string_lossy());
        format!("<system-reminder>\n<project_memory>\nIndex: {path}\n{}\n{}\n</project_memory>\n</system-reminder>",
            include_str!("../../prompt/file_memory.md"), xml(&index_excerpt(index)))
    }
}

#[derive(Serialize, Deserialize)]
struct Receipt {
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

pub(crate) fn copy_prefix(src: &rusqlite::Connection, dst: &rusqlite::Connection) -> Result<()> {
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
        index_key(boundary).write(tx, &self.observe()?.0)
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
            if let Some(index) = &current.first {
                blocks.push(index.replacen(
                    "<project_memory>",
                    "MEMORY.md changed. The current index follows.\n<project_memory>",
                    1,
                ));
            }
        }
        if let (Some(memory), Some(snapshot)) = (&self.memory, snapshot) {
            let query = query_text(content);
            if let Some(text) = memory.recall_files(&snapshot, &query, 4096, self.top)? {
                blocks.push(format!("<system-reminder>\n{text}</system-reminder>"));
            }
        }
        receipt(id).write(
            tx,
            &Receipt {
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

pub(crate) fn replay(
    shard: &SessionDb,
    boundary: Option<&str>,
    messages: &[Value],
) -> Result<(Option<String>, Vec<Entry>)> {
    shard.read(|r| {
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
    })
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
