//! Desktop clients can address an approval by session, or by a unique legacy ID.
use super::{DirectoryFilter, Stores};
use anyhow::{ensure, Context, Result};

impl Stores {
    pub fn decide_approval(
        &self,
        conversation: Option<&str>,
        approval: &str,
        status: &str,
        reason: Option<&str>,
        decided_at: &str,
    ) -> Result<()> {
        let shard = if let Some(id) = conversation {
            self.open_session(id)?
        } else {
            let mut found = None;
            for row in self.global().list_sessions(DirectoryFilter::default())? {
                let shard = self.open_listed(&row)?;
                if shard.read(|r| Ok(r.approval(approval)?.is_some()))? {
                    ensure!(found.is_none(), "Ambiguous approval ID");
                    found = Some(shard);
                }
            }
            found.context("Approval not found")?
        };
        shard.write(|tx| {
            crate::db::permission_repo::decide_for_session(
                tx.0,
                Some(shard.id()),
                approval,
                status,
                reason,
                decided_at,
            )
        })
    }
}

impl crate::db::DbManager {
    pub fn decide_approval(
        &self,
        conversation: Option<&str>,
        approval: &str,
        status: &str,
        reason: Option<&str>,
        decided_at: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            crate::db::permission_repo::decide_for_session(
                c,
                conversation,
                approval,
                status,
                reason,
                decided_at,
            )
        })?
    }
    pub fn reject_diff(&self, conversation: Option<&str>, id: &str) -> Result<()> {
        self.with_conn(|c|{
            ensure!(c.execute("UPDATE code_diffs SET status='rejected',applied_at=NULL WHERE id=?1 AND (?2 IS NULL OR conversation_id=?2)",rusqlite::params![id,conversation])?==1,"Diff not found");
            Ok(())
        })?
    }
}
impl Stores {
    pub fn reject_diff(&self, conversation: Option<&str>, diff: &str) -> Result<()> {
        let shard = if let Some(id) = conversation {
            self.open_session(id)?
        } else {
            let mut owner = None;
            for entry in self.global().list_sessions(Default::default())? {
                let s = self.open_listed(&entry)?;
                if s.read(|r| {
                    Ok(r.0
                        .prepare_cached("SELECT 1 FROM code_diffs WHERE id=?1")?
                        .exists([diff])?)
                })? {
                    ensure!(owner.is_none(), "Ambiguous diff ID");
                    owner = Some(s);
                }
            }
            owner.context("Diff not found")?
        };
        shard.write(|w| {
            ensure!(
                w.0.execute(
                    "UPDATE code_diffs SET status='rejected',applied_at=NULL WHERE id=?1",
                    [diff]
                )? == 1,
                "Diff not found"
            );
            Ok(())
        })
    }
}
impl super::SessionRepository<'_> {
    pub fn pending_approvals(
        &self,
    ) -> Result<Vec<crate::db::permission_repo::PermissionApprovalRow>> {
        self.read(crate::db::permission_repo::get_pending_approvals)
    }
    pub fn request_approval(
        &self,
        id: &str,
        message: &str,
        tool: &str,
        action: &str,
        risk: &str,
        at: &str,
    ) -> Result<()> {
        self.write("request permission approval", |c, session| {
            crate::db::permission_repo::insert_permission_approval(
                c, id, session, message, tool, action, risk, "pending", at,
            )
        })
    }
    pub fn diffs(&self) -> Result<Vec<crate::db::diff_repo::CodeDiffRow>> {
        self.read(crate::db::diff_repo::get_diffs_by_conversation)
    }
}
