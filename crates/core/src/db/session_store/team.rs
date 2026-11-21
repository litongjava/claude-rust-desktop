//! Team mutations compare the transmitted mutation and revision before clearing dirty state.
use super::{shared::CatalogBodies, GlobalPreferences, Stores, WRITE_DEADLINE};
use crate::{
    db::DbManager,
    services::team::{
        validate_memory, Conflict, ImportReport, Mutation, SyncRequest, SyncResponse, TeamConfig,
        TeamMemory,
    },
};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub trait TeamCatalog: GlobalPreferences {
    fn bind_team(&self, config: &TeamConfig) -> Result<()>;
    fn memories(&self, team: &str, deleted: bool) -> Result<Vec<TeamMemory>>;
    fn put_memory(&self, team: &str, memory: &TeamMemory, existing: bool) -> Result<i64>;
    fn delete_memory(&self, team: &str, id: &str) -> Result<()>;
    fn conflicts(&self, team: &str) -> Result<Vec<Conflict>>;
    fn resolve_conflict(&self, team: &str, id: &str, keep_local: bool) -> Result<()>;
    fn team_request(&self, team: &str) -> Result<SyncRequest>;
    fn team_response(&self, team: &str, request_cursor: i64, response: &SyncResponse)
        -> Result<()>;
    fn import_memories(
        &self,
        root: &Path,
        team: &str,
        entries: &[(TeamMemory, String, String)],
        preview: bool,
    ) -> Result<ImportReport>;
}
trait TeamConfiguration: CatalogBodies + GlobalPreferences {
    fn bind_config(c: &Connection, encoded: &str) -> Result<()>;
}
impl TeamConfiguration for DbManager {
    fn bind_config(c: &Connection, encoded: &str) -> Result<()> {
        super::state::set_local(c, "team-sync", encoded)
    }
}
impl TeamConfiguration for Stores {
    fn bind_config(c: &Connection, encoded: &str) -> Result<()> {
        c.execute("INSERT INTO global_settings VALUES('team','team-sync',?1) ON CONFLICT(domain,name) DO UPDATE SET value=excluded.value",[encoded])?;
        Ok(())
    }
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TeamMemory> {
    let tags: String = r.get(3)?;
    Ok(TeamMemory {
        id: r.get(0)?,
        title: r.get(1)?,
        content: r.get(2)?,
        tags: serde_json::from_str(&tags).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
        })?,
        deleted: r.get(4)?,
        revision: r.get(5)?,
    })
}
fn decode(db: &impl CatalogBodies, mut memory: TeamMemory) -> Result<TeamMemory> {
    memory.content = db.load_body(&memory.content)?;
    Ok(memory)
}
fn apply(c: &Connection, team: &str, m: &TeamMemory, body: &str) -> Result<()> {
    c.execute("INSERT INTO team_memories(team,id,title,content,tags,deleted,revision,dirty,mutation) VALUES(?1,?2,?3,?4,?5,?6,?7,0,'')
        ON CONFLICT(team,id) DO UPDATE SET title=excluded.title,content=excluded.content,tags=excluded.tags,deleted=excluded.deleted,revision=excluded.revision,dirty=0,mutation=''",
        params![team,m.id,m.title,body,serde_json::to_string(&m.tags)?,m.deleted,m.revision])?;
    Ok(())
}
fn conflict(c: &Connection, team: &str, id: &str, reference: &str) -> Result<()> {
    c.execute("INSERT INTO team_conflicts VALUES(?1,?2,?3) ON CONFLICT(team,id) DO UPDATE SET remote=excluded.remote",[team,id,reference])?;
    Ok(())
}
impl<D: TeamConfiguration> TeamCatalog for D {
    fn bind_team(&self, config: &TeamConfig) -> Result<()> {
        let raw = serde_json::to_string(config)?;
        self.directory_write("bind team endpoint", &config.team, WRITE_DEADLINE, |c| {
            let endpoint = config.endpoint.trim_end_matches('/');
            c.execute(
                "INSERT OR IGNORE INTO team_bindings VALUES(?1,?2)",
                params![config.team, endpoint],
            )?;
            ensure!(
                c.prepare_cached("SELECT 1 FROM team_bindings WHERE team=?1 AND endpoint=?2")?
                    .exists(params![config.team, endpoint])?,
                "This team is bound to another server; use an isolated --data-dir"
            );
            Self::bind_config(c, &raw)
        })
    }
    fn memories(&self, team: &str, deleted: bool) -> Result<Vec<TeamMemory>> {
        let rows=self.directory_read(|c|{
            let mut q=c.prepare_cached("SELECT id,title,content,tags,deleted,revision FROM team_memories WHERE team=?1 AND (?2 OR deleted=0) ORDER BY id")?;
            let rows=q.query_map(params![team,deleted],row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter().map(|m| decode(self, m)).collect()
    }
    fn put_memory(&self, team: &str, m: &TeamMemory, existing: bool) -> Result<i64> {
        validate_memory(m)?;
        let content = self.store_body(&m.content)?;
        self.directory_write("save team memory",&m.id,WRITE_DEADLINE,|tx|{
            let rev:Option<i64>=tx.query_row("SELECT revision FROM team_memories WHERE team=?1 AND id=?2",params![team,m.id],|r|r.get(0)).optional()?;
            ensure!(!existing || rev.is_some(),"Team memory does not exist");
            let rev=rev.unwrap_or(0);
            tx.execute("INSERT INTO team_memories(team,id,title,content,tags,deleted,revision,dirty,mutation) VALUES(?1,?2,?3,?4,?5,0,?6,1,?7)
                ON CONFLICT(team,id) DO UPDATE SET title=excluded.title,content=excluded.content,tags=excluded.tags,deleted=0,dirty=1,mutation=excluded.mutation",
                params![team,m.id,m.title,content,serde_json::to_string(&m.tags)?,rev,uuid::Uuid::new_v4().to_string()])?;
            Ok(rev)
        })
    }
    fn delete_memory(&self, team: &str, id: &str) -> Result<()> {
        self.directory_write("delete team memory",id,WRITE_DEADLINE,|c|{
            ensure!(c.execute("UPDATE team_memories SET deleted=1,dirty=1,mutation=?3 WHERE team=?1 AND id=?2 AND deleted=0",params![team,id,uuid::Uuid::new_v4().to_string()])?==1,"Team memory does not exist or is already deleted");Ok(())
        })
    }
    fn conflicts(&self, team: &str) -> Result<Vec<Conflict>> {
        let rows=self.directory_read(|c|{
            let mut q=c.prepare("SELECT m.id,m.title,m.content,m.tags,m.deleted,m.revision,x.remote FROM team_conflicts x JOIN team_memories m USING(team,id) WHERE x.team=?1 ORDER BY m.id")?;
            let rows=q.query_map([team],|r|Ok((row(r)?,r.get::<_,String>(6)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter()
            .map(|(memory, remote)| {
                Ok(Conflict {
                    local: decode(self, memory)?,
                    remote: serde_json::from_str(&self.load_body(&remote)?)?,
                })
            })
            .collect()
    }
    fn resolve_conflict(&self, team: &str, id: &str, keep_local: bool) -> Result<()> {
        let reference: String = self.directory_read(|c| {
            Ok(c.query_row(
                "SELECT remote FROM team_conflicts WHERE team=?1 AND id=?2",
                [team, id],
                |r| r.get(0),
            )
            .optional()?
            .context("Conflict does not exist")?)
        })?;
        let remote: TeamMemory = serde_json::from_str(&self.load_body(&reference)?)?;
        validate_memory(&remote)?;
        let content = if keep_local {
            String::new()
        } else {
            self.store_body(&remote.content)?
        };
        self.directory_write("resolve team conflict",id,WRITE_DEADLINE,|tx|{
            ensure!(tx.execute("DELETE FROM team_conflicts WHERE team=?1 AND id=?2 AND remote=?3",[team,id,&reference])?==1,"Team conflict changed; refresh the conflict");
            if keep_local {
                tx.execute("UPDATE team_memories SET revision=?3,dirty=1,mutation=?4 WHERE team=?1 AND id=?2",params![team,id,remote.revision,uuid::Uuid::new_v4().to_string()])?;
                Ok(())
            } else {apply(tx,team,&remote,&content)}
        })
    }
    fn team_request(&self, team: &str) -> Result<SyncRequest> {
        let (cursor,changes)=self.directory_read(|c|{
            let tx=c.unchecked_transaction()?;
            let cursor=tx.query_row("SELECT cursor FROM team_cursors WHERE team=?1",[team],|r|r.get(0)).optional()?.unwrap_or(0);
            let mut q=tx.prepare("SELECT m.id,m.title,m.content,m.tags,m.deleted,m.revision,m.mutation FROM team_memories m
                 WHERE m.team=?1 AND dirty=1 AND NOT EXISTS(SELECT 1 FROM team_conflicts x WHERE x.team=m.team AND x.id=m.id) ORDER BY m.id")?;
            let rows=q.query_map([team],|r|Ok((row(r)?,r.get::<_,String>(6)?)))?;
            Ok((cursor,rows.collect::<rusqlite::Result<Vec<_>>>()?))
        })?;
        let changes = changes
            .into_iter()
            .map(|(m, mutation)| {
                Ok(Mutation {
                    memory: decode(self, m)?,
                    mutation,
                })
            })
            .collect::<Result<_>>()?;
        Ok(SyncRequest { cursor, changes })
    }
    fn team_response(
        &self,
        team: &str,
        request_cursor: i64,
        response: &SyncResponse,
    ) -> Result<()> {
        ensure!(
            response.cursor >= request_cursor,
            "Server cursor moved backwards; server history was reset"
        );
        let mut staged = Vec::new();
        for m in &response.changes {
            validate_memory(m)?;
            staged.push((
                m,
                self.store_body(&m.content)?,
                self.store_body(&serde_json::to_string(m)?)?,
            ));
        }
        let mut conflicts = Vec::new();
        for m in &response.conflicts {
            validate_memory(m)?;
            conflicts.push((&m.id, self.store_body(&serde_json::to_string(m)?)?));
        }
        for accepted in &response.accepted {
            validate_memory(&accepted.memory)?;
        }
        self.directory_write("apply team synchronization",team,WRITE_DEADLINE,|tx|{
            let cursor:i64=tx.query_row("SELECT cursor FROM team_cursors WHERE team=?1",[team],|r|r.get(0)).optional()?.unwrap_or(0);
            ensure!(cursor==request_cursor,"Team sync cursor changed");
            for a in &response.accepted{
                tx.execute("UPDATE team_memories SET revision=?3,dirty=CASE WHEN mutation=?4 THEN 0 ELSE dirty END WHERE team=?1 AND id=?2",params![team,a.memory.id,a.memory.revision,a.mutation])?;
            }
            for (m,body,remote) in &staged {
                let local:Option<(bool,i64)>=tx.query_row("SELECT dirty,revision FROM team_memories WHERE team=?1 AND id=?2",params![team,m.id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                match local {
                    Some((true,revision)) if m.revision>revision=>conflict(tx,team,&m.id,remote)?,
                    Some((true,_))=>{},
                    Some((false,revision)) if m.revision<=revision=>{},
                    _=>apply(tx,team,m,body)?,
                }
            }
            for (id,reference) in &conflicts{conflict(tx,team,id,reference)?;}
            tx.execute("INSERT INTO team_cursors VALUES(?1,?2) ON CONFLICT(team) DO UPDATE SET cursor=excluded.cursor",params![team,response.cursor])?;
            Ok(())
        })
    }
    fn import_memories(
        &self,
        root: &Path,
        team: &str,
        entries: &[(TeamMemory, String, String)],
        preview: bool,
    ) -> Result<ImportReport> {
        let backup = if preview {
            None
        } else {
            Some(self.catalog_backup(root,"before-team-import",&["team_memories","team_import_sources","team_cursors","team_conflicts","team_bindings"],
        "SELECT content FROM team_memories UNION SELECT payload FROM team_import_sources UNION SELECT remote FROM team_conflicts ORDER BY 1")?.to_string_lossy().into_owned())
        };
        let mut prepared = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (m, path, source) in entries {
            validate_memory(m)?;
            ensure!(seen.insert(&m.id), "Duplicate IDs in legacy source");
            let body = if preview {
                self.body_reference(&m.content)?
            } else {
                self.store_body(&m.content)?
            };
            let source = if preview {
                None
            } else {
                Some(self.store_body(source)?)
            };
            prepared.push((m, path, body, source));
        }
        self.directory_write("import team memories",team,WRITE_DEADLINE,|tx|{
            let mut report=ImportReport{team:team.into(),imported:0,skipped:0,preview,backup:backup.clone()};
            for (m,path,body,source) in &prepared{
                let existing=tx.query_row("SELECT id,title,content,tags,deleted,revision FROM team_memories WHERE team=?1 AND id=?2",params![team,m.id],row).optional()?;
                if let Some(existing)=existing {ensure!(existing.title==m.title && existing.content==*body && existing.tags==m.tags && !existing.deleted,"Legacy ID {} conflicts with current team data; no data was imported",m.id);report.skipped+=1;continue;}
                report.imported+=1;
                if let Some(source)=source {
                    tx.execute("INSERT INTO team_memories(team,id,title,content,tags,deleted,revision,dirty,mutation) VALUES(?1,?2,?3,?4,?5,0,0,1,?6)",params![team,m.id,m.title,body,serde_json::to_string(&m.tags)?,uuid::Uuid::new_v4().to_string()])?;
                    tx.execute("INSERT INTO team_import_sources VALUES(?1,?2,?3,?4)",params![team,m.id,path,source])?;
                }
            }
            Ok(report)
        })
    }
}
