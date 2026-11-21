//! The directory serializes binding selection and worker retirement. A revision
//! carries a committed picker effort to the owner's preferences even after a crash.
use super::{state, SessionDb};
use crate::native_engine::session_binding::{store, BindingSnapshot};
use anyhow::{Context, Result};
use rusqlite::{OpenFlags, OptionalExtension};

pub(super) struct BindingControl {
    pub pending: bool,
    effort: Option<(i64, Option<String>)>,
}

impl SessionDb {
    pub fn observe_provider(&self) -> Result<store::Observed> {
        let _guard = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        self.stores.global().read(|c| store::read(c, self.id()))
    }

    pub fn publish_provider(
        &self,
        expected: u64,
        selection: &BindingSnapshot,
        retiring: Option<(&str, Option<u32>)>,
        requester: &str,
        picker_effort: Option<Option<&str>>,
    ) -> Result<store::Write> {
        let _guard = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        let result = self
            .stores
            .global()
            .connection(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
                c.pragma_update(None, "synchronous", "NORMAL")?;
                crate::db::write_tx(
                    c,
                    store::PUBLISH_LABEL,
                    crate::db::STARTUP_WRITE_DEADLINE,
                    |tx| {
                        store::publish_choice_in(
                            tx,
                            self.id(),
                            expected,
                            selection,
                            retiring,
                            requester,
                            picker_effort,
                            store::BindingStore::Directory,
                        )
                    },
                )
            })?;
        Ok(result)
    }

    pub fn activate_provider(&self, worker: &str, generation: u64) -> Result<()> {
        let _guard = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        self.stores
            .global()
            .connection(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
                c.pragma_update(None, "synchronous", "NORMAL")?;
                store::activate_on(
                    c,
                    self.id(),
                    worker,
                    generation,
                    store::BindingStore::Directory,
                )
            })
    }

    pub(super) fn binding_control(&self) -> Result<BindingControl> {
        let control = self.stores.global().read(|c| Ok(c.query_row(
            "SELECT b.pending IS NOT NULL,e.revision,e.effort FROM conversations d
             LEFT JOIN cli_binding_generations b ON b.session_id=d.id LEFT JOIN cli_binding_efforts e ON e.session_id=d.id
             WHERE d.id=?1 AND d.incarnation=?2 AND d.storage_key=?3",
            [&self.id,&self.incarnation,&self.key], |r| Ok(BindingControl {
                pending: r.get(0)?, effort:r.get::<_,Option<i64>>(1)?.map(|rev|r.get(2).map(|value|(rev,value))).transpose()?,
            })).optional()?.context("Session incarnation is no longer visible")?))?;
        if let Some((revision, effort)) = &control.effort {
            let applied = |c: &rusqlite::Connection| -> Result<i64> {
                Ok(c.query_row("SELECT coalesce((SELECT CAST(value AS INTEGER) FROM peripheral_settings WHERE key='binding.effort-revision'),0)", [], |r|r.get(0))?)
            };
            if self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |c| applied(c))?
                < *revision
            {
                self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
                    crate::db::write_tx(c,"apply model picker effort",crate::db::STARTUP_WRITE_DEADLINE,|tx| {
                        if applied(tx)? < *revision {
                            tx.execute("INSERT INTO peripheral_settings(key,value) VALUES('cli.session',json_object('effort',?1))
                                ON CONFLICT(key) DO UPDATE SET value=json_set(value,'$.effort',?1)", [effort])?;
                            state::set_local(tx,"binding.effort-revision",&revision.to_string())?;
                            super::history::advance(tx)?;
                        }
                        Ok(())
                    })
                })?;
            }
        }
        Ok(control)
    }
}
