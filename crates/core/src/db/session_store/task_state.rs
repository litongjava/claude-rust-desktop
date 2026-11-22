//! Super Agent operation records outlive participants. Publish referenced bodies before G CAS.
use super::{shared::DirectoryRepository, Stores, WRITE_DEADLINE};
use crate::db::DbManager;
use anyhow::{ensure, Context, Result};
use rusqlite::OptionalExtension;
use serde::{de::DeserializeOwned, Serialize};

enum Change<'a> {
    Save,
    Insert,
    Replace(&'a str),
}
trait TaskBodies: DirectoryRepository {
    const SHARED: bool;
    fn body_ref<T: Serialize>(&self, v: &T) -> Result<String>;
    fn body(&self, reference: &str) -> Result<String>;
}
impl TaskBodies for Stores {
    const SHARED: bool = false;
    fn body_ref<T: Serialize>(&self, v: &T) -> Result<String> {
        self.put_payload(&serde_json::to_value(v)?)
    }
    fn body(&self, reference: &str) -> Result<String> {
        Ok(serde_json::to_string(&self.payload(reference)?)?)
    }
}
impl TaskBodies for DbManager {
    const SHARED: bool = true;
    fn body_ref<T: Serialize>(&self, v: &T) -> Result<String> {
        Ok(serde_json::to_string(v)?)
    }
    fn body(&self, raw: &str) -> Result<String> {
        Ok(raw.into())
    }
}
fn reference<D: TaskBodies>(db: &D, key: &str) -> Result<Option<String>> {
    let sql = if D::SHARED {
        "SELECT value FROM peripheral_settings WHERE key=?1"
    } else {
        "SELECT value FROM global_settings WHERE domain='super-agent' AND name=?1"
    };
    db.directory_read(|c| Ok(c.query_row(sql, [key], |r| r.get(0)).optional()?))
}
fn change<D: TaskBodies>(db: &D, key: &str, value: &str, action: Change<'_>) -> Result<bool> {
    db.directory_write("publish Super Agent operation",key,WRITE_DEADLINE,|tx|{
        let (table,columns,values,conflict)=if D::SHARED {("peripheral_settings","key,value","?1,?2","key")} else {("global_settings","domain,name,value","'super-agent',?1,?2","domain,name")};
        let count=match action {
            Change::Insert=>tx.execute(&format!("INSERT INTO {table}({columns}) VALUES({values}) ON CONFLICT({conflict}) DO NOTHING"),[key,value])?,
            Change::Save=>tx.execute(&format!("INSERT INTO {table}({columns}) VALUES({values}) ON CONFLICT({conflict}) DO UPDATE SET value=excluded.value"),[key,value])?,
            Change::Replace(old)=>{
                let sql=if D::SHARED {"UPDATE peripheral_settings SET value=?2 WHERE key=?1 AND value=?3"} else {"UPDATE global_settings SET value=?2 WHERE domain='super-agent' AND name=?1 AND value=?3"};
                tx.execute(sql,[key,value,old])?
            }
        };
        Ok(count==1)
    })
}

pub trait TaskState {
    fn task_state<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>>;
    fn save_task_state<T: Serialize>(&self, key: &str, value: &T) -> Result<()>;
    fn reserve_task_state<T: Serialize + DeserializeOwned>(
        &self,
        key: &str,
        initial: T,
    ) -> Result<T>;
    fn update_task_state<T: Serialize + DeserializeOwned>(
        &self,
        key: &str,
        apply: impl FnMut(&mut T),
    ) -> Result<T>;
    fn task_state_rows(&self, prefix: &str) -> Result<Vec<(String, String)>>;
}
impl<D: TaskBodies> TaskState for D {
    fn task_state<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        reference(self, key)?
            .map(|r| Ok(serde_json::from_str(&self.body(&r)?)?))
            .transpose()
    }
    fn save_task_state<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        change(self, key, &self.body_ref(value)?, Change::Save)?;
        Ok(())
    }
    fn reserve_task_state<T: Serialize + DeserializeOwned>(
        &self,
        key: &str,
        initial: T,
    ) -> Result<T> {
        if let Some(prior) = self.task_state(key)? {
            return Ok(prior);
        }
        if change(self, key, &self.body_ref(&initial)?, Change::Insert)? {
            return Ok(initial);
        }
        self.task_state(key)?
            .context("Reserved operation disappeared")
    }
    fn update_task_state<T: Serialize + DeserializeOwned>(
        &self,
        key: &str,
        mut apply: impl FnMut(&mut T),
    ) -> Result<T> {
        let start = std::time::Instant::now();
        loop {
            let old = reference(self, key)?.context("Launch operation disappeared")?;
            let mut value = serde_json::from_str(&self.body(&old)?)?;
            apply(&mut value);
            if change(self, key, &self.body_ref(&value)?, Change::Replace(&old))? {
                return Ok(value);
            }
            ensure!(
                start.elapsed() < WRITE_DEADLINE,
                "Launch operation changed during update"
            );
        }
    }
    fn task_state_rows(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let sql = if D::SHARED {
            "SELECT key,value FROM peripheral_settings WHERE key LIKE ?1 ORDER BY key"
        } else {
            "SELECT name,value FROM global_settings WHERE domain='super-agent' AND name LIKE ?1 ORDER BY name"
        };
        let rows = self.directory_read(|c| {
            let mut q = c.prepare_cached(sql)?;
            let rows = q.query_map([format!("{prefix}%")], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter()
            .map(|(key, r)| Ok((key, self.body(&r)?)))
            .collect()
    }
}
