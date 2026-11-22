//! Shared service ownership. Idle polls read G and renew only near expiry.
use super::shared::DirectoryRepository;
use crate::services::lifecycle::ServiceStatus;
use anyhow::{ensure, Result};
use rusqlite::{params, OptionalExtension};
use std::time::Duration;

const TTL: i64 = 15;
const WAIT: Duration = Duration::from_millis(250);

pub trait ServiceLeases: Send + Sync {
    fn acquire_lease(&self, name: &str, owner: &str) -> Result<()>;
    fn renew_lease(&self, name: &str, owner: &str) -> Result<bool>;
    fn record_lease(&self, name: &str, owner: &str, error: Option<&str>) -> Result<()>;
    fn release_lease(&self, name: &str, owner: &str) -> Result<()>;
    fn service_status(&self) -> Result<Vec<ServiceStatus>>;
    fn stop_lease(&self, name: &str) -> Result<()>;
    fn lease_running(&self, name: &str) -> Result<bool>;
}

impl<D: DirectoryRepository + Send + Sync> ServiceLeases for D {
    fn acquire_lease(&self, name: &str, owner: &str) -> Result<()> {
        self.directory_write("claim service lease",name,WAIT,|tx|{
            let now = chrono::Utc::now().timestamp();
            let changed = tx.execute(
                "INSERT INTO service_leases(name,owner,pid,expires_at,state) VALUES(?1,?2,?3,?4,'running')
                ON CONFLICT(name) DO UPDATE SET owner=excluded.owner,pid=excluded.pid,expires_at=excluded.expires_at,
                stop_requested=0,state='running',error=NULL WHERE service_leases.expires_at<=?5",
                params![name,owner,std::process::id(),now+TTL,now])?;
            ensure!(changed==1, "Service {name} is already running in another host");
            Ok(())
        })
    }
    fn renew_lease(&self, name: &str, owner: &str) -> Result<bool> {
        let expiry = self.directory_read(|c| {
            Ok(c.query_row(
            "SELECT expires_at FROM service_leases WHERE name=?1 AND owner=?2 AND stop_requested=0",
            [name,owner], |r|r.get::<_,i64>(0)).optional()?)
        })?;
        let now = chrono::Utc::now().timestamp();
        let Some(expiry) = expiry.filter(|value| *value > now) else {
            return Ok(false);
        };
        if expiry > now + TTL / 2 {
            return Ok(true);
        }
        self.directory_write("renew service lease",name,WAIT,|tx|{
            let now=chrono::Utc::now().timestamp();
            Ok(tx.execute("UPDATE service_leases SET expires_at=?1 WHERE name=?2 AND owner=?3 AND stop_requested=0 AND expires_at>?4",
                params![now+TTL,name,owner,now])?==1)
        })
    }
    fn record_lease(&self, name: &str, owner: &str, error: Option<&str>) -> Result<()> {
        self.directory_write("record service result",name,WAIT,|tx| {
            tx.execute("UPDATE service_leases SET error=?1,completed=completed+?2 WHERE name=?3 AND owner=?4",params![error,i64::from(error.is_none()),name,owner])?;
            Ok(())
        })
    }
    fn release_lease(&self, name: &str, owner: &str) -> Result<()> {
        self.directory_write("release service lease",name,WAIT,|tx|{
            tx.execute("UPDATE service_leases SET expires_at=0,state='stopped',stop_requested=0 WHERE name=?1 AND owner=?2",[name,owner])?;
            Ok(())
        })
    }
    fn service_status(&self) -> Result<Vec<ServiceStatus>> {
        self.directory_read(|c| {
            let now=chrono::Utc::now().timestamp();
            let mut stmt=c.prepare_cached("SELECT name,pid,state,error,completed,expires_at,stop_requested FROM service_leases ORDER BY name")?;
            let rows=stmt.query_map([], |r| {
                let state:String=r.get(2)?;
                let expiry:i64=r.get(5)?;
                let stop:bool=r.get(6)?;
                Ok(ServiceStatus{name:r.get(0)?,pid:r.get(1)?, state:if expiry<=now && state=="running" {"expired".into()} else if stop {"stopping".into()} else {state}, error:r.get(3)?, completed:r.get(4)?})
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
    fn stop_lease(&self, name: &str) -> Result<()> {
        self.directory_write("request service stop",name,WAIT,|tx|{
            let changed=tx.execute("UPDATE service_leases SET stop_requested=1 WHERE name=?1 AND expires_at>?2 AND state='running'",params![name,chrono::Utc::now().timestamp()])?;
            ensure!(changed==1,"Service {name} is not running");
            Ok(())
        })
    }
    fn lease_running(&self, name: &str) -> Result<bool> {
        self.directory_read(|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM service_leases WHERE name=?1 AND expires_at>?2 AND state='running')",params![name,chrono::Utc::now().timestamp()],|r|r.get(0))?))
    }
}
