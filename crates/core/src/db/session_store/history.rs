//! Session-bound repository adapters. A callback cannot carry its connection
//! into another store or outlive the lifecycle lock.
use super::{GlobalDb, SessionDb, SessionRead, SessionWrite};
use crate::db::{diff_repo, message_repo as messages, permission_repo, turn_repo};
use crate::native_engine::mcp_output::McpOutput;
use crate::services::{input_queue, permission_authority};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub revision: i64,
    pub count: i64,
    pub activity: String,
    pub status: Option<String>,
    pub first_prompt: Option<String>,
    pub latest_prompt: Option<String>,
    pub transcript_bytes: i64,
    pub git_branch: Option<String>,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

pub(super) fn projection(c: &Connection) -> Result<Projection> {
    Ok(c.query_row("SELECT content_revision,message_count,updated_at,last_turn_status,
        first_prompt,latest_prompt,transcript_bytes,(SELECT value FROM peripheral_settings WHERE key='git.branch'),
        request_count,CAST(input_tokens AS INTEGER),CAST(output_tokens AS INTEGER),CAST(total_tokens AS INTEGER)
        FROM manifest JOIN projection_state USING(singleton) WHERE singleton=1", [], Projection::from_row)?)
}

// SQLite clamps decimal TEXT to the signed column range on the final CAST.
// Keep the exact totals locally so deleting a large request can undo a clamp.
pub(super) fn flush_summary(c: &Connection) -> Result<()> {
    let (dirty, history_dirty): (bool, bool) = c.query_row(
        "SELECT prompts_dirty,history_edit IS NOT NULL FROM projection_state WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if dirty {
        let first: Option<String> = c.query_row("SELECT content FROM messages WHERE role='user' AND content<>'' ORDER BY sort_order,rowid LIMIT 1", [], |r|r.get(0)).optional()?;
        c.execute("UPDATE projection_state SET first_prompt=?1,
            latest_prompt=(SELECT substr(content,1,80) FROM messages WHERE role='user' AND content<>'' ORDER BY sort_order DESC,rowid DESC LIMIT 1),
            prompts_dirty=0,history_edit=NULL WHERE singleton=1", [first.as_deref().and_then(crate::db::conversation_repo::prompt_title)])?;
    } else if history_dirty {
        c.execute(
            "UPDATE projection_state SET history_edit=NULL WHERE singleton=1",
            [],
        )?;
    }
    let mut stmt = c.prepare(
        "SELECT direction,coalesce(input,0),coalesce(output,0),total FROM request_metric_deltas",
    )?;
    let mut rows = stmt.query([])?;
    let mut delta = [0i128; 4];
    let mut changed = false;
    while let Some(r) = rows.next()? {
        let sign = i128::from(r.get::<_, i64>(0)?);
        let input = i128::from(r.get::<_, i64>(1)?);
        let output = i128::from(r.get::<_, i64>(2)?);
        let total = r
            .get::<_, Option<i64>>(3)?
            .map(i128::from)
            .unwrap_or(input + output);
        for (sum, value) in delta.iter_mut().zip([1, input, output, total]) {
            *sum += sign * value;
        }
        changed = true;
    }
    if changed {
        let stored: [String; 3] = c.query_row("SELECT input_tokens,output_tokens,total_tokens FROM projection_state WHERE singleton=1", [], |r|Ok([r.get(0)?,r.get(1)?,r.get(2)?]))?;
        for (sum, prior) in delta[1..].iter_mut().zip(stored) {
            *sum += prior.parse::<i128>()?;
        }
        c.execute("UPDATE projection_state SET request_count=request_count+?1,input_tokens=?2,output_tokens=?3,total_tokens=?4 WHERE singleton=1",
            params![i64::try_from(delta[0])?,delta[1].to_string(),delta[2].to_string(),delta[3].to_string()])?;
        c.execute("DELETE FROM request_metric_deltas", [])?;
    }
    Ok(())
}

impl Projection {
    pub(super) fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Projection {
            revision: r.get(0)?,
            count: r.get(1)?,
            activity: r.get(2)?,
            status: r.get(3)?,
            first_prompt: r
                .get::<_, Option<String>>(4)?
                .as_deref()
                .and_then(crate::db::conversation_repo::prompt_title),
            latest_prompt: r.get(5)?,
            transcript_bytes: r.get(6)?,
            git_branch: r.get(7)?,
            request_count: r.get(8)?,
            input_tokens: r.get(9)?,
            output_tokens: r.get(10)?,
            total_tokens: r.get(11)?,
        })
    }
}

// Compare instants including offsets and sub-millisecond precision. Keep the
// original representation of the winning timestamp (including imported text).
fn latest_activity<'a>(existing: &'a str, incoming: &'a str) -> &'a str {
    if chrono::DateTime::parse_from_rfc3339(existing).ok()
        >= chrono::DateTime::parse_from_rfc3339(incoming).ok()
    {
        existing
    } else {
        incoming
    }
}

pub(super) fn advance(c: &Connection) -> Result<()> {
    flush_summary(c)?;
    let existing: String = c.query_row(
        "SELECT updated_at FROM manifest WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    c.execute(
        "UPDATE manifest SET updated_at=?1,content_revision=content_revision+1,
        message_count=(SELECT count(*) FROM messages),
        last_turn_status=(SELECT status FROM execution_turns ORDER BY started_at DESC,rowid DESC LIMIT 1)
        WHERE singleton=1",
        [latest_activity(&existing, &chrono::Utc::now().to_rfc3339())],
    )?;
    Ok(())
}

impl GlobalDb {
    // Projection is secondary. Never wait for the G writer after a local commit.
    pub(super) fn project(&self, id: &str, incarnation: &str, p: &Projection) -> Result<()> {
        self.checkout(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE, true, |c| {
            c.pragma_update(None, "synchronous", "NORMAL")?;
            c.busy_timeout(std::time::Duration::ZERO)?;
            let result = (|| -> Result<()> {
                let tx = rusqlite::Transaction::new_unchecked(
                    c,
                    rusqlite::TransactionBehavior::Immediate,
                )?;
                let current = tx
                    .query_row(
                        "SELECT updated_at,content_revision FROM conversations WHERE id=?1 AND incarnation=?2",
                        params![id, incarnation],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                    )
                    .optional()?;
                let Some((existing, revision)) = current else {
                    return Ok(());
                };
                let activity = latest_activity(&existing, &p.activity);
                if p.revision <= revision {
                    if activity != existing {
                        tx.execute(
                            "UPDATE conversations SET updated_at=?1 WHERE id=?2 AND incarnation=?3",
                            params![activity, id, incarnation],
                        )?;
                    }
                    tx.commit()?;
                    return Ok(());
                }
                tx.execute(
                    "UPDATE conversations SET content_revision=?1,message_count=?2,updated_at=?3,
                last_turn_status=?4,first_prompt=?5,latest_prompt=?6,transcript_bytes=?7, git_branch=?10, request_count=?11,input_tokens=?12,output_tokens=?13,total_tokens=?14
                WHERE id=?8 AND incarnation=?9 AND content_revision<?1",
                    params![
                        p.revision,
                        p.count,
                        activity,
                        p.status,
                        p.first_prompt,
                        p.latest_prompt,
                        p.transcript_bytes,
                        id,
                        incarnation, p.git_branch,p.request_count,p.input_tokens,p.output_tokens,p.total_tokens
                    ],
                )?;
                tx.commit()?;
                Ok(())
            })();
            c.busy_timeout(std::time::Duration::from_secs(5))?;
            result?;
            Ok(())
        })
    }
}

fn stream_occurrence_id(message: &str, provider_id: &str) -> String {
    format!("{message}:{provider_id}")
}

impl SessionDb {
    pub fn context_search(&self, input: Value) -> Result<String> {
        crate::tools::context_search::search(&self.incarnation, input, |selection| {
            self.read(|r| crate::db::context_search_repo::read_snapshot(r.0, &self.id, selection))
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Save to the same occurrence that a later assistant stream flush will address.
    pub fn save_mcp(
        &self,
        message: &str,
        provider_id: &str,
        transport: &Value,
        failed: bool,
    ) -> Result<String> {
        let id = stream_occurrence_id(message, provider_id);
        let output = McpOutput::from_transport(transport)?;
        let display = output.display();
        self.write_named("save MCP output", |tx| {
            output.save(tx.0, &self.id, &id, &display, failed)?;
            Ok(())
        })?;
        Ok(display)
    }
}

impl SessionRead<'_> {
    pub fn enriched_messages(&self) -> Result<Vec<Value>> {
        let id: String =
            self.0
                .query_row("SELECT id FROM manifest WHERE singleton=1", [], |r| {
                    r.get(0)
                })?;
        crate::db::transcript::enriched_messages(self.0, &id)
    }
    pub fn messages(&self) -> Result<Vec<messages::MessageRow>> {
        let id: String = self
            .0
            .query_row("SELECT id FROM manifest", [], |r| r.get(0))?;
        messages::get_messages_by_conversation(self.0, &id)
    }
    pub fn projection(&self) -> Result<Projection> {
        projection(self.0)
    }
    pub fn turn(&self, id: &str) -> Result<Option<turn_repo::TurnRecord>> {
        Ok(turn_repo::get_turn(self.0, id)?)
    }
    pub fn segments(&self, id: &str) -> Result<Vec<String>> {
        Ok(turn_repo::turn_messages(self.0, id)?)
    }
    pub fn requests(&self, id: &str) -> Result<Value> {
        messages::get_request_snapshots(self.0, id)
    }
    pub fn tools(&self, id: &str) -> Result<Vec<messages::ToolCallRow>> {
        messages::get_tool_calls_for_message(self.0, id)
    }
    pub fn origins(&self, id: &str) -> Result<Vec<(String, String, i64)>> {
        messages::get_tool_origins_for_message(self.0, id)
    }
    pub fn delivered_inputs(&self, id: &str) -> Result<Vec<input_queue::DeliveredInput>> {
        input_queue::load_delivered(self.0, id)
    }
    pub fn mcp(&self, id: &str, display: &str) -> Result<Option<(Value, Value, bool)>> {
        let row: Option<(String, String, bool)> = self
            .0
            .query_row("SELECT content_json,metadata_json,is_error FROM mcp_tool_outputs WHERE tool_use_id=?1 AND display_text=?2", [id, display], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        row.map(|(c, m, e)| Ok((serde_json::from_str(&c)?, serde_json::from_str(&m)?, e)))
            .transpose()
    }
    pub fn approval(&self, id: &str) -> Result<Option<permission_repo::PermissionApprovalRow>> {
        permission_repo::get_approval_by_id(self.0, id)
    }
    pub fn diffs(&self, message: &str) -> Result<Vec<diff_repo::CodeDiffRow>> {
        diff_repo::get_diffs_by_message(self.0, message)
    }
    pub fn user_content(&self, message: &str) -> Result<Value> {
        let m = messages::get_message(self.0, message)?.context("Message unavailable")?;
        messages::user_content_for_display(self.0, &m)
    }
}

impl SessionWrite<'_> {
    pub(super) fn next_order(&self) -> Result<i64> {
        Ok(self.0.query_row(
            "SELECT coalesce(max(sort_order),-1)+1 FROM messages",
            [],
            |r| r.get(0),
        )?)
    }
    pub fn replace_messages(&self, messages: &[Value]) -> Result<()> {
        crate::db::transcript::replace_messages(self.0, self.1, messages)?;
        self.2.set(true);
        Ok(())
    }
    pub fn append(&self, role: &str, content: &str) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        messages::insert_message(
            self.0,
            &id,
            self.1,
            role,
            content,
            None,
            &chrono::Utc::now().to_rfc3339(),
            false,
            self.next_order()?,
        )?;
        self.2.set(true);
        Ok(id)
    }
    pub fn append_user(&self, id: &str, content: &Value) -> Result<()> {
        messages::save_user_message(
            self.0,
            id,
            self.1,
            content,
            &chrono::Utc::now().to_rfc3339(),
            self.next_order()?,
        )?;
        permission_authority::record_conn(
            self.0,
            self.1,
            permission_authority::Source::User,
            content,
        )?;
        self.2.set(true);
        Ok(())
    }
    pub fn start_turn(&self) -> Result<String> {
        ensure!(!self.3, "Provider switch pending; worker is retiring");
        let id = uuid::Uuid::new_v4().to_string();
        self.0
            .execute("INSERT INTO execution_turns(id,conversation_id,started_at,status) VALUES(?1,?2,?3,'running')", params![id, self.1, chrono::Utc::now().to_rfc3339()])?;
        self.2.set(true);
        Ok(id)
    }
    pub fn segment(&self, turn: &str) -> Result<String> {
        let id = self.append("assistant", "")?;
        turn_repo::link_message(self.0, turn, &id)?;
        Ok(id)
    }
    pub fn finish_turn(
        &self,
        turn: &str,
        status: turn_repo::TurnStatus,
        elapsed_ms: u64,
    ) -> Result<()> {
        ensure!(
            status != turn_repo::TurnStatus::Running,
            "Expected terminal status"
        );
        ensure!(
            self.0.execute(
                "UPDATE execution_turns SET status=?1,ended_at=?2,elapsed_ms=?3 WHERE id=?4 AND status='running'",
                params![
                    turn_repo::status_name(status),
                    chrono::Utc::now().to_rfc3339(),
                    elapsed_ms,
                    turn
                ]
            )? == 1,
            "Turn is no longer running"
        );
        self.2.set(true);
        Ok(())
    }
    pub fn settle_interrupted(&self) -> Result<Vec<turn_repo::TurnRecord>> {
        self.2.set(true);
        let mut settled = Vec::new();
        while let Some(turn) = turn_repo::settle_next(self.0, self.1)? {
            settled.push(turn);
        }
        Ok(settled)
    }
    pub fn message_content(&self, id: &str, content: &str) -> Result<()> {
        messages::update_message_content(self.0, id, content)
    }
    pub fn snapshots(&self, message: &str, requests: &Value) -> Result<()> {
        let previous = messages::get_request_snapshots(self.0, message)?;
        if previous != *requests {
            if let Some(old) = previous.as_array() {
                let retained: std::collections::HashSet<&str> = requests
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r["id"].as_str())
                    .collect();
                for id in old.iter().filter_map(|r| r["id"].as_str()) {
                    if !retained.contains(id) {
                        self.0.execute(
                            "DELETE FROM model_requests WHERE message_id=?1 AND id=?2",
                            [message, id],
                        )?;
                    }
                }
                for entry in old.iter().filter(|r| r["id"].as_str().is_none()) {
                    crate::db::request_repo::remove_anonymous_snapshot(self.0, message, entry)?;
                }
            }
            messages::save_request_snapshots(self.0, message, requests)?;
        }
        Ok(())
    }
    pub fn normalized_request(&self, message: &str, request: &Value) -> Result<()> {
        crate::db::request_repo::save_request(self.0, message, request)
    }
    pub fn stream_tools(
        &self,
        message: &str,
        content: &str,
        tools: &[messages::ToolCallRow],
        origins: &crate::services::tool_history::ToolOrigins,
    ) -> Result<()> {
        if !tools.is_empty() {
            messages::upsert_stream_message(self.0, self.1, message, content)?;
        }
        for tool in tools {
            let row = messages::ToolCallRow {
                id: stream_occurrence_id(message, &tool.id),
                message_id: message.into(),
                ..tool.clone()
            };
            self.tool(&row, None)?;
            if let Some((parent, sequence)) = origins.get(&tool.id) {
                messages::save_tool_origin(
                    self.0,
                    &row.id,
                    &stream_occurrence_id(message, parent),
                    i64::try_from(*sequence)?,
                )?;
            }
        }
        Ok(())
    }
    pub fn tool(&self, row: &messages::ToolCallRow, origin: Option<(&str, i64)>) -> Result<()> {
        messages::upsert_tool_call(
            self.0,
            &row.id,
            &row.message_id,
            &row.name,
            row.input.as_deref(),
            row.output.as_deref(),
            row.is_error,
            row.sort_order,
        )?;
        if let Some((parent, seq)) = origin {
            messages::save_tool_origin(self.0, &row.id, parent, seq)?;
        }
        Ok(())
    }
    pub fn tool_note(&self, id: &str, note: &str) -> Result<()> {
        messages::set_tool_call_note(self.0, id, note)
    }
    pub fn tool_duration(&self, id: &str, millis: u64) -> Result<()> {
        messages::set_tool_duration(self.0, id, millis)
    }
    pub fn attachment(
        &self,
        id: &str,
        message: &str,
        name: &str,
        kind: &str,
        mime: &str,
        size: i64,
        source: &str,
    ) -> Result<()> {
        self.0.execute(
            "INSERT INTO attachments VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![id, message, name, kind, mime, size, source],
        )?;
        Ok(())
    }
    pub fn approval(&self, row: &permission_repo::PermissionApprovalRow) -> Result<()> {
        permission_repo::insert_permission_approval(
            self.0,
            &row.id,
            self.1,
            &row.message_id,
            &row.tool_name,
            &row.action,
            &row.risk_level,
            &row.status,
            &row.created_at,
        )
    }
    pub fn decide(
        &self,
        id: &str,
        status: &str,
        decision: Option<&str>,
        reason: Option<&str>,
        at: &str,
    ) -> Result<()> {
        permission_repo::update_approval_status(self.0, id, status, decision, reason, at)
    }
    pub fn diff(&self, row: &diff_repo::CodeDiffRow) -> Result<()> {
        diff_repo::insert_code_diff(
            self.0,
            &row.id,
            self.1,
            &row.message_id,
            &row.file_path,
            row.original_content.as_deref(),
            row.modified_content.as_deref(),
            row.diff_text.as_deref(),
            &row.status,
            &row.created_at,
        )
    }
    pub fn diff_status(&self, id: &str, status: &str, at: Option<&str>) -> Result<()> {
        diff_repo::update_diff_status(self.0, id, status, at)
    }
}

/// Durable execution addresses carry the saved incarnation across worker restarts.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryAddress {
    pub store: String,
    pub session: String,
    pub incarnation: String,
}
impl SessionDb {
    pub fn address(&self) -> HistoryAddress {
        HistoryAddress {
            store: self.stores.global().uuid.clone(),
            session: self.id.clone(),
            incarnation: self.incarnation.clone(),
        }
    }
}
impl super::Stores {
    pub fn resume_history(&self, address: &HistoryAddress) -> Result<SessionDb> {
        ensure!(
            self.global().uuid == address.store,
            "Execution store changed"
        );
        let session = self.open_session(&address.session)?;
        ensure!(
            session.incarnation == address.incarnation,
            "Execution incarnation changed"
        );
        Ok(session)
    }
}
#[derive(Debug, PartialEq)]
pub struct Attachment {
    pub id: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub mime: Option<String>,
    pub size: Option<i64>,
    pub source: Option<String>,
}
impl SessionRead<'_> {
    pub fn attachments(&self, message: &str) -> Result<Vec<Attachment>> {
        let mut q = self.0.prepare("SELECT id,file_name,file_type,mime_type,file_size,source FROM attachments WHERE message_id=?1 ORDER BY rowid")?;
        let rows = q
            .query_map([message], |r| {
                Ok(Attachment {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    mime: r.get(3)?,
                    size: r.get(4)?,
                    source: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    pub fn tool_note(&self, id: &str) -> Result<Option<String>> {
        messages::tool_call_note(self.0, id)
    }
    pub fn tool_duration(&self, id: &str) -> Result<Option<u64>> {
        messages::tool_duration(self.0, id)
    }
    pub fn child_invocations(
        &self,
        message: &str,
    ) -> Result<Vec<(String, crate::services::subagents::Invocation)>> {
        let mut q = self.0.prepare("SELECT child_incarnation,invocation FROM child_invocations WHERE parent_message=?1 ORDER BY rowid")?;
        let mut cursor = q.query([message])?;
        let mut out = Vec::new();
        while let Some(r) = cursor.next()? {
            out.push((r.get(0)?, serde_json::from_str(&r.get::<_, String>(1)?)?));
        }
        Ok(out)
    }
    pub fn receipt_count(&self) -> Result<i64> {
        Ok(self
            .0
            .query_row("SELECT count(*) FROM delivery_receipts", [], |r| r.get(0))?)
    }
    pub fn authority(&self) -> Result<Vec<Value>> {
        let mut q = self
            .0
            .prepare("SELECT content_json FROM session_permission_authority ORDER BY id")?;
        let mut rows = q.query([])?;
        let mut v = Vec::new();
        while let Some(r) = rows.next()? {
            v.push(serde_json::from_str(&r.get::<_, String>(0)?)?);
        }
        Ok(v)
    }
    pub fn binding(&self) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row("SELECT definition FROM session_agents", [], |r| r.get(0))
            .optional()?)
    }
}
impl SessionWrite<'_> {
    pub fn child_invocation(
        &self,
        address: &HistoryAddress,
        row: &crate::services::subagents::Invocation,
    ) -> Result<()> {
        let store: String = self
            .0
            .query_row("SELECT store_uuid FROM manifest", [], |r| r.get(0))?;
        ensure!(
            address.store == store
                && address.session != self.1
                && row.parent_session_id == self.1
                && row.child_session_id == address.session
                && !row.attempt_id.is_empty()
                && !row.tool_use_id.is_empty(),
            "Invalid child invocation ownership"
        );
        let updated = self.0.execute(
            "INSERT INTO child_invocations VALUES(?1,?2,?3,?4) ON CONFLICT(attempt_key) DO UPDATE SET invocation=excluded.invocation WHERE parent_message=excluded.parent_message AND child_incarnation=excluded.child_incarnation",
            params![
                row.key(),
                row.parent_message_id,
                address.incarnation,
                serde_json::to_string(row)?
            ],
        )?;
        ensure!(updated == 1, "Child invocation identity changed");
        Ok(())
    }
    pub fn bind_agent(&self, definition: &str) -> Result<()> {
        self.0.execute("INSERT INTO session_agents VALUES(?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET definition=excluded.definition", params![self.1, definition])?;
        Ok(())
    }
    pub fn approved_plan(&self, content: &Value) -> Result<()> {
        permission_authority::record_conn(
            self.0,
            self.1,
            permission_authority::Source::ApprovedPlan,
            content,
        )
    }
    pub fn rewind_from(&self, message: &str) -> Result<()> {
        let order: i64 = self.0.query_row(
            "SELECT sort_order FROM messages WHERE id=?1",
            [message],
            |r| r.get(0),
        )?;
        self.0.execute("DELETE FROM execution_turns WHERE id IN (SELECT t.turn_id FROM message_turns t JOIN messages m ON m.id=t.message_id WHERE m.sort_order>=?1) AND id NOT IN (SELECT t.turn_id FROM message_turns t JOIN messages m ON m.id=t.message_id WHERE m.sort_order<?1)", [order])?;
        self.0
            .execute("DELETE FROM messages WHERE sort_order>=?1", [order])?;
        self.2.set(true);
        Ok(())
    }
}

impl SessionRead<'_> {
    pub fn compaction_count(&self) -> Result<i64> {
        Ok(self.0.query_row(
            "SELECT local_count FROM compaction_state WHERE singleton=1",
            [],
            |r| r.get(0),
        )?)
    }
    pub fn model_history(
        &self,
        format: &crate::native_engine::provider_manager::ApiFormat,
    ) -> Result<(Vec<Value>, Option<String>)> {
        let id: String = self
            .0
            .query_row("SELECT id FROM manifest", [], |r| r.get(0))?;
        crate::runtime::replay_history(self.0, &id, format, |message| {
            let raw: Option<String> = self
                .0
                .query_row(
                    "SELECT value FROM native_boundaries WHERE message_id=?1",
                    [message],
                    |r| r.get(0),
                )
                .optional()?;
            raw.map(|v| serde_json::from_str(&v).map_err(Into::into))
                .transpose()
        })
    }
}
impl SessionWrite<'_> {
    pub fn native_boundary(&self, message: &str, value: &Value) -> Result<()> {
        self.0.execute("UPDATE compaction_state SET local_count=local_count+1 WHERE singleton=1 AND NOT EXISTS(SELECT 1 FROM native_boundaries WHERE message_id=?1)", [message])?;
        self.0
            .execute("INSERT INTO native_boundaries VALUES(?1,?2) ON CONFLICT(message_id) DO UPDATE SET value=excluded.value", params![message, serde_json::to_string(value)?])?;
        self.0.execute(
            "UPDATE messages SET is_compact_boundary=1 WHERE id=?1",
            [message],
        )?;
        self.2.set(true);
        Ok(())
    }
}
