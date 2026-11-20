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
}

pub(super) fn projection(c: &Connection) -> Result<Projection> {
    Ok(c.query_row("SELECT content_revision,message_count,updated_at,last_turn_status,
        (SELECT substr(content,1,80) FROM messages WHERE role='user' AND content<>'' ORDER BY sort_order,rowid LIMIT 1),
        (SELECT substr(content,1,80) FROM messages WHERE role='user' AND content<>'' ORDER BY sort_order DESC,rowid DESC LIMIT 1),
        (SELECT coalesce(sum(length(CAST(content AS BLOB))),0) FROM messages) FROM manifest WHERE singleton=1",[],
        |r|Ok(Projection{revision:r.get(0)?,count:r.get(1)?,activity:r.get(2)?,status:r.get(3)?,first_prompt:r.get(4)?,latest_prompt:r.get(5)?,transcript_bytes:r.get(6)?}))?)
}

pub(super) fn advance(c: &Connection) -> Result<()> {
    c.execute("UPDATE manifest SET updated_at=?1,content_revision=content_revision+1,
        message_count=(SELECT count(*) FROM messages),
        last_turn_status=(SELECT status FROM execution_turns ORDER BY started_at DESC,rowid DESC LIMIT 1)
        WHERE singleton=1",[chrono::Utc::now().to_rfc3339()])?;
    Ok(())
}

impl GlobalDb {
    // Projection is secondary. Never wait for the G writer after a local commit.
    pub(super) fn project(&self, id: &str, incarnation: &str, p: &Projection) -> Result<()> {
        self.connection(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
            c.busy_timeout(std::time::Duration::ZERO)?;
            let result = c.execute(
                "UPDATE conversations SET content_revision=?1,message_count=?2,updated_at=?3,
                last_turn_status=?4,first_prompt=?5,latest_prompt=?6,transcript_bytes=?7
                WHERE id=?8 AND incarnation=?9 AND content_revision<?1",
                params![
                    p.revision,
                    p.count,
                    p.activity,
                    p.status,
                    p.first_prompt,
                    p.latest_prompt,
                    p.transcript_bytes,
                    id,
                    incarnation
                ],
            );
            c.busy_timeout(std::time::Duration::from_secs(5))?;
            result?;
            Ok(())
        })
    }
}

impl SessionDb {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn save_mcp(&self, id: &str, transport: &Value, failed: bool) -> Result<String> {
        let output = McpOutput::from_transport(transport)?;
        let display = output.display();
        self.write(|tx| {
            output.save(tx.0, &self.id, id, &display, failed)?;
            Ok(())
        })?;
        Ok(display)
    }
}

impl SessionRead<'_> {
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
        let row:Option<(String,String,bool)>=self.0.query_row("SELECT content_json,metadata_json,is_error FROM mcp_tool_outputs WHERE tool_use_id=?1 AND display_text=?2",[id,display],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
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
        let id = uuid::Uuid::new_v4().to_string();
        self.0.execute("INSERT INTO execution_turns(id,conversation_id,started_at,status) VALUES(?1,?2,?3,'running')",params![id,self.1,chrono::Utc::now().to_rfc3339()])?;
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
        ensure!(self.0.execute("UPDATE execution_turns SET status=?1,ended_at=?2,elapsed_ms=?3 WHERE id=?4 AND status='running'",params![turn_repo::status_name(status),chrono::Utc::now().to_rfc3339(),elapsed_ms,turn])?==1,"Turn is no longer running");
        self.2.set(true);
        Ok(())
    }
    pub fn settle_interrupted(&self) -> Result<Vec<turn_repo::TurnRecord>> {
        self.2.set(true);
        turn_repo::settle_interrupted(self.0, self.1)
    }
    pub fn message_content(&self, id: &str, content: &str) -> Result<()> {
        messages::update_message_content(self.0, id, content)
    }
    pub fn snapshots(&self, message: &str, requests: &Value) -> Result<()> {
        if messages::get_request_snapshots(self.0, message)? != *requests {
            messages::save_request_snapshots(self.0, message, requests)?;
        }
        Ok(())
    }
    pub fn tool(&self, row: &messages::ToolCallRow, origin: Option<(&str, i64)>) -> Result<()> {
        messages::insert_tool_call(
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
