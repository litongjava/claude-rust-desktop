//! Updates for an already admitted assistant segment, including worker snapshots.
use super::ExecutorHistory;
use crate::db::message_repo::{self as messages, ToolCallRow};
use crate::services::{
    input_queue::{self, DeliveredInput},
    tool_history::ToolOrigins,
};
use anyhow::{ensure, Result};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub struct SegmentUpdate<'a> {
    pub text: Option<&'a str>,
    pub thinking: Option<&'a str>,
    pub tools: &'a [ToolCallRow],
    pub origins: Option<&'a ToolOrigins>,
    pub durations: Option<&'a HashMap<String, u64>>,
    pub notes: Option<&'a HashMap<String, Vec<String>>>,
    pub inputs: Option<&'a HashMap<String, Vec<DeliveredInput>>>,
    pub snapshots: Option<&'a Value>,
}

impl SegmentUpdate<'_> {
    fn persist(&self, c: &Connection, session: &str, message: &str, legacy: bool) -> Result<()> {
        ensure!(
            c.prepare_cached(
                "SELECT 1 FROM messages WHERE id=?1 AND conversation_id=?2 AND role='assistant'"
            )?
            .exists([message, session])?,
            "Assistant segment no longer belongs to this session"
        );
        if let Some(text) = self.text {
            messages::update_message_content(c, message, text)?;
        }
        if let Some(thinking) = self.thinking {
            c.execute(
                "UPDATE messages SET thinking=?1 WHERE id=?2 AND thinking IS NOT ?1",
                [thinking, message],
            )?;
        }
        let insert = if legacy {
            messages::insert_tool_call
        } else {
            messages::upsert_tool_call
        };
        for tool in self.tools {
            let occurrence = format!("{message}:{}", tool.id);
            insert(
                c,
                &occurrence,
                message,
                &tool.name,
                tool.input.as_deref(),
                tool.output.as_deref(),
                tool.is_error,
                tool.sort_order,
            )?;
            if let Some((parent, seq)) = self.origins.and_then(|map| map.get(&tool.id)) {
                messages::save_tool_origin(
                    c,
                    &occurrence,
                    &format!("{message}:{parent}"),
                    i64::try_from(*seq)?,
                )?;
            }
            if let Some(ms) = self.durations.and_then(|map| map.get(&tool.id)) {
                messages::set_tool_duration(c, &occurrence, *ms)?;
            }
            if let Some(notes) = self.notes.and_then(|map| map.get(&tool.id)) {
                messages::set_tool_call_note(c, &occurrence, &notes.join("\n\n"))?;
            }
            if let Some(inputs) = self.inputs.and_then(|map| map.get(&tool.id)) {
                input_queue::save_delivered(c, &occurrence, inputs)?;
            }
        }
        Ok(())
    }
}

impl ExecutorHistory {
    pub fn save_segment(&self, message: &str, update: SegmentUpdate<'_>) -> Result<()> {
        self.save_segment_with_deadline(
            message,
            update,
            "persist turn results",
            super::WRITE_DEADLINE,
        )
    }

    /// A missed live snapshot is retried from the authoritative event buffer at turn end.
    pub fn save_live_segment(&self, message: &str, update: SegmentUpdate<'_>) -> Result<()> {
        self.save_segment_with_deadline(
            message,
            update,
            "persist live transcript",
            std::time::Duration::from_millis(250),
        )
    }

    fn save_segment_with_deadline(
        &self,
        message: &str,
        update: SegmentUpdate<'_>,
        label: &str,
        deadline: std::time::Duration,
    ) -> Result<()> {
        match self {
            Self::Session(s) => {
                let _locks = s.stores.key_lock(&s.key, false, false)?;
                s.write_locked_with_deadline(label, deadline, |w| {
                    update.persist(w.0, s.id(), message, false)?;
                    if let Some(requests) = update.snapshots {
                        w.snapshots(message, requests)?;
                    }
                    Ok(())
                })
            }
            Self::Shared(db, id) => db.with_conn(|c| {
                crate::db::write_tx(c, label, deadline, |tx| {
                    update.persist(tx, id, message, true)?;
                    if let Some(requests) = update.snapshots {
                        messages::save_request_snapshots(tx, message, requests)?;
                    }
                    Ok(())
                })
            })?,
        }
    }

    /// A boundary can archive a segment before its queued-input sidecars are flushed.
    pub fn save_segment_inputs(
        &self,
        message: &str,
        tools: &[ToolCallRow],
        inputs: &HashMap<String, Vec<DeliveredInput>>,
    ) -> Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        let save = |c: &Connection| {
            let mut found=c.prepare_cached("SELECT 1 FROM tool_calls t JOIN messages m ON m.id=t.message_id WHERE t.id=?1 AND m.id=?2 AND m.conversation_id=?3")?;
            for tool in tools {
                if let Some(inputs) = inputs.get(&tool.id) {
                    let occurrence = format!("{message}:{}", tool.id);
                    ensure!(
                        found.exists(params![occurrence, message, self.id()])?,
                        "Archived tool occurrence no longer belongs to this session"
                    );
                    input_queue::save_delivered(c, &occurrence, inputs)?;
                }
            }
            Ok(())
        };
        match self {
            Self::Session(s) => s.write_named("archive queued inputs", |w| save(w.0)),
            Self::Shared(db, _) => db.with_conn(|c| {
                crate::db::write_tx(c, "archive queued inputs", super::WRITE_DEADLINE, |tx| {
                    save(tx)
                })
            })?,
        }
    }
}
