use super::layout::{self, GLOBAL};
use anyhow::{ensure, Result};
use r2d2::{ManageConnection, Pool};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// One pool budget for all access modes. A slot closes its old descriptor
// before switching between an OS read-only connection and a writer.
struct Slots;
type Slot = Option<(OpenFlags, Connection)>;
impl ManageConnection for Slots {
    type Connection = Slot;
    type Error = rusqlite::Error;
    fn connect(&self) -> rusqlite::Result<Slot> {
        Ok(None)
    }
    fn is_valid(&self, _: &mut Slot) -> rusqlite::Result<()> {
        Ok(())
    }
    fn has_broken(&self, _: &mut Slot) -> bool {
        false
    }
}

pub struct GlobalDb {
    path: PathBuf,
    pub(super) uuid: String,
    pool: Pool<Slots>,
}

impl GlobalDb {
    pub(super) fn new(root: &Path, uuid: String) -> Result<Self> {
        Ok(Self {
            path: root.join("global.db"),
            uuid,
            pool: Pool::builder().max_size(4).min_idle(Some(0)).build(Slots)?,
        })
    }

    fn connection<T>(
        &self,
        flags: OpenFlags,
        body: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let mut slot = self.pool.get()?;
        ensure!(self.path.is_file(), "Global database is missing");
        if slot.as_ref().map(|(mode, _)| *mode) != Some(flags) {
            *slot = None;
            *slot = Some((flags, layout::open(&self.path, flags)?));
        }
        let conn = &slot.as_ref().unwrap().1;
        layout::verify(conn, GLOBAL)?;
        let store: String = conn.query_row(
            "SELECT store_uuid FROM store_identity WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        ensure!(store == self.uuid, "Global store identity changed");
        body(conn)
    }

    pub(super) fn read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.connection(OpenFlags::SQLITE_OPEN_READ_ONLY, body)
    }

    pub(super) fn write<T>(
        &self,
        label: &str,
        id: &str,
        publication: bool,
        mut body: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let start = Instant::now();
        self.connection(OpenFlags::SQLITE_OPEN_READ_WRITE, |conn| {
            conn.pragma_update(
                None,
                "synchronous",
                if publication { "FULL" } else { "NORMAL" },
            )?;
            crate::db::write_tx(conn, label, Duration::from_secs(30), |tx| body(tx))
        })
        .map_err(|e| crate::db::write_boundary_context(label, id, start, e))
    }

    pub(super) fn incarnation(&self, id: &str, key: &str) -> Result<Option<String>> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT incarnation FROM conversations WHERE id=?1 AND storage_key=?2",
                [id, key],
                |r| r.get(0),
            )
            .optional()?)
        })
    }
    pub fn store_uuid(&self) -> &str {
        &self.uuid
    }
}
