use super::layout::{self, GLOBAL};
use anyhow::{ensure, Result};
use r2d2::{ManageConnection, Pool};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction};
use std::path::{Path, PathBuf};
use std::sync::Weak;
use std::time::Instant;

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
    pub(super) root: Weak<super::Root>,
    #[cfg(unix)]
    identity: (u64, u64),
    path: PathBuf,
    pub(super) uuid: String,
    pool: Pool<Slots>,
}

impl GlobalDb {
    pub(super) fn new(root: &Path, uuid: String) -> Result<Self> {
        Ok(Self {
            root: Weak::new(),
            #[cfg(unix)]
            identity: file_identity(&root.join("global.db"))?,
            path: root.join("global.db"),
            uuid,
            pool: Pool::builder().max_size(4).min_idle(Some(0)).build(Slots)?,
        })
    }

    pub(super) fn connection<T>(
        &self,
        flags: OpenFlags,
        body: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let mut slot = self.pool.get()?;
        #[cfg(unix)]
        ensure!(
            file_identity(&self.path)? == self.identity,
            "Global database was replaced"
        );
        // Without a portable inode, discard the cached descriptor and check
        // the UUID on a fresh open of the current pathname.
        #[cfg(not(unix))]
        {
            *slot = None;
        }
        ensure!(
            std::fs::read_to_string(
                self.path
                    .with_file_name("session-locks")
                    .join(".initialized")
            )? == self.uuid,
            "Global store UUID changed"
        );
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
            crate::db::write_tx(conn, label, super::WRITE_DEADLINE, |tx| body(tx))
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
    pub fn conversation(
        &self,
        id: &str,
    ) -> Result<Option<crate::db::conversation_repo::ConversationRow>> {
        let key = layout::storage_key(id)?;
        let stores = super::Stores {
            root: self.root.upgrade().expect("global belongs to a live root"),
            fault: None,
        };
        let Some(_locks) = stores.resolved_key(&key)? else {
            return Ok(None);
        };
        self.read(|conn| {
            let snapshot = conn.unchecked_transaction()?;
            let mut row = crate::db::conversation_repo::conversation_metadata(&snapshot, id)?;
            if let Some(conversation) = row.as_mut().filter(|r| {
                r.title
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("CLI session"))
            }) {
                let prompt: Option<String> = snapshot.query_row(
                    "SELECT first_prompt FROM conversations WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )?;
                if let Some(title) = prompt
                    .as_deref()
                    .and_then(crate::db::conversation_repo::prompt_title)
                {
                    conversation.title = Some(title);
                }
            }
            Ok(row)
        })
    }

    #[doc(hidden)]
    pub fn with_read_for_test<T>(&self, body: impl FnOnce() -> T) -> Result<T> {
        self.read(|_| Ok(body()))
    }
    pub fn store_uuid(&self) -> &str {
        &self.uuid
    }
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(metadata.is_file(), "Global database must be a regular file");
    Ok((metadata.dev(), metadata.ino()))
}
