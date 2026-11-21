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
