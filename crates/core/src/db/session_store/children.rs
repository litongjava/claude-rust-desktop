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
    pub(crate) fn invocation_message(&self, requested: Option<&str>) -> Result<String> {
        use rusqlite::OptionalExtension;
        self.read(|c, id| {
            let selected = c
                .query_row(
                    "SELECT id FROM messages WHERE conversation_id=?1 AND role='assistant'
                AND (?2 IS NULL OR id=?2) ORDER BY sort_order DESC,rowid DESC LIMIT 1",
                    params![id, requested],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            // Shared-store clients historically allowed an association before the first segment.
            if selected.is_none() && requested.is_none() && matches!(self, Self::Shared(..)) {
                return Ok(String::new());
            }
            selected.context("Invocation parent assistant segment is missing")
        })
    }

    pub(crate) fn save_invocation(
        &self,
        child: &ExecutorHistory,
        row: &crate::services::subagents::Invocation,
    ) -> Result<()> {
        ensure!(
            self.id() == row.parent_session_id && child.id() == row.child_session_id,
            "Child invocation owner changed"
        );
        match (self, child) {
            (Self::Session(parent), Self::Session(child)) => parent
                .write_named("save child invocation", |w| {
                    w.child_invocation(&child.address(), row)
                }),
            (Self::Shared(..), Self::Shared(..)) => {
                let raw = serde_json::to_string(row)?;
                self.child_repo().write("save child invocation", |c, _| {
                    super::state::set_local(c, &row.key(), &raw)
                })
            }
            _ => anyhow::bail!("Child uses another history store"),
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

impl super::Stores {
    pub(crate) fn fork_agent(&self, parent: &str, title: &str, directive: &str) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.open_session(parent)?
            .branch(&id, super::Branch::AgentFork { title, directive })?;
        Ok(id)
    }
    pub(crate) fn is_agent_fork(&self, session: &str) -> Result<bool> {
        self.open_session(session)?
            .read(|r| Ok(r.setting(super::settings::AGENT_FORK_PARENT)?.is_some()))
    }
}
impl crate::db::DbManager {
    pub(crate) fn fork_agent(&self, parent: &str, title: &str, directive: &str) -> Result<String> {
        let id = self.branch_conversation(parent, None, None)?;
        self.rename_session(&id, title)?;
        self.with_conn(|c| {
            crate::db::write_tx(
                c,
                "record agent fork",
                crate::db::STARTUP_WRITE_DEADLINE,
                |tx| {
                    super::state::set_local(
                        tx,
                        &format!("fork-of.{id}"),
                        &serde_json::json!({"parent":parent}).to_string(),
                    )?;
                    crate::services::permission_authority::record_conn(
                        tx,
                        &id,
                        crate::services::permission_authority::Source::User,
                        &serde_json::json!(directive),
                    )
                },
            )
        })??;
        Ok(id)
    }
    pub(crate) fn is_agent_fork(&self, session: &str) -> Result<bool> {
        self.with_conn(|c| Ok(super::state::local(c, &format!("fork-of.{session}"))?.is_some()))?
    }
}
