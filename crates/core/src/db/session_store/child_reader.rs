//! A parent transcript records the exact child incarnation that produced each attempt.
use super::{SessionDb, Stores};
use crate::{
    db::DbManager,
    services::subagents::{self, Invocation},
};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

fn page(
    invocation: &Invocation,
    messages: Vec<Value>,
    status: &str,
    parent_exists: bool,
    offset: usize,
    limit: usize,
) -> Value {
    let mut result = json!({"invocation":invocation,"messages":messages,"status":status,"parent_exists":parent_exists});
    let cursor = subagents::snapshot_cursor(&result);
    let count = messages.len();
    let end = offset.saturating_add(limit).min(count);
    result["messages"] = json!(messages
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>());
    result["next_offset"] = if end < count { json!(end) } else { Value::Null };
    result["cursor"] = json!(cursor);
    result
}

fn legacy_snapshot(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    parent: &str,
    child: &str,
    offset: usize,
    limit: usize,
) -> anyhow::Result<Option<Value>> {
    let tx = conn.unchecked_transaction()?;
    let Some(mut invocation) = subagents::list_invocations(&tx, parent, None)?
        .into_iter()
        .rev()
        .find(|invocation| invocation.child_session_id == child)
    else {
        return Ok(None);
    };
    let parent_exists = crate::db::conversation_repo::get_conversation(&tx, parent)?.is_some();
    let child_exists = crate::db::conversation_repo::get_conversation(&tx, child)?.is_some();
    let status = if child_exists {
        subagents::authoritative_status(&tx, root, &invocation)?
    } else {
        "missing".into()
    };
    invocation.status = status.clone();
    let messages = crate::db::transcript::enriched_messages(&tx, child)?;
    Ok(Some(page(
        &invocation,
        messages,
        &status,
        parent_exists,
        offset,
        limit,
    )))
}

impl DbManager {
    pub fn invocation_list(&self, parent: &str, tool: Option<&str>) -> Result<Value> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let mut invocations = subagents::list_invocations(&tx, parent, None)?;
            let mut seen = std::collections::HashSet::new();
            for row in invocations.iter_mut().rev() {
                if seen.insert(row.child_session_id.clone()) {
                    row.status = subagents::authoritative_status(&tx, self.data_dir(), row)?;
                }
            }
            invocations.retain(|r| tool.is_none_or(|t| r.tool_use_id == t));
            Ok(json!({"invocations":invocations}))
        })?
    }
    pub fn invocation_page(
        &self,
        parent: &str,
        child: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Option<Value>> {
        self.with_conn(|c| legacy_snapshot(c, self.data_dir(), parent, child, offset, limit))?
    }
}

impl SessionDb {
    fn saved_invocations(&self) -> Result<Vec<(String, Invocation)>> {
        self.read(|r|{
            let mut q=r.0.prepare("SELECT attempt_key,child_incarnation,invocation FROM child_invocations ORDER BY rowid")?;
            let mut rows=q.query([])?;
            let mut out=Vec::new();
            while let Some(row)=rows.next()? {
                let inv:Invocation=serde_json::from_str(&row.get::<_,String>(2)?)?;
                ensure!(inv.parent_session_id==self.id && inv.key()==row.get::<_,String>(0)?,"Invalid persisted subagent association");
                out.push((row.get(1)?,inv));
            }
            Ok(out)
        })
    }
}

impl Stores {
    fn selected_child(&self, id: &str, inc: &str) -> Result<Option<SessionDb>> {
        self.global()
            .read_directory_row(id, |c| super::directory::one(c, id))?
            .filter(|entry| entry.incarnation == inc)
            .map(|entry| self.open_listed(&entry))
            .transpose()
    }

    fn child_result(
        &self,
        parent: &SessionDb,
        inc: &str,
        inv: &Invocation,
        with_history: bool,
    ) -> Result<(String, Vec<Value>)> {
        let Some(child) = self.selected_child(&inv.child_session_id, inc)? else {
            return Ok(("missing".into(), Vec::new()));
        };
        let (progress, messages) = child.read(|r| {
            let progress = if matches!(inv.status.as_str(), "completed" | "failed" | "cancelled")
                || inv.mode == "background"
            {
                None
            } else {
                r.setting(super::settings::CHILD_PROGRESS)?
                    .map(Value::Object)
            };
            let messages = if with_history {
                r.enriched_messages()?
            } else {
                Vec::new()
            };
            Ok((progress, messages))
        })?;
        let status = subagents::status_with_progress(
            self.data_dir(),
            inv,
            progress.as_ref(),
            parent.incarnation(),
            child.incarnation(),
        )?;
        Ok((status, messages))
    }
    pub fn invocation_list(&self, id: &str, tool: Option<&str>) -> Result<Value> {
        let Some(entry) = self
            .global()
            .read_directory_row(id, |c| super::directory::one(c, id))?
        else {
            return Ok(json!({"invocations":[]}));
        };
        let parent = self.open_listed(&entry)?;
        let mut rows = parent.saved_invocations()?;
        let mut seen = std::collections::HashSet::new();
        for (inc, inv) in rows.iter_mut().rev() {
            if seen.insert(inc.clone()) && tool.is_none_or(|t| inv.tool_use_id == t) {
                inv.status = self.child_result(&parent, inc, inv, false)?.0;
            }
        }
        parent.check_directory()?;
        let invocations: Vec<_> = rows
            .into_iter()
            .map(|(_, r)| r)
            .filter(|r| tool.is_none_or(|t| r.tool_use_id == t))
            .collect();
        Ok(json!({"invocations":invocations}))
    }

    pub fn invocation_page(
        &self,
        id: &str,
        child: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Option<Value>> {
        let Some(entry) = self
            .global()
            .read_directory_row(id, |c| super::directory::one(c, id))?
        else {
            return Ok(None);
        };
        let parent = self.open_listed(&entry)?;
        let Some((inc, mut inv)) = parent
            .saved_invocations()?
            .into_iter()
            .rev()
            .find(|(_, r)| r.child_session_id == child)
        else {
            return Ok(None);
        };
        let (status, rows) = self.child_result(&parent, &inc, &inv, true)?;
        inv.status = status.clone();
        parent.check_directory()?;
        Ok(Some(page(&inv, rows, &status, true, offset, limit)))
    }
}
