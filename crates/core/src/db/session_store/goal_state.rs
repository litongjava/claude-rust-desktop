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
