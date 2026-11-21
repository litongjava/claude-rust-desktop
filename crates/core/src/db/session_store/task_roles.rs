//! Router role is a local setting. Membership is granted by G to one incarnation.
use super::{owned::Backend, settings, SessionRepository, Stores};
use crate::{
    db::DbManager,
    super_agent::{role::MemberRole, Role},
};
use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use std::sync::Arc;

impl SessionRepository<'_> {
    pub(crate) fn role(&self) -> Result<Role> {
        match &self.0 {
            Backend::Admitted(s) => s.read(|r| {
                Ok(r.setting(settings::ROLE).context("Unknown role in session settings")?.unwrap_or_default())
            }),
            Backend::Shared(_, _) => self.read(|c, id| {
                match super::state::local(c, &format!("super-agent.role.{id}"))?.as_deref() {
                    None | Some("") => Ok(Role::Agent),
                    Some(raw) => Role::parse(raw),
                }
            }),
        }
    }
    pub(crate) fn set_role(&self, role: Role) -> Result<()> {
        let deadline = crate::db::STARTUP_WRITE_DEADLINE;
        match &self.0 {
            Backend::Admitted(s) => {
                let _lock = s.stores.key_lock(&s.key, false, false)?;
                s.write_locked_with_deadline("session role", deadline, |w| {
                    w.set_setting(settings::ROLE, &role)
                })
            }
            Backend::Shared(db, id) => db.with_conn(|c| {
                crate::db::write_tx(c, "session role", deadline, |tx| {
                    super::state::set_local(tx, &format!("super-agent.role.{id}"), role.as_str())
                })
            })?,
        }
    }
}

pub trait TaskMembership {
    fn member_session(&self, id: &str) -> Result<Option<(String, MemberRole)>>;
}
impl<D: TaskMembership> TaskMembership for Arc<D> {
    fn member_session(&self, id: &str) -> Result<Option<(String, MemberRole)>> {
        self.as_ref().member_session(id)
    }
}
impl TaskMembership for Stores {
    fn member_session(&self, id: &str) -> Result<Option<(String, MemberRole)>> {
        self.global().read_directory_row(id,|c|{
            let raw=c.query_row("SELECT m.task_id,m.role FROM sa_member_sessions m
                JOIN conversations v ON (v.id,v.incarnation)=(m.session_id,m.incarnation)
                JOIN sa_tasks t ON t.task_id=m.task_id WHERE m.session_id=?1 AND m.role IN ('coder','reviewer')
                ORDER BY t.updated_at DESC,t.rowid DESC LIMIT 1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
            Ok(raw.map(|(task,role)|(task,if role=="coder"{MemberRole::Coder}else{MemberRole::Reviewer})))
        })
    }
}
impl TaskMembership for DbManager {
    fn member_session(&self, session: &str) -> Result<Option<(String, MemberRole)>> {
        self.with_conn(|conn| {
        let row: Option<(String, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT task_id, coder_session_id, reviewer_session_id FROM sa_tasks WHERE coder_session_id=?1 OR reviewer_session_id=?1 ORDER BY updated_at DESC LIMIT 1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(row.and_then(|(task_id, coder, reviewer)| {
            if coder.as_deref() == Some(session) {
                Some((task_id, MemberRole::Coder))
            } else if reviewer.as_deref() == Some(session) {
                Some((task_id, MemberRole::Reviewer))
            } else {
                None
            }
        }))
    })?
    }
}
