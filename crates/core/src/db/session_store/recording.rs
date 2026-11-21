//! Request recording is opt-in per launch. Only a live execution parent can lend its policy.
use super::Stores;
use crate::{db::DbManager, native_engine::request_recording::Policy};
use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use std::{path::Path, sync::Arc};

pub trait RecordingPolicies {
    fn recording_root(&self) -> &Path;
    fn recording_policy_and_parent(&self, session: &str) -> (Option<Policy>, Option<String>);
}

impl Stores {
    pub fn set_request_recording_policy(
        &self,
        session: &str,
        cli: bool,
        full_access: bool,
    ) -> Result<()> {
        let incarnation = self
            .recording_incarnation(session)?
            .context("Recording session unavailable")?;
        self.root
            .recording
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(incarnation, Policy { cli, full_access });
        Ok(())
    }

    fn recording_incarnation(&self, session: &str) -> Result<Option<String>> {
        self.global().read_directory_row(session, |c| {
            Ok(c.query_row(
                "SELECT incarnation FROM conversations WHERE id=?1",
                [session],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
    }

    pub fn request_recording_policy(&self, session: &str) -> Option<Policy> {
        let incarnation = self.recording_incarnation(session).ok()??;
        self.root
            .recording
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&incarnation)
            .cloned()
    }
}

impl RecordingPolicies for Stores {
    fn recording_root(&self) -> &Path {
        self.data_dir()
    }
    fn recording_policy_and_parent(&self, session: &str) -> (Option<Policy>, Option<String>) {
        // Resolve the complete lineage in a single G snapshot. A deleted or replaced
        // parent must never authorize recording for a child from the old incarnation.
        let lineage=self.global().read_directory_row(session, |c|{
            let mut q=c.prepare_cached("WITH RECURSIVE lineage(id,inc,depth,path) AS (
                SELECT id,incarnation,0,'/'||incarnation||'/' FROM conversations WHERE id=?1
                UNION ALL SELECT p.id,p.incarnation,l.depth+1,l.path||p.incarnation||'/' FROM lineage l
                JOIN session_execution_parents e ON e.child_id=l.id
                JOIN conversations p ON p.id=e.parent_id AND p.incarnation=e.parent_incarnation
                WHERE instr(l.path,'/'||p.incarnation||'/')=0)
                SELECT id,inc FROM lineage ORDER BY depth")?;
            let rows=q.query_map([session], |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?;
            Ok(Some(rows.collect::<rusqlite::Result<Vec<_>>>()?))
        }).ok().flatten().unwrap_or_default();
        let policies = self
            .root
            .recording
            .read()
            .unwrap_or_else(|e| e.into_inner());
        (
            lineage
                .iter()
                .find_map(|(_, inc)| policies.get(inc))
                .cloned(),
            lineage.get(1).map(|(id, _)| id.clone()),
        )
    }
}

impl RecordingPolicies for DbManager {
    fn recording_root(&self) -> &Path {
        self.data_dir()
    }
    fn recording_policy_and_parent(&self, session: &str) -> (Option<Policy>, Option<String>) {
        let parent = |id: &str| {
            self.with_conn(|c| {
                c.query_row(
                    "SELECT value FROM peripheral_settings WHERE key=?1",
                    [format!("delegation.{id}")],
                    |r| r.get::<_, String>(0),
                )
                .optional()
            })
            .ok()
            .and_then(Result::ok)
            .flatten()
            .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
            .and_then(|v| v["parent_session_id"].as_str().map(str::to_owned))
            .or_else(|| {
                crate::services::background_sessions::worker_directory(self.data_dir(), id)
                    .ok()
                    .flatten()
                    .and_then(|dir| crate::services::background_sessions::read_job(&dir).ok())
                    .map(|job| job.parent_session_id)
            })
            .filter(|p| p != id)
        };
        let first = parent(session);
        let mut policy = self.request_recording_policy(session);
        let mut ancestor = first.clone();
        let mut visited = std::collections::HashSet::from([session.to_owned()]);
        while policy.is_none() {
            let Some(id) = ancestor else { break };
            if !visited.insert(id.clone()) {
                break;
            }
            policy = self.request_recording_policy(&id);
            ancestor = parent(&id);
        }
        (policy, first)
    }
}
impl RecordingPolicies for super::ExecutorHistory {
    fn recording_root(&self) -> &Path {
        match self {
            Self::Session(s) => s.stores.data_dir(),
            Self::Shared(db, _) => db.data_dir(),
        }
    }
    fn recording_policy_and_parent(&self, id: &str) -> (Option<Policy>, Option<String>) {
        if self.id()!=id { return (None,None); }
        match self {
            Self::Session(s) => {
                let Ok(_locks)=s.stores.key_lock(&s.key,false,false) else {return (None,None)};
                if s.check_directory().is_err() {return (None,None)}
                s.stores.recording_policy_and_parent(id)
            }
            Self::Shared(db, _) => db.recording_policy_and_parent(id),
        }
    }
}
impl<D: RecordingPolicies> RecordingPolicies for Arc<D> {
    fn recording_root(&self) -> &Path {
        self.as_ref().recording_root()
    }
    fn recording_policy_and_parent(&self, session: &str) -> (Option<Policy>, Option<String>) {
        self.as_ref().recording_policy_and_parent(session)
    }
}
