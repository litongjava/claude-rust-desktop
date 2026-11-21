//! Analytics takes a consistent snapshot of each selected incarnation.
use super::{DirectoryFilter, SessionRead, Stores};
use anyhow::{ensure, Result};
use chrono::{Local, NaiveDate};
use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct CorpusUsage {
    pub days: Vec<DailyUsage>,
    pub failures: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct DailyUsage {
    pub session: String,
    pub day: NaiveDate,
    pub model: String,
    pub started_ms: i64,
    pub ended_ms: i64,
    pub requests: u64,
    pub tokens: [i128; 4],
    pub heat: i128,
}

impl SessionRead<'_> {
    pub fn cost(&self, config: &crate::config::AppConfig) -> Result<serde_json::Value> {
        let id: String =
            self.0
                .query_row("SELECT id FROM manifest WHERE singleton=1", [], |r| {
                    r.get(0)
                })?;
        crate::services::usage::cost_snapshot(self.0, &id, config)
    }
}

fn usage_days(
    conn: &rusqlite::Connection,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<DailyUsage>> {
    let mut days = BTreeMap::new();
    let mut q=conn.prepare(
            "SELECT started_at_ms,coalesce(ended_at_ms,started_at_ms),coalesce(model,''),
             coalesce(input_tokens,0),coalesce(output_tokens,0),coalesce(cache_read_tokens,0),coalesce(cache_write_tokens,0),conversation_id
             FROM model_requests WHERE started_at_ms IS NOT NULL")?;
    let mut rows = q.query([])?;
    while let Some(row) = rows.next()? {
        ensure!(!cancelled(), "Stats scan cancelled");
        let started_ms: i64 = row.get(0)?;
        let ended_ms: i64 = row.get(1)?;
        let Some(time) = chrono::DateTime::from_timestamp_millis(started_ms) else {
            continue;
        };
        let day = time.with_timezone(&Local).date_naive();
        let model: String = row.get(2)?;
        let session: String = row.get(7)?;
        let v = days
            .entry((day, model.clone(), session.clone()))
            .or_insert_with(|| DailyUsage {
                session,
                day,
                model,
                started_ms,
                ended_ms,
                requests: 0,
                tokens: [0; 4],
                heat: 0,
            });
        v.started_ms = v.started_ms.min(started_ms);
        v.ended_ms = v.ended_ms.max(ended_ms);
        v.requests += 1;
        let mut total = 0;
        for (i, sum) in v.tokens.iter_mut().enumerate() {
            let n = i128::from(row.get::<_, i64>(3 + i)?);
            *sum += n;
            total += n;
        }
        v.heat += total.max(1);
    }
    Ok(days.into_values().collect())
}

impl Stores {
    pub fn usage(&self, cancelled: impl Fn() -> bool) -> Result<CorpusUsage> {
        let mut result = CorpusUsage::default();
        for row in self.global().list_sessions(DirectoryFilter::default())? {
            ensure!(!cancelled(), "Stats scan cancelled");
            let id = row.conversation.id.as_str();
            let shard = self
                .open_listed(&row)
                .and_then(|s| s.read(|r| usage_days(r.0, &cancelled)));
            ensure!(!cancelled(), "Stats scan cancelled");
            match shard {
                Ok(mut days) => result.days.append(&mut days),
                Err(error) => result.failures.push((id.into(), format!("{error:#}"))),
            }
        }
        Ok(result)
    }
}

impl crate::db::DbManager {
    pub fn usage(&self, cancelled: impl Fn() -> bool) -> Result<CorpusUsage> {
        Ok(CorpusUsage {
            days: self.with_conn(|c| usage_days(c, &cancelled))??,
            failures: Vec::new(),
        })
    }
}

impl Stores {
    pub fn skill_usage(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        cancelled: impl Fn() -> bool,
    ) -> Result<BTreeMap<String, crate::services::skill_usage::Usage>> {
        let mut totals = BTreeMap::<String, crate::services::skill_usage::Usage>::new();
        for row in self.global().list_sessions(Default::default())? {
            ensure!(!cancelled(), "Usage scan cancelled");
            let session = self.open_listed(&row)?;
            for (name, current) in session
                .read(|r| crate::services::skill_usage::skill_snapshot(r.0, now, &cancelled))?
            {
                let total = totals.entry(name).or_default();
                total.uses = total.uses.saturating_add(current.uses);
                total.tokens_7d = total.tokens_7d.saturating_add(current.tokens_7d);
                total.last_used = total.last_used.max(current.last_used);
            }
        }
        Ok(totals)
    }

    pub fn mcp_last_used(
        &self,
        cancelled: impl Fn() -> bool,
    ) -> Result<BTreeMap<String, chrono::DateTime<chrono::Utc>>> {
        let mut latest = BTreeMap::new();
        for row in self.global().list_sessions(Default::default())? {
            ensure!(!cancelled(), "Usage scan cancelled");
            let session = self.open_listed(&row)?;
            for (name, at) in
                session.read(|r| crate::services::skill_usage::mcp_snapshot(r.0, &cancelled))?
            {
                let last = latest.entry(name).or_insert(at);
                *last = (*last).max(at);
            }
        }
        Ok(latest)
    }
}
impl crate::db::DbManager {
    pub fn skill_usage(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        cancelled: impl Fn() -> bool,
    ) -> Result<BTreeMap<String, crate::services::skill_usage::Usage>> {
        self.with_conn(|c| crate::services::skill_usage::skill_snapshot(c, now, &cancelled))?
    }
    pub fn mcp_last_used(
        &self,
        cancelled: impl Fn() -> bool,
    ) -> Result<BTreeMap<String, chrono::DateTime<chrono::Utc>>> {
        self.with_conn(|c| crate::services::skill_usage::mcp_snapshot(c, &cancelled))?
    }
}

use crate::db::message_repo;
use crate::services::context_usage::Boundary;
use serde_json::Value;
impl super::SessionRepository<'_> {
    pub(crate) fn context_boundary(&self) -> Result<Option<Boundary>> {
        self.read(|conn,session| {
        use rusqlite::OptionalExtension;
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT id, created_at FROM messages WHERE conversation_id=?1 AND is_compact_boundary=1 ORDER BY sort_order DESC, rowid DESC LIMIT 1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(id, created)| Boundary {
            id,
            created_at_ms: chrono::DateTime::parse_from_rfc3339(&created)
                .ok()
                .map(|at| at.timestamp_millis()),
        }))
    })
    }

    pub(crate) fn matching_snapshot(
        &self,
        accept: impl Fn(&Value) -> bool,
    ) -> Result<Option<Value>> {
        self.read(|conn,session| {
        let mut statement = conn.prepare(
            "SELECT t.requests_json FROM message_trajectory t JOIN messages m ON m.id=t.message_id WHERE m.conversation_id=?1 ORDER BY m.sort_order DESC, m.created_at DESC, m.rowid DESC",
        )?;
        let mut rows = statement.query([session])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let Ok(snapshots) = serde_json::from_str::<Value>(&raw) else {
                // A corrupt row may hide a newer request. Do not report stale data.
                return Ok(None);
            };
            let Some(items) = snapshots.as_array() else { return Ok(None); };
            if let Some(snapshot) = items.iter().rev().find(|s| (s["source"] == "native_engine" || s.get("source").is_none()) && accept(s)) {
                // Preserve unknown metadata on this request; never replace it
                // with an older request merely because the old one has counts.
                // Only the selected snapshot is expanded: restoring every row
                // would multiply the status query's work.
                let mut snapshot = snapshot.clone();
                message_repo::load_trajectory_blobs(conn, &mut snapshot)?;
                return Ok(Some(snapshot));
            }
            // Local workflows and failed pre-request turns can persist [].
            // A row containing only side-query snapshots is not a parent request.
        }
        Ok(None)
    })
    }
    /// Count durable usage and merge still-live UI snapshots from the same read view.
    pub fn usage_totals(&self, live: &[Value]) -> Result<(Value, u64, u64)> {
        self.read(|c,id| {
            let tx=if c.is_autocommit() {Some(c.unchecked_transaction()?)} else {None};
            let c=tx.as_deref().unwrap_or(c);
            let mut stmt=c.prepare_cached("SELECT input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,total_duration_ms,
                CASE WHEN json_valid(usage_json) THEN json_extract(usage_json,'$.input_tokens') IS NOT NULL AND json_extract(usage_json,'$.prompt_tokens') IS NULL ELSE 0 END
                FROM model_requests WHERE conversation_id=?1")?;
            let mut rows=stmt.query([id])?;
            let mut sums=[None::<i128>;5];
            let mut count=0u64;
            let mut input=0i128;
            while let Some(row)=rows.next()? {
                count+=1;
                let anthropic:bool=row.get(5)?;
                for (i,sum) in sums.iter_mut().enumerate() {
                    if let Some(value)=row.get::<_,Option<i64>>(i)? {
                        *sum=Some(sum.unwrap_or(0)+i128::from(value));
                        if i==0 || anthropic && (i==2 || i==3) { input+=i128::from(value); }
                    }
                }
            }
            let [uncached,output,cache_read,cache_write,ms]=sums.map(|v|v.map(|n|n.clamp(i64::MIN.into(),i64::MAX.into()) as i64));
            let usage=serde_json::json!({"session_id":id,"requests":count,"input_tokens":uncached,"output_tokens":output,"cache_read_tokens":cache_read,"cache_write_tokens":cache_write,"duration_ms":ms});
            let mut input=input.clamp(0,u64::MAX.into()) as u64;
            let mut output=output.unwrap_or(0).max(0) as u64;
            let mut saved=c.prepare_cached("SELECT 1 FROM model_requests WHERE id=?1 AND conversation_id=?2")?;
            for snapshot in live {
                if !saved.exists([snapshot["id"].as_str().unwrap_or_default(),id])? {
                    input=input.saturating_add(snapshot["inputTokens"].as_u64().unwrap_or_default());
                    output=output.saturating_add(snapshot["outputTokens"].as_u64().unwrap_or_default());
                }
            }
            Ok((usage,input,output))
        })
    }
}
