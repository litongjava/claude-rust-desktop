//! Directory rows contain the display fields needed before a session is opened.
use super::{GlobalDb, Result, SessionDb, Stores};
use crate::db::conversation_repo::{row_to_conversation, ConversationRow};
use rusqlite::{params_from_iter, types::Value, OptionalExtension};

#[derive(Default)]
pub struct DirectoryFilter<'a> {
    pub project: Option<&'a str>,
    pub workspace: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub archived: Option<bool>,
    pub pinned: Option<bool>,
    pub nonempty: bool,
    pub exclude: Option<&'a str>,
}

pub struct DirectoryEntry {
    pub conversation: ConversationRow,
    pub incarnation: String,
    pub first_prompt: Option<String>,
    pub latest_prompt: Option<String>,
    pub git_branch: Option<String>,
    pub transcript_bytes: i64,
    pub last_turn_status: Option<String>,
    pub content_revision: i64,
}
const FIELDS: &str = "SELECT id, title, model, provider, workspace_path, project_id,
    research_mode, pinned, archived, created_at, updated_at, message_count, workspace_mode,
    incarnation, first_prompt, latest_prompt, git_branch, transcript_bytes, last_turn_status,
    content_revision FROM conversations";
fn entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<DirectoryEntry> {
    let mut conversation = row_to_conversation(row)?;
    let first_prompt: Option<String> = row.get(14)?;
    if conversation
        .title
        .as_deref()
        .is_none_or(|title| title.trim().is_empty() || title.eq_ignore_ascii_case("CLI session"))
    {
        if let Some(prompt) = &first_prompt {
            conversation.title = Some(prompt.clone());
        }
    }
    Ok(DirectoryEntry {
        conversation,
        incarnation: row.get(13)?,
        first_prompt,
        latest_prompt: row.get(15)?,
        git_branch: row.get(16)?,
        transcript_bytes: row.get(17)?,
        last_turn_status: row.get(18)?,
        content_revision: row.get(19)?,
    })
}

impl GlobalDb {
    pub fn list_sessions(&self, filter: DirectoryFilter<'_>) -> Result<Vec<DirectoryEntry>> {
        let mut sql = String::from(FIELDS);
        let mut conditions = Vec::new();
        let mut args = Vec::<Value>::new();
        for (predicate, value) in [
            ("project_id=?", filter.project),
            ("workspace_path=?", filter.workspace),
            ("workspace_mode=?", filter.mode),
            ("id<>?", filter.exclude),
        ] {
            if let Some(value) = value {
                conditions.push(predicate);
                args.push(value.to_owned().into());
            }
        }
        for (predicate, value) in [("archived=?", filter.archived), ("pinned=?", filter.pinned)] {
            if let Some(value) = value {
                conditions.push(predicate);
                args.push(i64::from(value).into());
            }
        }
        if filter.nonempty {
            conditions.push("message_count>0");
        }
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        sql.push_str(" ORDER BY updated_at DESC,id");
        self.read(|conn| {
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params_from_iter(args), entry)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
    pub fn conversation(&self, id: &str) -> Result<Option<ConversationRow>> {
        self.read_directory_row(id, |c| {
            Ok(c.query_row(&format!("{FIELDS} WHERE id=?1"), [id], entry)
                .optional()?
                .map(|r| r.conversation))
        })
    }
}
impl Stores {
    /// An enumerated row can be deleted and its logical ID reused before selection.
    pub fn open_listed(&self, row: &DirectoryEntry) -> Result<SessionDb> {
        let s = self.open_session(&row.conversation.id)?;
        anyhow::ensure!(
            s.incarnation() == row.incarnation,
            "Listed session was replaced"
        );
        Ok(s)
    }
}
