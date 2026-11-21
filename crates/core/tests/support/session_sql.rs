//! Direct fixture inspection and fault injection for a published test store.
#![allow(dead_code)]
use anyhow::{ensure, Result};
use deepseek_code_core::db::session_store::Stores;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

fn existing(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.pragma_update(None, "foreign_keys", true)?;
    Ok(db)
}

pub trait FixtureSql {
    fn directory_sql<T>(&self, body: impl FnOnce(&Connection) -> T) -> Result<T>;
    fn shard_path(&self, id: &str) -> Result<PathBuf>;
    fn shard_sql<T>(&self, id: &str, body: impl FnOnce(&Connection) -> T) -> Result<T>;
}

impl FixtureSql for Stores {
    fn directory_sql<T>(&self, body: impl FnOnce(&Connection) -> T) -> Result<T> {
        let db = existing(&self.data_dir().join("global.db"))?;
        Ok(body(&db))
    }
    fn shard_path(&self, id: &str) -> Result<PathBuf> {
        let incarnation = self.open_session(id)?.incarnation().to_owned();
        let key: String = self.directory_sql(|c| {
            c.query_row(
                "SELECT storage_key FROM conversations WHERE id=?1 AND incarnation=?2",
                [id, &incarnation],
                |r| r.get(0),
            )
        })??;
        ensure!(
            Path::new(&key).file_name() == Some(key.as_ref()),
            "Invalid fixture storage key"
        );
        Ok(self
            .data_dir()
            .join("sessions")
            .join(key)
            .join("session.db"))
    }
    fn shard_sql<T>(&self, id: &str, body: impl FnOnce(&Connection) -> T) -> Result<T> {
        let db = existing(&self.shard_path(id)?)?;
        ensure!(
            db.query_row("SELECT id FROM manifest", [], |r| r.get::<_, String>(0))? == id,
            "Wrong fixture shard"
        );
        Ok(body(&db))
    }
}
