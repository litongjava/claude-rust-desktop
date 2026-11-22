//! Transcript access retained for the lifetime of a tool executor.
use super::{SessionDb, Stores};
use crate::db::DbManager;
use crate::native_engine::mcp_output::{self, McpOutput};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub enum ExecutorHistory {
    Session(SessionDb),
    Shared(Arc<DbManager>, String),
}

impl Stores {
    pub(crate) fn listed_history(&self, id: &str) -> Result<Option<ExecutorHistory>> {
        self.global()
            .read_directory_row(id, |c| super::directory::one(c, id))?
            .map(|row| self.open_listed(&row).map(ExecutorHistory::Session))
            .transpose()
    }

    pub(crate) fn job_history(&self,session:&str,address:Option<&super::HistoryAddress>)->Result<ExecutorHistory>{
        let address=address.context("ToolScript job has no session incarnation")?;
        anyhow::ensure!(address.session==session,"ToolScript job belongs to another session");
        Ok(ExecutorHistory::Session(self.resume_history(address)?))
    }
    pub fn executor_history(&self, id: &str) -> Result<ExecutorHistory> {
        Ok(ExecutorHistory::Session(self.open_session(id)?))
    }
}

impl DbManager {
    pub(crate) fn job_history(self:&Arc<Self>,session:&str,address:Option<&super::HistoryAddress>)->Result<ExecutorHistory>{
        anyhow::ensure!(address.is_none(),"ToolScript job belongs to another data store");
        self.executor_history(session)
    }
    pub fn executor_history(self: &Arc<Self>, id: &str) -> Result<ExecutorHistory> {
        Ok(ExecutorHistory::Shared(self.clone(), id.into()))
    }
}

