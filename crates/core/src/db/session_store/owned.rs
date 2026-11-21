//! Short-lived repository selection for services whose public methods take a session ID.
use super::{SessionDb, Stores};
use crate::db::DbManager;
use anyhow::{ensure, Result};
use rusqlite::Connection;
use std::sync::Arc;

pub trait SessionAccess {
    fn select_session<'a>(&'a self, id: &'a str) -> Result<SessionRepository<'a>>;
}
pub struct SessionRepository<'a>(Backend<'a>);
enum Backend<'a> {
    Admitted(SessionDb),
    Shared(&'a DbManager, &'a str),
}
impl SessionAccess for Stores {
    fn select_session<'a>(&'a self, id: &'a str) -> Result<SessionRepository<'a>> {
        Ok(SessionRepository(Backend::Admitted(self.open_session(id)?)))
    }
}
impl SessionAccess for SessionDb {
    fn select_session<'a>(&'a self, id: &'a str) -> Result<SessionRepository<'a>> {
        ensure!(
            self.id() == id,
            "Session handle does not own the requested history"
        );
        Ok(SessionRepository(Backend::Admitted(self.clone())))
    }
}
impl SessionAccess for DbManager {
    fn select_session<'a>(&'a self, id: &'a str) -> Result<SessionRepository<'a>> {
        Ok(SessionRepository(Backend::Shared(self, id)))
    }
}
impl<D: SessionAccess> SessionAccess for Arc<D> {
    fn select_session<'a>(&'a self, id: &'a str) -> Result<SessionRepository<'a>> {
        self.as_ref().select_session(id)
    }
}

impl SessionRepository<'_> {
    pub(super) fn read<T>(&self, body: impl FnOnce(&Connection, &str) -> Result<T>) -> Result<T> {
        match &self.0 {
            Backend::Admitted(s) => s.read(|r| body(r.0, s.id())),
            Backend::Shared(db, id) => db.with_conn(|c| body(c, id))?,
        }
    }
    pub(super) fn write<T>(
        &self,
        label: &str,
        mut body: impl FnMut(&Connection, &str) -> Result<T>,
    ) -> Result<T> {
        match &self.0 {
            Backend::Admitted(s) => s.write_named(label, |w| body(w.0, s.id())),
            Backend::Shared(db, id) => db.with_conn(|c| {
                crate::db::write_tx(c, label, super::WRITE_DEADLINE, |tx| body(tx, id))
            })?,
        }
    }
}

impl SessionRepository<'_> {
    pub(crate) fn saved_mode(&self) -> Result<Option<crate::permissions::PermissionMode>> {
        self.read(|c, id| {
            super::state::local(c, &format!("cli.permission-mode.{id}"))?
                .map(|s| crate::services::session_permission_mode::parse(&s))
                .transpose()
        })
    }
    pub(crate) fn saved_plan_origin(&self) -> Result<bool> {
        self.read(|c, id| {
            Ok(
                super::state::local(c, &format!("plan.return-bypass.{id}"))?.as_deref()
                    == Some("true"),
            )
        })
    }
    /// The in-memory policy changes under the manager lock only if SQLite commits.
    pub(crate) fn commit_mode(
        &self,
        manager: &crate::permissions::PermissionManager,
        mode: crate::permissions::PermissionMode,
    ) -> Result<()> {
        let label = "session permission mode";
        let deadline = crate::db::STARTUP_WRITE_DEADLINE;
        let commit =
            |tx: rusqlite::Transaction<'_>| manager.commit_mode_update(mode, || Ok(tx.commit()?));
        match &self.0 {
            Backend::Admitted(s) => {
                let _guard = s.stores.key_lock(&s.key, false, false)?;
                s.write_locked_with_commit(
                    label,
                    deadline,
                    |w| w.set_agent_mode_from(mode, manager.get_mode(), manager.bypass_available()),
                    commit,
                )
            }
            Backend::Shared(db, id) => db.with_conn(|c| {
                crate::db::write_tx_with_commit(c, label, deadline, |tx| {
                    super::state::save_agent_mode(
                        &tx,
                        id,
                        mode,
                        manager.get_mode(),
                        manager.bypass_available(),
                    )?;
                    commit(tx)
                })
            })?,
        }
    }
}
