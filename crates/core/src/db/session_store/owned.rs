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
