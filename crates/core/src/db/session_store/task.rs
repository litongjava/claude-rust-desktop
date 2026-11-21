//! Lock all selected member incarnations before reading any evidence or entering
//! the global decision transaction. Locks outlive both snapshots and G commit.
use super::{layout, SessionDb, Stores};
use crate::super_agent::store::{Decision, EvidenceFence, MemberEvidence, TaskDatabase};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, Transaction};
use std::collections::BTreeMap;

impl TaskDatabase for Stores {
    fn task_read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.global().read(f)
    }

    fn task_write<T>(
        &self,
        label: &str,
        mut f: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.global()
            .connection(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
                c.pragma_update(None, "synchronous", "NORMAL")?;
                crate::db::write_tx(c, label, crate::super_agent::store::WRITE_DEADLINE, |tx| {
                    f(tx)
                })
            })
    }

    fn fence_decision(&self, selection: Decision<'_>) -> Result<Option<EvidenceFence>> {
        let (predicate,value)=match selection {
            Decision::Task(id)=>("task_id=?1",id),
            Decision::Event(id)=>("task_id IN (SELECT task_id FROM sa_events WHERE event_id=?1 AND event_type='TASK_COMPLETED' AND processed=0)",id),
            Decision::Pending(router)=>("router_session_id=?1 AND task_id IN (SELECT task_id FROM sa_events WHERE event_type='TASK_COMPLETED' AND processed=0)",router),
        };
        let members = self.global().read(|c| {
            let mut q = c.prepare(&format!(
                "SELECT coder_session_id,reviewer_session_id FROM sa_tasks WHERE {predicate}"
            ))?;
            let mut cursor = q.query([value])?;
            let mut members = BTreeMap::new();
            while let Some(row) = cursor.next()? {
                for col in 0..2 {
                    if let Some(id) = row
                        .get::<_, Option<String>>(col)?
                        .filter(|s| !s.trim().is_empty())
                    {
                        members.insert(layout::storage_key(&id)?, id);
                    }
                }
            }
            Ok(members)
        })?;
        let mut fence = EvidenceFence::default();
        if !members.is_empty() {
            fence.locks.push(self.namespace()?);
        }
        for (key, id) in members {
            let Some(incarnation) = self.global().incarnation(&id, &key)? else {
                continue;
            };
            let path = self.root.path.join("session-locks").join(&key);
            let lock = std::fs::File::options()
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("Opening evidence lock for member {id}"))?;
            lock.try_lock()
                .with_context(|| format!("Super Agent member {id} is busy; retry the decision"))?;
            fence.locks.push(lock);
            let member = SessionDb {
                stores: self.clone(),
                key,
                id: id.clone(),
                incarnation: incarnation.clone(),
            };
            member.check_directory()?;
            member.upgrade_locked()?;
            let (revision,nonempty)=member.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY,|c| {
                Ok(c.query_row("SELECT content_revision,EXISTS(SELECT 1 FROM messages) FROM manifest WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?)))?)
            })?;
            fence.members.insert(
                id,
                MemberEvidence {
                    incarnation,
                    revision,
                    nonempty,
                },
            );
        }
        Ok(Some(fence))
    }
}
