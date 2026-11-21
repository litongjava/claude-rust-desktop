//! Common implementation boundary for repositories whose rows belong entirely to G.
use super::Stores;
use anyhow::Result;
use rusqlite::{Connection, Transaction};
use std::time::Duration;

pub(super) trait DirectoryRepository {
    fn directory_read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T>;
    fn directory_write<T>(
        &self,
        label: &str,
        owner: &str,
        deadline: Duration,
        body: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T>;
}
impl DirectoryRepository for Stores {
    fn directory_read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.global().read(body)
    }
    fn directory_write<T>(
        &self,
        label: &str,
        owner: &str,
        deadline: Duration,
        mut body: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let start = std::time::Instant::now();
        self.global()
            .connection(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
                c.pragma_update(None, "synchronous", "NORMAL")?;
                crate::db::write_tx(c, label, deadline, |tx| body(tx))
            })
            .map_err(|e| crate::db::write_boundary_context(label, owner, start, e))
    }
}
impl DirectoryRepository for crate::db::DbManager {
    fn directory_read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with_conn(body)?
    }
    fn directory_write<T>(
        &self,
        label: &str,
        _owner: &str,
        deadline: Duration,
        mut body: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.with_conn(|c| crate::db::write_tx(c, label, deadline, |tx| body(tx)))?
    }
}
