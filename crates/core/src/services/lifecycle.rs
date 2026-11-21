//! SQLite leases are shared across processes; network work never holds a DB lock.
use super::ServiceContext;
use crate::db::session_store::ServiceLeases;
use anyhow::{ensure, Result};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub pid: u32,
    pub state: String,
    pub error: Option<String>,
    pub completed: u64,
}

pub struct Lease {
    db: Arc<dyn ServiceLeases>,
    pub name: String,
    owner: String,
}
impl Lease {
    pub fn acquire<D: ServiceLeases + 'static>(
        context: &ServiceContext<D>,
        name: &str,
    ) -> Result<Self> {
        let owner = uuid::Uuid::new_v4().to_string();
        context.db.acquire_lease(name, &owner)?;
        Ok(Self {
            db: context.db.clone(),
            name: name.into(),
            owner,
        })
    }
    pub fn renew(&self) -> Result<bool> {
        self.db.renew_lease(&self.name, &self.owner)
    }
    pub fn record(&self, error: Option<&str>) -> Result<()> {
        self.db.record_lease(&self.name, &self.owner, error)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Err(err) = self.db.release_lease(&self.name, &self.owner) {
            tracing::warn!("Could not release service lease {}: {err}", self.name);
        }
    }
}
pub fn status(context: &ServiceContext<impl ServiceLeases>) -> Result<Vec<ServiceStatus>> {
    context.db.service_status()
}
pub fn stop(context: &ServiceContext<impl ServiceLeases>, name: &str) -> Result<()> {
    context.db.stop_lease(name)
}
pub async fn wait_stopped(context: &ServiceContext<impl ServiceLeases>, name: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let running = context.db.lease_running(name)?;
        if !running {
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Timed out waiting for {name} to stop"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A foreground host. Dropping this future cancels the in-flight HTTP request and releases its lease.
pub async fn run_team_sync(context: ServiceContext, interval_seconds: u64) -> Result<()> {
    ensure!(
        (1..=86400).contains(&interval_seconds),
        "Interval must be 1..86400 seconds"
    );
    let config = super::team::configured(&context)?;
    context.credentials.get(&config.credential)?;
    let lease = Lease::acquire(&context, "team-sync")?;
    run_team_sync_with_lease(context, interval_seconds, lease).await
}
pub async fn run_team_sync_with_lease(
    context: ServiceContext,
    interval_seconds: u64,
    lease: Lease,
) -> Result<()> {
    let mut heartbeat = tokio::time::interval(Duration::from_millis(250));
    loop {
        let operation = super::team::sync(&context);
        tokio::pin!(operation);
        let result = loop {
            tokio::select! {
                result = &mut operation => break result,
                _ = heartbeat.tick() => if !lease.renew()? { return Ok(()); },
            }
        };
        lease.record(result.as_ref().err().map(|e| e.to_string()).as_deref())?;
        let delay = tokio::time::sleep(Duration::from_secs(interval_seconds));
        tokio::pin!(delay);
        loop {
            tokio::select! {
                _ = &mut delay => break,
                _ = heartbeat.tick() => if !lease.renew()? { return Ok(()); },
            }
        }
    }
}
