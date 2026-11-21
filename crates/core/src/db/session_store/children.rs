//! Foreground children retain the chosen shard until their final progress update.
use super::{owned::Backend, ExecutorHistory, SessionRepository};
use crate::db::{message_repo, request_repo};
use anyhow::{ensure, Context, Result};
use rusqlite::params;
use serde_json::Value;

impl ExecutorHistory {
    fn child_repo(&self) -> SessionRepository<'_> {
        match self {
            Self::Session(s) => SessionRepository(Backend::Admitted(s.clone())),
            Self::Shared(db, id) => SessionRepository(Backend::Shared(db, id)),
        }
    }
    pub(crate) fn save_child_state(&self, value: &Value) -> Result<()> {
        let progress = value
            .as_object()
            .context("Expected a child progress record")?;
        ensure!(
            value["session_id"].as_str() == Some(self.id()),
            "Child progress belongs to another session"
        );
        self.child_repo()
            .set_preference(super::settings::CHILD_PROGRESS, Some(progress))
    }
    pub(crate) fn child_segment(&self, message: &str, text: &str, thinking: &str) -> Result<()> {
        self.child_repo().write("save child segment",|c,id|{
            let changed=c.execute("INSERT INTO messages(id,conversation_id,role,content,thinking,created_at,sort_order)
                VALUES(?1,?2,'assistant',?3,?4,?5,(SELECT coalesce(max(sort_order),-1)+1 FROM messages WHERE conversation_id=?2))
                ON CONFLICT(id) DO UPDATE SET content=excluded.content,thinking=excluded.thinking
                WHERE messages.conversation_id=excluded.conversation_id AND messages.role='assistant'",
                params![message,id,text,thinking,chrono::Utc::now().to_rfc3339()])?;
            ensure!(changed==1,"Child segment identity changed");
            Ok(())
        })
    }
    pub(crate) fn child_stream_tool(
        &self,
        message: &str,
        text: &str,
        row: &message_repo::ToolCallRow,
    ) -> Result<()> {
        ensure!(
            row.message_id == message,
            "Child tool belongs to another message"
        );
        match self {
            Self::Session(s) => s.write_named("save child tool", |w| {
                message_repo::upsert_stream_message(w.0, w.1, message, text)?;
                w.tool(row, None)
            }),
            Self::Shared(db, id) => {
                db.with_conn(|c| message_repo::save_stream_tool_result(c, id, message, text, row))?
            }
        }
    }
    pub(crate) fn child_request(&self, message: &str, snapshot: &Value) -> Result<u64> {
        self.child_repo().write("save child request",|c,id|{
            c.execute("INSERT OR IGNORE INTO messages(id,conversation_id,role,content,created_at,sort_order)
                VALUES(?1,?2,'assistant','',?3,(SELECT coalesce(max(sort_order),-1)+1 FROM messages WHERE conversation_id=?2))",
                params![message,id,chrono::Utc::now().to_rfc3339()])?;
            ensure!(c.prepare_cached("SELECT 1 FROM messages WHERE id=?1 AND conversation_id=?2 AND role='assistant'")?.exists([message,id])?,"Child request message identity changed");
            request_repo::save_request(c,message,snapshot)?;
            let mut q=c.prepare_cached("SELECT output_tokens FROM model_requests WHERE conversation_id=?1 AND output_tokens IS NOT NULL")?;
            let mut total=0u64;
            for n in q.query_map([id],|r|r.get::<_,u64>(0))? {total=total.saturating_add(n?);}
            Ok(total)
        })
    }
}

impl<D: super::SessionAccess> crate::services::ServiceContext<D> {
    pub fn child_progress(&self, id: &str) -> Result<Option<Value>> {
        self.db
            .select_session(id)?
            .preference(super::settings::CHILD_PROGRESS)
            .map(|progress| progress.map(Value::Object))
    }
    pub fn child_description(&self, id: &str) -> Result<Option<String>> {
        self.db
            .select_session(id)?
            .preference(super::settings::AGENT_DESCRIPTION)
    }
    pub(crate) fn set_child_description(&self, id: &str, description: &str) -> Result<()> {
        self.db.select_session(id)?.set_preference(
            super::settings::AGENT_DESCRIPTION,
            Some(&description.to_owned()),
        )
    }
}
