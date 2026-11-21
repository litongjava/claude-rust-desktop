//! Learning observations belong to the machine, including records from retired sessions.
use super::{shared::DirectoryRepository, Stores, WRITE_DEADLINE};
use crate::{db::DbManager, learning::Experience};
use anyhow::{ensure, Context, Result};
use rusqlite::OptionalExtension;

pub trait LearningRecords: Send + Sync {
    fn record_experience(&self, key: &str, experience: &Experience) -> Result<()>;
    fn rate_experience(&self, key: &str, rating: f64) -> Result<()>;
    fn experiences(&self) -> Result<Vec<Experience>>;
}
impl LearningRecords for DbManager {
    fn record_experience(&self, key: &str, experience: &Experience) -> Result<()> {
        let json = serde_json::to_string(experience)?;
        self.with_conn(|c| -> Result<()> {
        let tx = crate::db::write_transaction(&c)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT value FROM peripheral_settings WHERE key=?1",
                [&key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(
                serde_json::from_str::<Experience>(&old)? == *experience,
                "Experience ID conflicts with existing data"
            );
        } else {
            tx.execute(
                "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)",
                [key, json.as_str()],
            )?;
        }
        tx.execute("DELETE FROM peripheral_settings WHERE key IN (SELECT key FROM peripheral_settings WHERE key LIKE 'learning.experience.%' ORDER BY rowid DESC LIMIT -1 OFFSET 1000)", [])?;
        tx.commit()?;
        Ok(())
    })??;
        Ok(())
    }

    fn rate_experience(&self, key: &str, rating: f64) -> Result<()> {
        self.with_conn(|c| -> Result<()> {
            let tx = crate::db::write_transaction(&c)?;
            let json: String = tx.query_row(
                "SELECT value FROM peripheral_settings WHERE key=?1",
                [&key],
                |r| r.get(0),
            )?;
            let mut experience: Experience = serde_json::from_str(&json)?;
            experience.feedback = Some(rating);
            tx.execute(
                "UPDATE peripheral_settings SET value=?2 WHERE key=?1",
                rusqlite::params![key, serde_json::to_string(&experience)?],
            )?;
            tx.commit()?;
            Ok(())
        })??;
        Ok(())
    }

    fn experiences(&self) -> Result<Vec<Experience>> {
        self.with_conn(|c| -> Result<Vec<Experience>> {
        let mut statement = c.prepare("SELECT value FROM peripheral_settings WHERE key LIKE 'learning.experience.%' ORDER BY rowid")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    })?
    }
}

impl LearningRecords for Stores {
    fn record_experience(&self, key: &str, experience: &Experience) -> Result<()> {
        let json = serde_json::to_string(experience)?;
        let existing = self.directory_write("record learning experience", key, WRITE_DEADLINE, |tx| {
            let inserted=tx.execute("INSERT OR IGNORE INTO global_settings(domain,name,value) VALUES('learning',?1,?2)", [key,json.as_str()])?;
            if inserted==0 {
                return Ok(Some(tx.query_row("SELECT value FROM global_settings WHERE domain='learning' AND name=?1",[key],|r| r.get::<_,String>(0))?));
            }
            tx.execute("DELETE FROM global_settings WHERE rowid IN (
                SELECT rowid FROM global_settings WHERE domain='learning' AND name GLOB 'learning.experience.*'
                ORDER BY rowid DESC LIMIT -1 OFFSET 1000)", [])?;
            Ok(None)
        })?;
        if let Some(existing) = existing {
            ensure!(
                serde_json::from_str::<Experience>(&existing)? == *experience,
                "Experience ID conflicts with existing data"
            );
        }
        Ok(())
    }
    fn rate_experience(&self, key: &str, rating: f64) -> Result<()> {
        self.directory_write("rate learning experience",key,WRITE_DEADLINE, |tx| {
            let changed=tx.execute("UPDATE global_settings SET value=json_set(value,'$.feedback',?2) WHERE domain='learning' AND name=?1",rusqlite::params![key,rating])?;
            ensure!(changed==1, "Experience not found");
            Ok(())
        })
    }
    fn experiences(&self) -> Result<Vec<Experience>> {
        let raw=self.directory_read(|c|{
            let mut q=c.prepare_cached("SELECT value FROM global_settings WHERE domain='learning' AND name GLOB 'learning.experience.*' ORDER BY rowid")?;
            let rows=q.query_map([],|r|r.get::<_,String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        raw.into_iter()
            .map(|r| serde_json::from_str(&r).context("Invalid learning experience"))
            .collect()
    }
}
