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
    pub fn executor_history(&self, id: &str) -> Result<ExecutorHistory> {
        Ok(ExecutorHistory::Session(self.open_session(id)?))
    }
}

impl DbManager {
    pub fn executor_history(self: &Arc<Self>, id: &str) -> Result<ExecutorHistory> {
        Ok(ExecutorHistory::Shared(self.clone(), id.into()))
    }
}

impl ExecutorHistory {
    pub fn id(&self) -> &str {
        match self {
            Self::Session(session) => session.id(),
            Self::Shared(_, id) => id,
        }
    }

    fn read<T>(&self, f: impl FnOnce(&Connection, &str) -> Result<T>) -> Result<T> {
        match self {
            Self::Session(session) => session.read(|r| f(r.0, session.id())),
            Self::Shared(db, id) => db.with_conn(|c| f(c, id))?,
        }
    }

    pub(crate) fn restore(&self, messages: &mut Vec<Value>) -> Result<()> {
        self.read(|c, id| mcp_output::restore_history(messages, c, id))
    }

    pub(crate) fn loaded_tool_results(&self) -> Result<Vec<String>> {
        self.read(|c, id| {
            let mut query = c.prepare_cached(
                "SELECT t.output FROM tool_calls t JOIN messages m ON m.id=t.message_id
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