impl ExecutorHistory {
    pub(crate) fn address(&self)->Option<super::HistoryAddress>{
        match self {Self::Session(s)=>Some(s.address()), Self::Shared(..)=>None}
    }
    pub(crate) fn new_message_id(&self) -> String {
        match self {
            Self::Session(s) => s.new_message_id(),
            Self::Shared(..) => uuid::Uuid::new_v4().to_string(),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Session(session) => session.id(),
            Self::Shared(_, id) => id,
        }
    }

    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection, &str) -> Result<T>) -> Result<T> {
        match self {
            Self::Session(session) => session.read(|r| f(r.0, session.id())),
            Self::Shared(db, id) => db.with_conn(|c| f(c, id))?,
        }
    }

    /// Capture the history view that hooks can expose to a command. The snapshot
    /// has the same lifetime and incarnation as the tool that triggered it.
    pub(crate) fn write_hook_transcript(&self, out: &mut impl std::io::Write) -> Result<()> {
        self.read(|c,id| {
            let mut query = c.prepare_cached("SELECT role,id,created_at,content FROM messages WHERE conversation_id=?1 ORDER BY sort_order,rowid")?;
            let mut rows = query.query([id])?;
            while let Some(row) = rows.next()? {
                let role: String = row.get(0)?;
                serde_json::to_writer(&mut *out, &serde_json::json!({
                    "type":role,"uuid":row.get::<_,String>(1)?,"sessionId":id,"timestamp":row.get::<_,String>(2)?,
                    "message":{"role":role,"content":row.get::<_,String>(3)?}
                }))?;
                out.write_all(b"\n")?;
            }
            Ok(())
        })
    }

    pub(crate) fn restore(&self, messages: &mut Vec<Value>) -> Result<()> {
        self.read(|c, id| mcp_output::restore_history(messages, c, id))
    }

    pub(crate) fn loaded_tool_results(&self) -> Result<Vec<String>> {
        self.read(|c, id| {
            let mut query = c.prepare_cached(
                "SELECT coalesce(t.model_output,t.output) FROM tool_calls t JOIN messages m ON m.id=t.message_id
                 WHERE m.conversation_id=?1 AND m.role='assistant'
                   AND t.name='ToolLoad' AND t.is_error=0 AND t.output IS NOT NULL
                   AND m.sort_order >= COALESCE((SELECT MAX(sort_order) FROM messages
                       WHERE conversation_id=?1 AND is_compact_boundary=1), -1)
                 ORDER BY m.sort_order ASC, t.sort_order ASC",
            )?;
            let rows = query.query_map([id], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Finish the occurrence before publishing its display event. A later host
    /// flush can update ordering or input while reusing these exact bytes.
    pub(crate) fn complete_model_result(
        &self,
        message: &str,
        occurrence: &str,
        name: &str,
        input: &Value,
        output: &str,
        failed: bool,
        cwd: &std::path::Path,
    ) -> Result<Option<String>> {
        use rusqlite::params;
        let id = format!("{message}:{occurrence}");
        let save = |conn: &Connection| -> Result<Option<String>> {
            conn.execute("INSERT INTO messages(id,conversation_id,role,content,created_at,sort_order)
                VALUES(?1,?2,'assistant','',?3,(SELECT coalesce(max(sort_order),-1)+1 FROM messages WHERE conversation_id=?2))
                ON CONFLICT(id) DO NOTHING",params![message,self.id(),chrono::Utc::now().to_rfc3339()])?;
            anyhow::ensure!(
                conn.query_row(
                    "SELECT role='assistant' AND conversation_id=?2 FROM messages WHERE id=?1",
                    params![message, self.id()],
                    |r| r.get::<_, bool>(0)
                )?,
                "Result does not belong to this assistant message"
            );
            let order=conn.query_row("SELECT coalesce((SELECT sort_order FROM tool_calls WHERE id=?1),(SELECT coalesce(max(sort_order),-1)+1 FROM tool_calls WHERE message_id=?2))",params![id,message],|r|r.get(0))?;
            crate::db::message_repo::store_tool_call(
                conn,
                &id,
                message,
                name,
                Some(&input.to_string()),
                Some(output),
                failed,
                order,
                cwd,
            )?;
            Ok(conn.query_row(
                "SELECT model_output FROM tool_calls WHERE id=?",
                [&id],
                |r| r.get(0),
            )?)
        };
        match self {
            Self::Session(session) => session.write_named("complete tool result", |w| save(w.0)),
            Self::Shared(db, _) => db.with_conn(|conn| {
                crate::db::write_tx(
                    conn,
                    "complete tool result",
                    crate::db::STARTUP_WRITE_DEADLINE,
                    |tx| save(tx),
                )
            })?,
        }
    }

    pub(crate) fn save_mcp(
        &self,
        message: Option<&str>,
        provider_id: &str,
        output: &McpOutput,
        display: &str,
        failed: bool,
    ) -> Result<()> {
        match self {
            Self::Session(s) => s.save_mcp_output(
                message.context("Stored MCP result requires its assistant message")?,
                provider_id,
                output,
                display,
                failed,
            ),
            Self::Shared(db, id) => {
                db.with_conn(|c| output.save(c, id, provider_id, display, failed))?
            }
        }
    }

    pub(crate) fn search(&self, input: Value) -> Result<String> {
        match self {
            Self::Session(s) => s.context_search(input),
            Self::Shared(db, id) => crate::tools::context_search::execute(db, id, input),
        }
    }
}

/// Hook runners may select their session after their plugin configuration is loaded.
pub trait HookSessions: super::RecordingPolicies + Send + Sync {
    fn hook_history(self: Arc<Self>, id: &str) -> Result<Option<ExecutorHistory>>;
}
impl HookSessions for Stores {
    fn hook_history(self: Arc<Self>, id: &str) -> Result<Option<ExecutorHistory>> {
        self.listed_history(id)
    }
}
impl HookSessions for DbManager {
    fn hook_history(self: Arc<Self>, id: &str) -> Result<Option<ExecutorHistory>> {
        self.executor_history(id).map(Some)
    }
}

impl ExecutorHistory {
    pub fn execution_lease(
        &self,
    ) -> Result<Option<crate::services::session_messages::ExecutionLease>> {
        match self {
            Self::Session(s) => s.acquire_execution().map(Some),
            Self::Shared(db, id) => {
                crate::services::session_messages::ExecutionLease::acquire(db.data_dir(), id)
            }
        }
    }
    pub fn settle_abandoned(
        &self,
        lease: &crate::services::session_messages::ExecutionLease,
    ) -> Result<Vec<crate::db::turn_repo::TurnRecord>> {
        match self {
            Self::Session(s) => s.settle_abandoned(lease),
            Self::Shared(db, id) => {
                db.with_conn(|c| crate::db::turn_repo::settle_interrupted(c, id))?
            }
        }
    }
}
