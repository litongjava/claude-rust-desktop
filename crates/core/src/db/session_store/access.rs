//! Shared access tokens, rule catalog, and host health probes.
use super::{shared::DirectoryRepository, WRITE_DEADLINE};
use crate::db::{h5_repo as tokens, permission_repo as permissions};
use anyhow::Result;

pub trait HostAccess {
    fn directory_healthy(&self) -> bool;
    fn always_allowed(&self, tool: &str, action: &str) -> bool;
    fn insert_always_allow_rule(
        &self,
        id: &str,
        pattern: &str,
        kind: &str,
        created: &str,
        updated: &str,
    ) -> Result<()>;
    fn get_always_allow_rules(&self) -> Result<Vec<permissions::AlwaysAllowRuleRow>>;
    fn delete_always_allow_rule(&self, id: &str) -> Result<()>;
    fn insert_h5_token(
        &self,
        id: &str,
        token: &str,
        conversation: &str,
        expiry: &str,
        created: &str,
    ) -> Result<()>;
    fn list_h5_tokens_by_conversation(&self, conversation: &str)
        -> Result<Vec<tokens::H5TokenRow>>;
    fn revoke_h5_token(&self, id: &str) -> Result<()>;
    fn validate_h5_token(&self, token: &str) -> Result<Option<tokens::H5TokenRow>>;
    fn cleanup_expired_tokens(&self) -> Result<()>;
}
impl<D: DirectoryRepository> HostAccess for D {
    fn directory_healthy(&self) -> bool {
        self.directory_read(|c| Ok(c.query_row("SELECT 1", [], |_| Ok(()))?))
            .is_ok()
    }
    fn always_allowed(&self, tool: &str, action: &str) -> bool {
        self.directory_read(|c| Ok(permissions::check_always_allowed(c, tool, action)))
            .unwrap_or(false)
    }
    fn insert_always_allow_rule(
        &self,
        id: &str,
        pattern: &str,
        kind: &str,
        created: &str,
        updated: &str,
    ) -> Result<()> {
        self.directory_write("create permission rule", id, WRITE_DEADLINE, |tx| {
            permissions::insert_always_allow_rule(tx, id, pattern, kind, created, updated)
        })
    }
    fn get_always_allow_rules(&self) -> Result<Vec<permissions::AlwaysAllowRuleRow>> {
        self.directory_read(permissions::get_always_allow_rules)
    }
    fn delete_always_allow_rule(&self, id: &str) -> Result<()> {
        self.directory_write("delete permission rule", id, WRITE_DEADLINE, |tx| {
            permissions::delete_always_allow_rule(tx, id)
        })
    }
    fn insert_h5_token(
        &self,
        id: &str,
        token: &str,
        conversation: &str,
        expiry: &str,
        created: &str,
    ) -> Result<()> {
        self.directory_write("issue H5 token", conversation, WRITE_DEADLINE, |tx| {
            tokens::insert_h5_token(tx, id, token, conversation, expiry, created)
        })
    }
    fn list_h5_tokens_by_conversation(
        &self,
        conversation: &str,
    ) -> Result<Vec<tokens::H5TokenRow>> {
        self.directory_read(|c| tokens::list_h5_tokens_by_conversation(c, conversation))
    }
    fn revoke_h5_token(&self, id: &str) -> Result<()> {
        self.directory_write("revoke H5 token", id, WRITE_DEADLINE, |tx| {
            tokens::revoke_h5_token(tx, id)
        })
    }
    fn validate_h5_token(&self, token: &str) -> Result<Option<tokens::H5TokenRow>> {
        self.directory_write("use H5 token", "H5", WRITE_DEADLINE, |tx| {
            tokens::validate_h5_token(tx, token)
        })
    }
    fn cleanup_expired_tokens(&self) -> Result<()> {
        self.directory_write("expire H5 tokens", "H5", WRITE_DEADLINE, |tx| {
            tokens::cleanup_expired_tokens(tx)
        })
    }
}

/// An accepted H5 request remains bound to its recipient until its reader or turn finishes.
pub struct H5Session {
    pub conversation: crate::db::conversation_repo::ConversationRow,
    history: super::ExecutorHistory,
    _guards: Vec<std::fs::File>,
}
impl H5Session {
    pub fn messages(&self) -> Result<Vec<crate::db::message_repo::MessageRow>> {
        self.history
            .read(crate::db::message_repo::get_messages_by_conversation)
    }
}
impl super::Stores {
    pub fn authorize_h5(&self, token: &str) -> Result<Option<H5Session>> {
        let entry = self
            .global()
            .write("authorize H5 session", "H5", false, |tx| {
                let Some(grant) = tokens::validate_h5_token(tx, token)? else {
                    return Ok(None);
                };
                super::directory::one(tx, &grant.conversation_id)
            })?;
        let Some(entry) = entry else { return Ok(None) };
        let shard = self.open_listed(&entry)?;
        let guards = self.key_lock(&shard.key, false, false)?;
        shard.check_directory()?;
        Ok(Some(H5Session {
            history: super::ExecutorHistory::Session(shard),
            conversation: entry.conversation,
            _guards: guards,
        }))
    }
}
impl crate::db::DbManager {
    pub fn authorize_h5(self: &std::sync::Arc<Self>, token: &str) -> Result<Option<H5Session>> {
        let row = self.directory_write("authorize H5 session", "H5", WRITE_DEADLINE, |tx| {
            let Some(grant) = tokens::validate_h5_token(tx, token)? else {
                return Ok(None);
            };
            crate::db::conversation_repo::get_conversation(tx, &grant.conversation_id)
        })?;
        row.map(|conversation| {
            Ok(H5Session {
                history: self.executor_history(&conversation.id)?,
                conversation,
                _guards: Vec::new(),
            })
        })
        .transpose()
    }
}
