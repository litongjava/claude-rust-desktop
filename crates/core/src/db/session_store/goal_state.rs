use super::SessionRepository;
use crate::services::{
    permission_authority::{self, Source},
    session_goals::{self, Goal, State},
};
use anyhow::Result;

impl SessionRepository<'_> {
    pub(crate) fn active_goal(&self) -> Result<Option<Goal>> {
        self.read(session_goals::active_conn)
    }
    /// Human intent and goal revision change together. Evaluator updates never grant authority.
    pub(crate) fn update_goal<T>(
        &self,
        user_input: Option<&str>,
        mut change: impl FnMut(&mut State) -> Result<T>,
    ) -> Result<T> {
        self.write("update session goal", |c, id| {
            if let Some(input) = user_input {
                permission_authority::record_conn(c, id, Source::User, &serde_json::json!(input))?;
            }
            session_goals::modify_conn(c, id, |_, state| change(state))
        })
    }
}

impl SessionRepository<'_> {
    pub(crate) fn record_human_authority(
        &self,
        content: &serde_json::Value,
        if_absent: bool,
    ) -> Result<()> {
        self.write("record human authority", |c,id| {
            if if_absent && c.prepare_cached("SELECT 1 FROM session_permission_authority WHERE conversation_id=?1 AND source='user' LIMIT 1")?.exists([id])? {
                return Ok(());
            }
            permission_authority::record_conn(c,id,Source::User,content)
        })
    }
    pub(crate) fn authority_history(&self, limit: usize) -> Result<Vec<serde_json::Value>> {
        self.read(|c, id| permission_authority::history_conn(c, id, limit))
    }
}

impl SessionRepository<'_> {
    /// Read the proposal state and revision in a single snapshot.
    pub(crate) fn inspect_proposal<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T>,
    ) -> Result<T> {
        self.read(|c, _| f(c))
    }

    pub(crate) fn edit_proposal<T>(
        &self,
        deadline: std::time::Duration,
        mut change: impl FnMut(
            &rusqlite::Connection,
            &mut crate::services::goal_proposals::ProposalState,
        ) -> Result<T>,
    ) -> Result<T> {
        use super::owned::Backend;
        let mut apply = |c: &rusqlite::Connection, id: &str| {
            let mut state = crate::services::goal_proposals::read(c, id)?;
            let result = change(c, &mut state)?;
            crate::services::goal_proposals::save_state(c, id, &state)?;
            Ok(result)
        };
        match &self.0 {
            Backend::Admitted(s) => {
                let _locks = s.stores.key_lock(&s.key, false, false)?;
                s.write_locked_with_deadline("goal proposal", deadline, |w| apply(w.0, w.1))
            }
            Backend::Shared(db, id) => db.with_conn(|c| {
                crate::db::write_tx(c, "goal proposal", deadline, |tx| apply(tx, id))
            })?,
        }
    }

    /// Directory trust is checked at admission; the selected incarnation cannot acquire
    /// another session's consent. Shared-store transactions can check trust locally.
    pub(crate) fn proposal_project_trust(&self) -> Result<Option<bool>> {
        match &self.0 {
            super::owned::Backend::Admitted(_) => self.directory_snapshot(|c, id| {
                Ok(Some(
                    crate::native_engine::workspace::conversation_project(c, id).is_ok(),
                ))
            }),
            super::owned::Backend::Shared(..) => Ok(None),
        }
    }
}
