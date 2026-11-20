//! Directory maintenance never retains a G checkout while opening a shard.
use super::*;
use std::fs::File;
use std::sync::mpsc;
const PAGE: usize = 8;
const INTERVAL: Duration = Duration::from_secs(2);

impl Stores {
    pub(super) fn start_projection_repair(&self) {
        self.root.repair.get_or_init(|| {
            self.try_repair_page();
            let root = Arc::downgrade(&self.root);
            let (stop, rx) = mpsc::channel();
            std::thread::spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(INTERVAL) {
                    let Some(root) = root.upgrade() else { break };
                    Stores { root, fault: None }.try_repair_page();
                }
            });
            stop
        });
    }

    fn try_repair_page(&self) {
        if let Err(error) = self.repair_page() {
            tracing::warn!(%error, "Directory projection pass postponed");
        }
    }

    fn repair_page(&self) -> Result<()> {
        let mut cursor = self.root.repair_cursor.lock().unwrap();
        let entries = self.global().read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, storage_key, incarnation FROM conversations
                WHERE storage_key>?1 ORDER BY storage_key LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![*cursor, PAGE], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        *cursor = if entries.len() == PAGE {
            entries.last().unwrap().1.clone()
        } else {
            String::new()
        };
        drop(cursor);
        for (id, key, incarnation) in entries {
            if storage_key(&id)? != key {
                continue;
            }
            let session = SessionDb {
                stores: self.clone(),
                id,
                key,
                incarnation,
            };
            if let Err(error) = session.repair_background() {
                tracing::debug!(%error,session=%session.id,"Directory projection pending");
            }
        }
        Ok(())
    }
}

impl SessionDb {
    fn repair_background(&self) -> Result<()> {
        let _namespace = self.stores.namespace()?;
        let file = File::options()
            .read(true)
            .write(true)
            .open(self.stores.root.path.join("session-locks").join(&self.key))?;
        if file.try_lock_shared().is_err() {
            return Ok(());
        }
        self.check_directory()?;
        let path = self.stores.sessions().join(&self.key).join("session.db");
        let old = {
            let _permit = Permits::take(1);
            layout::needs_upgrade(&path, SESSION)?
        };
        if old {
            file.unlock()?;
            if file.try_lock().is_err() {
                return Ok(());
            }
            self.check_directory()?;
            let _permit = Permits::take(1);
            if layout::needs_upgrade(&path, SESSION)? {
                let c = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                verify_manifest(
                    &c,
                    self.stores.global().store_uuid(),
                    &self.id,
                    &self.incarnation,
                )?;
                drop(c);
                layout::upgrade(&path, SESSION)?;
            }
        }
        self.project_current()
    }

    pub(super) fn try_project_current(&self) {
        if let Err(error) = self.project_current() {
            tracing::debug!(%error,session=%self.id,"Directory projection pending");
        }
    }

    // Caller holds the shared or exclusive lifecycle lock.
    fn project_current(&self) -> Result<()> {
        let global = self.stores.global();
        let published: i64 = global.read(|c| {
            Ok(c.query_row(
                "SELECT content_revision FROM conversations WHERE id=?1 AND incarnation=?2",
                [&self.id, &self.incarnation],
                |r| r.get(0),
            )?)
        })?;
        let snapshot = self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |c| {
            let tx = c.transaction()?;
            let revision: i64 =
                tx.query_row("SELECT content_revision FROM manifest", [], |r| r.get(0))?;
            if revision <= published {
                return Ok(None);
            }
            Ok(Some(history::projection(&tx)?))
        })?;
        if let Some(p) = snapshot {
            global.project(&self.id, &self.incarnation, &p)?;
        }
        Ok(())
    }
}
