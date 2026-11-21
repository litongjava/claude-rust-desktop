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
